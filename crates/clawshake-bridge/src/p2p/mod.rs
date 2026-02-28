mod addr;
mod event;
mod keypair;

use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use crate::{announce, proxy};
use anyhow::Result;
use clawshake_core::{
    mcp_client::McpClient,
    network_channel::{ConnectedPeers, OutboundCall},
    peer_table::PeerTable,
    permissions::PermissionStore,
};
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
    relay_server: Toggle<relay::Behaviour>,
    relay_client: relay::client::Behaviour,
    autonat: autonat::Behaviour,
    dcutr: dcutr::Behaviour,
    rendezvous_client: rendezvous::client::Behaviour,
    rendezvous_server: Toggle<rendezvous::server::Behaviour>,
    upnp: Toggle<upnp::tokio::Behaviour>,
}

// ---------------------------------------------------------------------------
// Well-known bootstrap peers
// ---------------------------------------------------------------------------

/// Hardcoded bootstrap peers dialed on startup when `--no-default-boot` is
/// not set.  Format: `/ip4/<ip>/tcp/7474/p2p/<peer-id>`
const BOOTSTRAP_PEERS: &[&str] = &[
    "/ip4/43.143.33.106/tcp/7474/p2p/12D3KooWDi1ntKAkUYpHfijLNExUTsirFyofnkEB3yjC8P3EGcY5",
    "/ip4/43.143.33.106/udp/7474/quic-v1/p2p/12D3KooWDi1ntKAkUYpHfijLNExUTsirFyofnkEB3yjC8P3EGcY5",
];

/// Default port used by relay/bootstrap nodes (stable so the address is predictable).
pub const RELAY_DEFAULT_PORT: u16 = 7474;

// ---------------------------------------------------------------------------
// Node entry point
// ---------------------------------------------------------------------------

pub async fn run(
    p2p_port: u16,
    boot_peers: Vec<String>,
    identity: Option<std::path::PathBuf>,
    backend: Option<McpClient>,
    store: Arc<PermissionStore>,
    table: Arc<PeerTable>,
    connected: ConnectedPeers,
    no_default_boot: bool,
    relay_server: bool,
    mut call_rx: mpsc::Receiver<OutboundCall>,
    reannounce_rx: Option<mpsc::Receiver<()>>,
) -> Result<()> {
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

    // Build the complete list of bootstrap peers to dial on startup.
    // User-supplied --boot peers always take effect.
    // Hardcoded default peers are added unless --no-default-boot is set.
    let all_boot_peers: Vec<String> = {
        let mut peers = boot_peers;
        if !no_default_boot {
            peers.extend(BOOTSTRAP_PEERS.iter().map(|s| s.to_string()));
        }
        peers
    };

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

    if let Some(ref b) = backend {
        let backend_clone = b.clone();
        let dht_tx_clone = dht_tx.clone();
        let peer_id_clone = local_peer_id;
        let addrs_clone = listen_addrs.clone();
        let perms_clone = Arc::clone(&store);
        tokio::spawn(async move {
            let mut tick = interval(Duration::from_secs(ANNOUNCE_INTERVAL));
            tick.tick().await; // burn the immediate first tick
            loop {
                tick.tick().await;
                let addrs = addr::dedup_announce_addrs(&addrs_clone);
                match announce::build_record(peer_id_clone, &addrs, &backend_clone, &perms_clone)
                    .await
                {
                    Ok(record) => {
                        if dht_tx_clone.send(record).await.is_err() {
                            break; // main loop exited
                        }
                    }
                    Err(e) => warn!("Announce build failed: {e}"),
                }
            }
        });

        // Event-driven announce: re-publish immediately when:
        //  - ExternalAddrConfirmed fires (main loop sends through announce_tx)
        //  - External signal (manifest change, permission change, etc.)
        let backend_event = b.clone();
        let dht_tx_event = dht_tx.clone();
        let addrs_event = listen_addrs.clone();
        let perms_event = Arc::clone(&store);
        let mut reannounce_rx = reannounce_rx;
        tokio::spawn(async move {
            loop {
                // Wait for either signal.
                let got = tokio::select! {
                    v = announce_rx.recv() => v.is_some(),
                    v = async {
                        match reannounce_rx.as_mut() {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending().await,
                        }
                    } => v.is_some(),
                };
                if !got {
                    break;
                }
                let addrs = addr::dedup_announce_addrs(&addrs_event);
                match announce::build_record(local_peer_id, &addrs, &backend_event, &perms_event)
                    .await
                {
                    Ok(record) => {
                        let _ = dht_tx_event.send(record).await;
                    }
                    Err(e) => warn!("Event-driven announce failed: {e}"),
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

    let mut state = event::NodeState {
        relay_banner_shown: false,
        relay_reserved_peers: std::collections::HashSet::new(),
        rendezvous_cookies: std::collections::HashMap::new(),
        rendezvous_registered: std::collections::HashSet::new(),
        rendezvous_servers: std::collections::HashSet::new(),
        peer_connections: std::collections::HashMap::new(),
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

            // Deliver async proxy responses back through the swarm.
            Some((channel, response)) = resp_rx.recv() => {
                let _ = swarm.behaviour_mut().proxy.send_response(channel, response);
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
