mod addr;
mod event;
mod keypair;

use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use crate::{announce, codec, proxy, stream};
use anyhow::{Context, Result};

/// Read the local keypair from disk and return the derived `PeerId`.
///
/// Uses the same key file that `run()` loads (`~/.clawshake/identity.key` or
/// the override path).  Does **not** generate a new key if none exists.
pub fn peer_id_from_disk(override_path: Option<&std::path::Path>) -> Result<PeerId> {
    let path = match override_path {
        Some(p) => p.to_path_buf(),
        None => {
            let home = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
            home.join(".clawshake").join("identity.key")
        }
    };
    anyhow::ensure!(
        path.exists(),
        "Identity key not found at {}",
        path.display()
    );
    let bytes =
        std::fs::read(&path).with_context(|| format!("reading keypair from {}", path.display()));
    let kp = libp2p::identity::Keypair::from_protobuf_encoding(&bytes?)
        .map_err(|e| anyhow::anyhow!("decoding keypair: {e}"))?;
    Ok(PeerId::from(&kp.public()))
}
use clawshake_core::{
    config::AdvertiseModels,
    mcp_client::McpClient,
    models::ModelAnnounce,
    network_channel::{
        ConnectedPeers, DhtLookup, OutboundCall, OutboundModelStreamingCall, OutboundStreamCall,
    },
    peer_table::PeerTable,
    permissions::PermissionStore,
};
use clawshake_models::backend::ModelBackend;
use libp2p::futures::StreamExt;
use libp2p::{
    autonat, dcutr, identify, kad, mdns, noise, relay, rendezvous, request_response,
    swarm::{behaviour::toggle::Toggle, NetworkBehaviour, SwarmEvent},
    tcp, upnp, yamux, Multiaddr, PeerId, StreamProtocol, SwarmBuilder,
};
use tokio::{
    select,
    sync::{mpsc, oneshot},
    time::interval,
};
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Composite behaviour
// ---------------------------------------------------------------------------

#[derive(NetworkBehaviour)]
struct ClawshakeBehaviour {
    kademlia: kad::Behaviour<kad::store::MemoryStore>,
    identify: identify::Behaviour,
    mdns: mdns::tokio::Behaviour,
    proxy: proxy::Behaviour,
    stream: stream::Behaviour,
    model_stream: libp2p_stream::Behaviour,
    relay_server: Toggle<relay::Behaviour>,
    relay_client: relay::client::Behaviour,
    autonat: autonat::Behaviour,
    dcutr: dcutr::Behaviour,
    rendezvous_client: rendezvous::client::Behaviour,
    rendezvous_server: Toggle<rendezvous::server::Behaviour>,
    upnp: Toggle<upnp::tokio::Behaviour>,
}

/// Default port used by relay/bootstrap nodes (stable so the address is predictable).
pub const RELAY_DEFAULT_PORT: u16 = 7474;

/// All state needed to start the P2P node.
pub struct P2pConfig {
    pub port: u16,
    pub boot_peers: Vec<String>,
    pub identity: Option<std::path::PathBuf>,
    pub backend: Option<McpClient>,
    pub permissions: Arc<PermissionStore>,
    pub peer_table: Arc<PeerTable>,
    pub connected: ConnectedPeers,
    pub relay_server: bool,
    pub call_rx: mpsc::Receiver<OutboundCall>,
    pub reannounce_rx: Option<mpsc::Receiver<()>>,
    pub dht_lookup_rx: mpsc::Receiver<DhtLookup>,
    pub model_backend: Option<ModelBackend>,
    pub stream_call_rx: Option<mpsc::Receiver<OutboundStreamCall>>,
    pub model_streaming_rx: Option<mpsc::Receiver<OutboundModelStreamingCall>>,
}

// ---------------------------------------------------------------------------
// Node entry point
// ---------------------------------------------------------------------------

