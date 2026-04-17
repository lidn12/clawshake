mod addr;
mod event;
mod keypair;
pub(crate) mod tunnel;

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock},
    time::Duration,
};

use crate::announce;
use anyhow::{Context, Result};
use clawshake_core::{
    mcp_client::McpClient,
    network_channel::{ConnectedPeers, DhtLookup, OutboundCall, TunnelTable},
    peer_table::PeerTable,
    permissions::PermissionStore,
};
use libp2p::futures::StreamExt;
use libp2p::{
    autonat, dcutr, identify, kad, mdns, noise, relay, rendezvous,
    swarm::{behaviour::toggle::Toggle, NetworkBehaviour},
    tcp, upnp, yamux, Multiaddr, PeerId, StreamProtocol, SwarmBuilder,
};
use tokio::{select, sync::mpsc, time::interval};
use tracing::{info, warn};

/// Read the local keypair from disk and return the derived `PeerId`.
///
/// Uses the same key file that `run()` loads (`~/.clawshake/identity.key` or
/// the override path).  Does **not** generate a new key if none exists.
pub fn peer_id_from_disk(override_path: Option<&std::path::Path>) -> Result<PeerId> {
    let path = match override_path {
        Some(p) => p.to_path_buf(),
        None => clawshake_core::config::config_dir()
            .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
            .join("identity.key"),
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

// ---------------------------------------------------------------------------
// Composite behaviour
// ---------------------------------------------------------------------------

#[derive(NetworkBehaviour)]
struct ClawshakeBehaviour {
    kademlia: kad::Behaviour<kad::store::MemoryStore>,
    identify: identify::Behaviour,
    mdns: mdns::tokio::Behaviour,
    /// Hosts tunnel (`/clawshake/tunnel/1.0.0`) and `_rpc` streams.
    streams: libp2p_stream::Behaviour,
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
    /// Sender half: cloned into `EventContext` so internal P2P events
    /// (ExternalAddrConfirmed) and external sources (permission changes,
    /// manifest watcher, expose/unexpose) all feed the same announce task.
    pub announce_tx: mpsc::Sender<()>,
    /// Receiver half: consumed by the background announce task.
    pub announce_rx: mpsc::Receiver<()>,
    pub dht_lookup_rx: mpsc::Receiver<DhtLookup>,
    /// Bridge-side tunnel table, shared with the IPC handler.
    pub tunnel_table: TunnelTable,
    /// Pre-loaded configuration — avoids redundant `config::load()` calls.
    pub config: clawshake_core::config::Config,
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
        announce_tx,
        mut announce_rx,
        mut dht_lookup_rx,
        tunnel_table,
        config: app_config,
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

            let streams = libp2p_stream::Behaviour::new();

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
                streams,
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
        let mut peers = app_config.network.bootstrap.clone();
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

    // DHT announce records from the background announce task.
    let (dht_tx, mut dht_rx) = mpsc::channel::<kad::Record>(4);

    // -- Announce task ------------------------------------------------------
    // Always runs: publishes the node's description, addresses, and (when a
    // backend is configured) its tool and model list to the DHT.  Relay-only
    // or no-backend nodes still announce so their description is visible to
    // peers via network_peers.
    const ANNOUNCE_INTERVAL: u64 = 300; // 5 minutes

    let node_description = app_config.network.description;

    {
        let backend_ann = backend.clone();
        let dht_tx_ann = dht_tx.clone();
        let peer_id_ann = local_peer_id;
        let addrs_ann = listen_addrs.clone();
        let perms_ann = Arc::clone(&store);
        let description_ann = node_description.clone();
        tokio::spawn(async move {
            let mut tick = interval(Duration::from_secs(ANNOUNCE_INTERVAL));
            tick.tick().await; // burn the immediate first tick
            loop {
                // Wait for periodic tick or any announce signal (internal
                // P2P events + external permission/manifest/expose changes).
                tokio::select! {
                    _ = tick.tick() => {}
                    v = announce_rx.recv() => { if v.is_none() { break; } }
                }
                let addrs = addr::dedup_announce_addrs(&addrs_ann);
                match announce::build_record(
                    peer_id_ann,
                    &addrs,
                    backend_ann.as_ref(),
                    &perms_ann,
                    description_ann.clone(),
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

    // -- TCP tunnel (libp2p-stream) -----------------------------------------
    // Accept inbound tunnel streams from peers that call `connect_{name}`
    // via `network_call`.  The tunnel protocol is separate from model streams.
    let tunnel_protocol = StreamProtocol::try_from_owned("/clawshake/tunnel/1.0.0".to_string())
        .expect("valid tunnel protocol");

    let mut tunnel_stream_control = swarm.behaviour().streams.new_control();
    let tunnel_incoming = tunnel_stream_control
        .accept(tunnel_protocol.clone())
        .expect("tunnel protocol not yet registered");
    {
        let tt = tunnel_table.clone();
        let rpc_ctx = Arc::new(tunnel::RpcContext {
            backend: backend.clone(),
            permissions: Arc::clone(&store),
        });
        tokio::spawn(async move {
            tunnel::accept_inbound_tunnels(tunnel_incoming, tt, rpc_ctx).await;
        });
    }

    // -- Auto-connect task (declarative [[connects]] entries) ---------------
    if !app_config.connects.is_empty() {
        let connects = app_config.connects.clone();
        let connected_watch = connected.clone();
        let mut sc = tunnel_stream_control.clone();
        let tp = tunnel_protocol.clone();
        tokio::spawn(async move {
            tunnel::auto_connect_task(connects, connected_watch, &mut sc, tp).await;
        });
    }

    let mut state = event::NodeState {
        relay_banner_shown: false,
        relay_reserved_peers: HashSet::new(),
        rendezvous_cookies: HashMap::new(),
        rendezvous_registered: HashSet::new(),
        rendezvous_servers: HashSet::new(),
        peer_connections: HashMap::new(),
        pending_dht_queries: HashMap::new(),
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
                event::handle_event(&mut swarm, event, &ctx, &mut state);
            }

            // network_call: send an outbound MCP request to a remote peer
            // via the _rpc tunnel.
            Some(OutboundCall { peer_id, request, response_tx }) = call_rx.recv() => {
                match peer_id.parse::<PeerId>() {
                    Ok(pid) => {
                        let mut tsc = tunnel_stream_control.clone();
                        let tp = tunnel_protocol.clone();
                        tokio::spawn(async move {
                            let rpc_future = tunnel::send_rpc_via_tunnel(
                                pid, request, &mut tsc, tp,
                            );
                            let result = match tokio::time::timeout(
                                std::time::Duration::from_secs(60),
                                rpc_future,
                            )
                            .await
                            {
                                Ok(r) => r,
                                Err(_) => Err(format!(
                                    "RPC call to {pid} timed out after 60 s"
                                )),
                            };
                            let _ = response_tx.send(result);
                        });
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