pub async fn run(cfg: P2pConfig) -> Result<()> {
    let P2pConfig {
        port: p2p_port,
        boot_peers,
        identity,
        backend,
        permissions: store,
        peer_table: table,
        connected,
        relay_server,
        mut call_rx,
        reannounce_rx,
        mut dht_lookup_rx,
        model_backend,
        mut stream_call_rx,
        mut model_streaming_rx,
    } = cfg;

    let keypair = keypair::load_or_create_keypair(identity.as_deref())?;
    let local_peer_id = PeerId::from(&keypair.public());
    info!("Local peer ID: {local_peer_id}");

    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_quic()
        .with_dns()?
        .with_relay_client(noise::Config::new, yamux::Config::default)?
        .with_behaviour(|key, relay_client| {
            let peer_id = key.public().to_peer_id();

            let kad_protocol = StreamProtocol::try_from_owned("/clawshake/kad/1.0.0".to_string())
                .expect("valid protocol string");
            let mut kad_config = kad::Config::new(kad_protocol);
            kad_config.set_query_timeout(Duration::from_secs(30));
            kad_config.set_periodic_bootstrap_interval(Some(Duration::from_secs(60)));
            let kademlia = kad::Behaviour::with_config(
                peer_id,
                kad::store::MemoryStore::new(peer_id),
                kad_config,
            );

            let identify = identify::Behaviour::new(
                identify::Config::new("/clawshake/1.0.0".to_string(), key.public())
                    .with_agent_version(format!("clawshake/{}", env!("CARGO_PKG_VERSION"))),
            );

            let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id)?;

            let proxy = proxy::new_behaviour();

            let stream_proto = stream::new_behaviour();

            let model_stream = libp2p_stream::Behaviour::new();

            // Only enable relay hop if the --relay-server flag was set.
            // Using Toggle so non-relay nodes don't advertise the
            // relay-hop protocol and peers won't waste time trying
            // to reserve slots on them.
            let is_relay = relay_server; // capture before shadowing
            let relay_server = if is_relay {
                Toggle::from(Some(relay::Behaviour::new(
                    peer_id,
                    relay::Config::default(),
                )))
            } else {
                Toggle::from(None)
            };
            let autonat = autonat::Behaviour::new(peer_id, autonat::Config::default());
            let dcutr = dcutr::Behaviour::new(peer_id);
            let rendezvous_client = rendezvous::client::Behaviour::new(key.clone());
            // Only run a rendezvous server on relay nodes — regular nodes
            // should not accept registrations from other peers.
            let rendezvous_server = if is_relay {
                Toggle::from(Some(rendezvous::server::Behaviour::new(
                    rendezvous::server::Config::default(),
                )))
            } else {
                Toggle::from(None)
            };

            // UPnP: request port mappings from the gateway so peers
            // behind NAT become directly reachable without manual port
            // forwarding.  Skip on relay servers (already public).
            let upnp = if is_relay {
                Toggle::from(None)
            } else {
                Toggle::from(Some(upnp::tokio::Behaviour::default()))
            };

            Ok(ClawshakeBehaviour {
                kademlia,
                identify,
                mdns,
                proxy,
                stream: stream_proto,
                model_stream,
                relay_server,
                relay_client,
                autonat,
                dcutr,
                rendezvous_client,
                rendezvous_server,
                upnp,
            })
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(120)))
        .build();

    // IPv4 listeners
    let tcp4_addr: Multiaddr = format!("/ip4/0.0.0.0/tcp/{p2p_port}").parse()?;
    swarm.listen_on(tcp4_addr)?;
    let quic4_addr: Multiaddr = format!("/ip4/0.0.0.0/udp/{p2p_port}/quic-v1").parse()?;
    swarm.listen_on(quic4_addr)?;

    // IPv6 listeners — if the OS has no v6 support these will silently fail
    // to bind; we log but don't treat it as fatal.
    let tcp6_addr: Multiaddr = format!("/ip6/::/tcp/{p2p_port}").parse()?;
    if let Err(e) = swarm.listen_on(tcp6_addr) {
        info!("IPv6 TCP listen failed (no v6 support?): {e}");
    }
    let quic6_addr: Multiaddr = format!("/ip6/::/udp/{p2p_port}/quic-v1").parse()?;
    if let Err(e) = swarm.listen_on(quic6_addr) {
        info!("IPv6 QUIC listen failed (no v6 support?): {e}");
    }

    // Relay servers are publicly reachable — start Kademlia in Server mode.
    // Regular nodes start as Client; AutoNAT will upgrade to Server once
    // public reachability is confirmed (see NatStatus::Public handler).
    if relay_server {
        swarm
            .behaviour_mut()
            .kademlia
            .set_mode(Some(kad::Mode::Server));
    }

    // Bootstrap peers come from two sources (both explicit, user-controlled):
    //   1. `~/.clawshake/config.toml` → [network] bootstrap = [...]
    //   2. CLI `--boot <MULTIADDR>` flags
    // When neither is set the node operates in local-only mode (mDNS only).
    let all_boot_peers: Vec<String> = {
        let cfg = clawshake_core::config::load(None).unwrap_or_default();
        let mut peers = cfg.network.bootstrap;
        peers.extend(boot_peers);
        peers
    };

    if all_boot_peers.is_empty() {
        info!("No bootstrap peers configured — running in local-only mode (mDNS)");
    }

    // Dial bootstrap peers.
    for addr_str in &all_boot_peers {
        match addr_str.parse::<Multiaddr>() {
            Ok(addr) => {
                if let Some(libp2p::multiaddr::Protocol::P2p(peer_id)) = addr.iter().last() {
                    let transport_addr: Multiaddr = addr
                        .iter()
                        .filter(|p| !matches!(p, libp2p::multiaddr::Protocol::P2p(_)))
                        .collect();
                    info!(%peer_id, %transport_addr, "Dialing bootstrap peer");
                    swarm
                        .behaviour_mut()
                        .kademlia
                        .add_address(&peer_id, transport_addr);
                }
                if let Err(e) = swarm.dial(addr.clone()) {
                    warn!(%addr, "Failed to dial bootstrap peer: {e}");
                }
            }
            Err(e) => warn!(addr = %addr_str, "Invalid multiaddr: {e}"),
        }
    }

    info!("Node is up. Waiting for peers…");

    // Shared listen addresses — updated by the event loop, read by the
    // announce task so DHT records include reachable multiaddrs.
    let listen_addrs: Arc<RwLock<Vec<Multiaddr>>> = Arc::new(RwLock::new(Vec::new()));

    // -- Channels -----------------------------------------------------------

    // Async proxy responses: (response_channel, response_bytes)
    // The proxy handling task sends back here; main loop delivers to swarm.
    let (resp_tx, mut resp_rx) =
        mpsc::channel::<(request_response::ResponseChannel<Vec<u8>>, Vec<u8>)>(64);

    // DHT announce records from the background announce task.
    let (dht_tx, mut dht_rx) = mpsc::channel::<kad::Record>(4);

    // Notify channel: poked by ExternalAddrConfirmed so the announce task
    // re-publishes immediately instead of waiting for the next 5-min tick.
    let (announce_tx, mut announce_rx) = mpsc::channel::<()>(4);

    // -- Announce task ------------------------------------------------------
    // If a backend is configured, query its tools/list on startup and then
    // every ANNOUNCE_INTERVAL seconds.
    const ANNOUNCE_INTERVAL: u64 = 300; // 5 minutes

    // Load config for model advertise settings.
    let models_config = clawshake_core::config::load(None)
        .unwrap_or_default()
        .models;

    if let Some(ref b) = backend {
        let backend_ann = b.clone();
        let dht_tx_ann = dht_tx.clone();
        let peer_id_ann = local_peer_id;
        let addrs_ann = listen_addrs.clone();
        let perms_ann = Arc::clone(&store);
        let model_backend_ann = model_backend.clone();
        let advertise_ann = models_config.advertise.clone();
        let mut reannounce_rx = reannounce_rx;
        tokio::spawn(async move {
            let mut tick = interval(Duration::from_secs(ANNOUNCE_INTERVAL));
            tick.tick().await; // burn the immediate first tick
            loop {
                // Wait for periodic tick, ExternalAddrConfirmed, or external signal.
                tokio::select! {
                    _ = tick.tick() => {}
                    v = announce_rx.recv() => { if v.is_none() { break; } }
                    v = async {
                        match reannounce_rx.as_mut() {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending().await,
                        }
                    } => { if v.is_none() { break; } }
                }
                let addrs = addr::dedup_announce_addrs(&addrs_ann);
                let models = query_models(&model_backend_ann, &advertise_ann).await;
                match announce::build_record(
                    peer_id_ann,
                    &addrs,
                    &backend_ann,
                    &perms_ann,
                    models,
                )
                .await
                {
                    Ok(record) => {
                        if dht_tx_ann.send(record).await.is_err() {
                            break; // main loop exited
                        }
                    }
                    Err(e) => warn!("Announce build failed: {e}"),
                }
            }
        });
    }

    // -- Main event loop ----------------------------------------------------
    // Pending outbound P2P calls: request_id -> oneshot sender for the response.
    // Populated when network_call sends a request through the swarm; resolved
    // when the proxy Response or OutboundFailure event comes back.
    let mut pending_outbound: std::collections::HashMap<
        request_response::OutboundRequestId,
        oneshot::Sender<Result<Vec<u8>, String>>,
    > = std::collections::HashMap::new();

    // Pending outbound stream calls (model completions via the local proxy).
    let mut pending_stream_outbound: std::collections::HashMap<
        request_response::OutboundRequestId,
        oneshot::Sender<Result<Vec<u8>, String>>,
    > = std::collections::HashMap::new();

    // Async stream (model) responses: spawned tasks send back here.
    let (stream_resp_tx, mut stream_resp_rx) =
        mpsc::channel::<(request_response::ResponseChannel<Vec<u8>>, Vec<u8>)>(64);

    // -- Model streaming (libp2p-stream) ------------------------------------
    // Obtain a Control handle for opening outbound streams and accepting
    // inbound ones.  The protocol uses the same framing as request_response
    // (4-byte BE length prefix) but allows multiple frames per stream —
    // essential for real-time token streaming from model backends.
    let model_stream_protocol =
        StreamProtocol::try_from_owned("/clawshake/models/stream/1.0.0".to_string())
            .expect("valid model stream protocol");

    let mut model_stream_control = swarm.behaviour().model_stream.new_control();

    // Accept inbound model streams — spawn a long-lived task that loops over
    // incoming streams and handles each in its own sub-task.
    if let Some(ref mb) = model_backend {
        let mb_inbound = mb.clone();
        let store_inbound = Arc::clone(&store);
        let mut incoming = model_stream_control
            .accept(model_stream_protocol.clone())
            .expect("model stream protocol not yet registered");
        tokio::spawn(async move {
            while let Some((peer, stream)) = incoming.next().await {
                let mb = mb_inbound.clone();
                let store = store_inbound.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        handle_inbound_model_stream(&mb, stream, &peer.to_string(), &store).await
                    {
                        warn!(%peer, "Inbound model stream error: {e}");
                    }
                });
            }
        });
    }

    let mut state = event::NodeState {
        relay_banner_shown: false,
        relay_reserved_peers: std::collections::HashSet::new(),
        rendezvous_cookies: std::collections::HashMap::new(),
        rendezvous_registered: std::collections::HashSet::new(),
        rendezvous_servers: std::collections::HashSet::new(),
        peer_connections: std::collections::HashMap::new(),
        pending_dht_queries: std::collections::HashMap::new(),
    };
    let mut rendezvous_tick = interval(Duration::from_secs(10));
    let ctx = event::EventContext {
        table: &table,
        connected: &connected,
        relay_server,
        local_peer_id,
        listen_addrs: &listen_addrs,
        announce_tx: &announce_tx,
    };
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    let mut bootstrap_retry = interval(Duration::from_secs(60));
    bootstrap_retry.tick().await; // burn first immediate tick
    loop {
        select! {
            event = swarm.select_next_some() => {
                match event {
                    // ── Inbound proxy request from a remote peer ──────────────
                    SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Proxy(
                        request_response::Event::Message {
                            peer,
                            message: request_response::Message::Request {
                                request, channel, ..
                            },
                            ..
                        },
                    )) => {
                        if let Some(ref b) = backend {
                            let b = b.clone();
                            let s = store.clone();
                            let tx = resp_tx.clone();
                            tokio::spawn(async move {
                                let response = proxy::forward(&b, &s, &peer, request).await;
                                let _ = tx.send((channel, response)).await;
                            });
                        } else {
                            // No backend — return a JSON-RPC error.
                            let err = serde_json::to_vec(&serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": null,
                                "error": {
                                    "code": -32603,
                                    "message": "No MCP backend configured on this node"
                                }
                            }))
                            .unwrap_or_default();
                            let _ = resp_tx.send((channel, err)).await;
                        }
                    }

                    // ── Response to an outbound network_call ──────────────────
                    SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Proxy(
                        request_response::Event::Message {
                            message: request_response::Message::Response {
                                request_id, response,
                            },
                            ..
                        },
                    )) => {
                        if let Some(tx) = pending_outbound.remove(&request_id) {
                            let _ = tx.send(Ok(response));
                        } else {
                            info!("Proxy: received response for unknown request {request_id:?}");
                        }
                    }

                    // ── Outbound failure for a pending network_call ───────────
                    SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Proxy(
                        request_response::Event::OutboundFailure {
                            peer, request_id, error, ..
                        },
                    )) => {
                        if let Some(tx) = pending_outbound.remove(&request_id) {
                            let _ = tx.send(Err(format!("P2P call to {peer} failed: {error}")));
                        } else {
                            warn!("MCP outbound failure to {peer}: {error}");
                        }
                    }

                    // ── Inbound stream protocol request (model completions) ──
                    SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Stream(
                        request_response::Event::Message {
                            peer,
                            message: request_response::Message::Request {
                                request, channel, ..
                            },
                            ..
                        },
                    )) => {
                        if let Some(ref mb) = model_backend {
                            let mb = mb.clone();
                            let tx = stream_resp_tx.clone();
                            let perm_store = Arc::clone(&store);
                            tokio::spawn(async move {
                                let response = handle_model_request(&mb, &request, &peer.to_string(), &perm_store).await;
                                let _ = tx.send((channel, response)).await;
                            });
                        } else {
                            info!(%peer, "Inbound stream request but no model backend configured");
                            let err = serde_json::to_vec(&serde_json::json!({
                                "error": {
                                    "message": "No model backend configured on this node",
                                    "type": "server_error",
                                }
                            })).unwrap_or_default();
                            let _ = swarm.behaviour_mut().stream.send_response(channel, err);
                        }
                    }

                    // ── Stream protocol response (outbound model call result) ─
                    SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Stream(
                        request_response::Event::Message {
                            message: request_response::Message::Response {
                                request_id, response,
                            },
                            ..
                        },
                    )) => {
                        if let Some(tx) = pending_stream_outbound.remove(&request_id) {
                            let _ = tx.send(Ok(response));
                        } else {
                            info!("Stream: received response for unknown request {request_id:?}");
                        }
                    }

                    // ── Stream protocol outbound failure ──────────────────────
                    SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Stream(
                        request_response::Event::OutboundFailure {
                            peer, request_id, error, ..
                        },
                    )) => {
                        if let Some(tx) = pending_stream_outbound.remove(&request_id) {
                            let _ = tx.send(Err(format!("Stream call to {peer} failed: {error}")));
                        } else {
                            warn!("Stream outbound failure to {peer} ({request_id:?}): {error}");
                        }
                    }

                    // ── Everything else ───────────────────────────────────────
                    other => event::handle_event(&mut swarm, other, &ctx, &mut state),
                }
            }

            // network_call: send an outbound MCP request to a remote peer.
            Some(OutboundCall { peer_id, request, response_tx }) = call_rx.recv() => {
                match peer_id.parse::<PeerId>() {
                    Ok(pid) => {
                        let req_id = swarm.behaviour_mut().proxy.send_request(&pid, request);
                        pending_outbound.insert(req_id, response_tx);
                    }
                    Err(e) => {
                        let _ = response_tx.send(Err(format!("invalid peer_id '{peer_id}': {e}")));
                    }
                }
            }

            // DHT lookup: network_tools / network_record request a live GET.
            Some(DhtLookup { peer_id, response_tx }) = dht_lookup_rx.recv() => {
                match peer_id.parse::<PeerId>() {
                    Ok(pid) => {
                        let key = announce::record_key(&pid);
                        let query_id = swarm.behaviour_mut().kademlia.get_record(key);
                        state.pending_dht_queries.insert(query_id, response_tx);
                    }
                    Err(e) => {
                        let _ = response_tx.send(Err(format!("invalid peer_id '{peer_id}': {e}")));
                    }
                }
            }

            // Deliver async proxy responses back through the swarm.
            Some((channel, response)) = resp_rx.recv() => {
                let _ = swarm.behaviour_mut().proxy.send_response(channel, response);
            }

            // Deliver async stream (model) responses back through the swarm.
            Some((channel, response)) = stream_resp_rx.recv() => {
                let _ = swarm.behaviour_mut().stream.send_response(channel, response);
            }

            // Outbound stream call: model proxy sends a completion request to a peer.
            Some(OutboundStreamCall { peer_id, request, response_tx }) = async {
                match stream_call_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match peer_id.parse::<PeerId>() {
                    Ok(pid) => {
                        let req_id = swarm.behaviour_mut().stream.send_request(&pid, request);
                        pending_stream_outbound.insert(req_id, response_tx);
                    }
                    Err(e) => {
                        let _ = response_tx.send(Err(format!("invalid peer_id '{peer_id}': {e}")));
                    }
                }
            }

            // Outbound model streaming call: like above but uses libp2p-stream
            // for true frame-by-frame streaming.  Opens a bidirectional stream,
            // writes the request, then reads StreamFrame JSON objects and
            // forwards each through the mpsc channel.
            Some(OutboundModelStreamingCall { peer_id, request, frame_tx }) = async {
                match model_streaming_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match peer_id.parse::<PeerId>() {
                    Ok(pid) => {
                        let mut ctl = model_stream_control.clone();
                        let proto = model_stream_protocol.clone();
                        tokio::spawn(async move {
                            if let Err(e) = drive_outbound_model_stream(
                                &mut ctl, pid, proto, &request, frame_tx,
                            ).await {
                                warn!(%pid, "Outbound model stream failed: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        let _ = frame_tx.send(Err(format!("invalid peer_id '{peer_id}': {e}"))).await;
                    }
                }
            }

            // Publish DHT announcement records from the announce task.
            Some(record) = dht_rx.recv() => {
                match swarm.behaviour_mut().kademlia.put_record(record, kad::Quorum::One) {
                    Ok(_) => info!("DHT announce: publishing record"),
                    Err(e) => warn!("DHT put_record failed: {e:?}"),
                }
            }

            // Periodic rendezvous discover — query known rendezvous servers for
            // newly registered peers so already-online nodes find joiners fast.
            _ = rendezvous_tick.tick() => {
                for &rz_peer in &state.rendezvous_servers {
                    let ns = event::clawshake_namespace();
                    let cookie = state.rendezvous_cookies.get(&rz_peer).cloned();
                    swarm
                        .behaviour_mut()
                        .rendezvous_client
                        .discover(Some(ns), cookie, None, rz_peer);
                }
            }

            // Graceful shutdown on Ctrl+C.
            _ = &mut shutdown => {
                info!("Received shutdown signal, stopping…");
                break;
            }
            // Retry bootstrap peers when completely isolated — covers the case
            // where the bootstrap node was unreachable at startup.
            _ = bootstrap_retry.tick() => {
                let isolated = connected
                    .read()
                    .expect("connected peers lock")
                    .is_empty();
                if isolated {
                    warn!("No peers connected \u{2014} retrying bootstrap dials");
                    for addr_str in &all_boot_peers {
                        if let Ok(addr) = addr_str.parse::<Multiaddr>() {
                            let _ = swarm.dial(addr);
                        }
                    }
                }
            }
        }
    }
    info!("Node stopped.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Model proxy helpers
// ---------------------------------------------------------------------------

/// Query the model backend for available models, filtered by the advertise
/// configuration.  Returns an empty vec if no model backend is configured.
async fn query_models(
    backend: &Option<ModelBackend>,
    advertise: &AdvertiseModels,
) -> Vec<ModelAnnounce> {
    let backend = match backend {
        Some(b) => b,
        None => return Vec::new(),
    };
    if advertise.is_none() {
        return Vec::new();
    }

    let all_models = match backend.list_models().await {
        Ok(m) => m,
        Err(e) => {
            warn!("Failed to query model backend: {e}");
            return Vec::new();
        }
    };

    let result = match advertise {
        AdvertiseModels::All(_) => all_models,
        AdvertiseModels::List(names) => all_models
            .into_iter()
            .filter(|m| names.iter().any(|n| n == &m.name))
            .collect(),
        AdvertiseModels::None(_) => Vec::new(),
    };

    if !result.is_empty() {
        let names: Vec<&str> = result.iter().map(|m| m.name.as_str()).collect();
        info!(count = result.len(), models = ?names, "Advertising models on the network");
    }

    result
}

/// Parse a model request, check permissions, and generate a request ID.
///
/// Returns `Ok((ModelRequest, request_id))` on success, or `Err(error_bytes)`
/// suitable for sending back to the peer.
async fn validate_model_request(
    request_bytes: &[u8],
    peer: &str,
    store: &PermissionStore,
) -> std::result::Result<(clawshake_core::models::ModelRequest, String), Vec<u8>> {
    use clawshake_core::identity::AgentId;
    use clawshake_core::models::ModelRequest;
    use clawshake_core::permissions::Decision;

    let req: ModelRequest = match serde_json::from_slice(request_bytes) {
        Ok(r) => r,
        Err(e) => {
            warn!(%peer, "Failed to parse model request: {e}");
            return Err(serde_json::to_vec(&serde_json::json!({
                "error": {
                    "message": format!("Invalid model request: {e}"),
                    "type": "invalid_request_error",
                }
            }))
            .unwrap_or_default());
        }
    };

    let agent_id = AgentId::P2p(peer.to_string());
    match store.check(&agent_id, &req.model).await {
        Decision::Allow => {
            info!(model = %req.model, %peer, "Model access granted");
        }
        _ => {
            warn!(model = %req.model, %peer, "Model access denied");
            return Err(serde_json::to_vec(&serde_json::json!({
                "error": {
                    "message": format!(
                        "Permission denied: peer '{}' is not allowed to use model '{}'. \
                         Grant access with: clawshake permissions allow p2p:{} {}",
                        peer, req.model, peer, req.model
                    ),
                    "type": "permission_denied",
                }
            }))
            .unwrap_or_default());
        }
    }

    let request_id = format!(
        "p2p-{}-{}",
        peer.chars().take(8).collect::<String>(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );

    Ok((req, request_id))
}

/// Handle an inbound model completion request from a peer.
///
/// Parses the request bytes as a `ModelRequest`, runs the completion against
/// the local model backend, collects all streamed chunks into a single
/// non-streaming OpenAI-compatible response, and returns the response bytes.
async fn handle_model_request(
    backend: &ModelBackend,
    request_bytes: &[u8],
    peer: &str,
    store: &PermissionStore,
) -> Vec<u8> {
    use futures::StreamExt;

    let (req, request_id) = match validate_model_request(request_bytes, peer, store).await {
        Ok(v) => v,
        Err(bytes) => return bytes,
    };

    // Run the completion
    let stream = match backend.complete(&request_id, &req).await {
        Ok(s) => s,
        Err(e) => {
            warn!(%peer, "Model completion failed: {e}");
            return serde_json::to_vec(&serde_json::json!({
                "error": {
                    "message": format!("Model backend error: {e}"),
                    "type": "server_error",
                }
            }))
            .unwrap_or_default();
        }
    };

    // Collect all streamed chunks into a single response.
    // Each Chunk.data is an OpenAI streaming delta — we extract the content
    // text from each and concatenate.
    let mut content = String::new();
    let mut usage = None;
    let mut finish_reason = None;

    tokio::pin!(stream);
    while let Some(frame) = stream.next().await {
        match frame {
            clawshake_core::stream::StreamFrame::Chunk { data, .. } => {
                // Extract content from OpenAI delta format:
                // { "choices": [{ "delta": { "content": "..." }, "finish_reason": "stop" }] }
                if let Some(choices) = data.get("choices").and_then(|c| c.as_array()) {
                    for choice in choices {
                        if let Some(text) = choice
                            .get("delta")
                            .and_then(|d| d.get("content"))
                            .and_then(|c| c.as_str())
                        {
                            content.push_str(text);
                        }
                        if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
                            finish_reason = Some(reason.to_string());
                        }
                    }
                }
            }
            clawshake_core::stream::StreamFrame::Done { meta, .. } => {
                usage = meta;
            }
            clawshake_core::stream::StreamFrame::Error { message, .. } => {
                warn!(%peer, "Model stream error: {message}");
                return serde_json::to_vec(&serde_json::json!({
                    "error": {
                        "message": message,
                        "type": "server_error",
                    }
                }))
                .unwrap_or_default();
            }
        }
    }

    // Build a non-streaming OpenAI-compatible response.
    let mut response = serde_json::json!({
        "id": request_id,
        "object": "chat.completion",
        "model": req.model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": content,
            },
            "finish_reason": finish_reason.unwrap_or_else(|| "stop".to_string()),
        }],
    });

    if let Some(u) = usage {
        response["usage"] = u;
    }

    info!(model = %req.model, %peer, content_len = content.len(), "Model completion done");

    serde_json::to_vec(&response).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// libp2p-stream model streaming helpers
// ---------------------------------------------------------------------------

/// Handle an inbound model stream from a remote peer.
///
/// Reads the `ModelRequest` from the stream, runs the completion against the
/// local model backend, and writes each `StreamFrame` back as it's produced.
/// The remote proxy reads these frames and forwards them as SSE events to
/// the HTTP client — giving true token-by-token streaming.
async fn handle_inbound_model_stream(
    backend: &ModelBackend,
    mut stream: libp2p::swarm::Stream,
    peer: &str,
    store: &PermissionStore,
) -> Result<()> {
    use clawshake_core::stream::StreamFrame;
    use futures::io::AsyncWriteExt;

    // Read the request (length-prefixed).
    let request_bytes = codec::read_framed(&mut stream)
        .await
        .context("reading model request from stream")?;

    let (req, request_id) = match validate_model_request(&request_bytes, peer, store).await {
        Ok(v) => v,
        Err(deny_bytes) => {
            // Permission denied / parse error — send as a StreamFrame::Error.
            let err = StreamFrame::error("denied", String::from_utf8_lossy(&deny_bytes), Some(-32001));
            codec::write_framed(&mut stream, &err.to_bytes()).await?;
            stream.close().await?;
            return Ok(());
        }
    };

    // Run the completion — returns a stream of StreamFrames.
    let frame_stream = match backend.complete(&request_id, &req).await {
        Ok(s) => s,
        Err(e) => {
            let err = StreamFrame::error(&request_id, format!("Backend error: {e}"), Some(-32603));
            codec::write_framed(&mut stream, &err.to_bytes()).await?;
            stream.close().await?;
            return Ok(());
        }
    };

    // Forward each frame from the backend to the remote peer.
    tokio::pin!(frame_stream);
    while let Some(frame) = futures::StreamExt::next(&mut frame_stream).await {
        let bytes = frame.to_bytes();
        if codec::write_framed(&mut stream, &bytes).await.is_err() {
            warn!(%peer, "Stream write failed — peer disconnected?");
            return Ok(());
        }
    }

    stream.close().await?;
    info!(model = %req.model, %peer, "Model stream completed");
    Ok(())
}

/// Drive an outbound model stream: open a bidirectional stream to the peer,
/// write the request, and read `StreamFrame`s back, forwarding each through
/// the mpsc channel to the proxy's SSE response.
async fn drive_outbound_model_stream(
    control: &mut libp2p_stream::Control,
    peer: PeerId,
    protocol: StreamProtocol,
    request: &[u8],
    frame_tx: mpsc::Sender<Result<Vec<u8>, String>>,
) -> Result<()> {
    use futures::io::AsyncWriteExt;

    info!(%peer, "Opening model stream");

    let mut stream = control
        .open_stream(peer, protocol)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to open model stream to {peer}: {e}"))?;

    // Write the request (length-prefixed).
    codec::write_framed(&mut stream, request)
        .await
        .context("writing model request to stream")?;

    // Read frames until the stream is closed or a Done/Error frame arrives.
    loop {
        match codec::read_framed(&mut stream).await {
            Ok(frame_bytes) => {
                // Check if this is a terminal frame (Done or Error)
                let is_terminal = matches!(
                    clawshake_core::stream::StreamFrame::from_bytes(&frame_bytes),
                    Ok(clawshake_core::stream::StreamFrame::Done { .. })
                        | Ok(clawshake_core::stream::StreamFrame::Error { .. })
                );

                if frame_tx.send(Ok(frame_bytes)).await.is_err() {
                    warn!(%peer, "Frame receiver dropped — client disconnected?");
                    break;
                }

                if is_terminal {
                    break;
                }
            }
            Err(e) => {
                // EOF = stream closed by peer (normal after Done frame).
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    break;
                }
                let _ = frame_tx.send(Err(format!("Stream read error: {e}"))).await;
                break;
            }
        }
    }

    let _ = stream.close().await;
    info!(%peer, "Model stream finished");
    Ok(())
}
