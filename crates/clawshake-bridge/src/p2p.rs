use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{Context, Result};
use clawshake_core::{peer_table::PeerTable, permissions::PermissionStore};
use libp2p::futures::StreamExt;
use libp2p::{
    autonat, dcutr, identify, kad, mdns, noise, relay, rendezvous, request_response, upnp,
    swarm::{behaviour::toggle::Toggle, NetworkBehaviour, SwarmEvent},
    tcp, yamux, Multiaddr, PeerId, StreamProtocol, SwarmBuilder,
};
use tokio::{select, sync::mpsc, time::interval};
use tracing::{info, warn};

use crate::{announce, backend::McpBackend, network::ConnectedPeers, proxy};

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
// Keypair persistence
// ---------------------------------------------------------------------------

fn load_or_create_keypair(
    override_path: Option<&std::path::Path>,
) -> Result<libp2p::identity::Keypair> {
    let path = match override_path {
        Some(p) => p.to_path_buf(),
        None => keypair_path()?,
    };

    if path.exists() {
        let bytes = std::fs::read(&path)
            .with_context(|| format!("reading keypair from {}", path.display()))?;
        let keypair = libp2p::identity::Keypair::from_protobuf_encoding(&bytes)
            .context("decoding keypair")?;
        info!("Loaded existing keypair from {}", path.display());
        Ok(keypair)
    } else {
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let bytes = keypair.to_protobuf_encoding().context("encoding keypair")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }
        std::fs::write(&path, &bytes)
            .with_context(|| format!("writing keypair to {}", path.display()))?;
        info!("Generated new keypair, saved to {}", path.display());
        Ok(keypair)
    }
}

fn keypair_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home.join(".clawshake").join("identity.key"))
}

// ---------------------------------------------------------------------------
// Node entry point
// ---------------------------------------------------------------------------

pub async fn run(
    p2p_port: u16,
    boot_peers: Vec<String>,
    identity: Option<std::path::PathBuf>,
    backend: Option<McpBackend>,
    store: Arc<PermissionStore>,
    table: Arc<PeerTable>,
    connected: ConnectedPeers,
    no_default_boot: bool,
    relay_server: bool,
) -> Result<()> {
    let keypair = load_or_create_keypair(identity.as_deref())?;
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
        tracing::debug!("IPv6 TCP listen failed (no v6 support?): {e}");
    }
    let quic6_addr: Multiaddr = format!("/ip6/::/udp/{p2p_port}/quic-v1").parse()?;
    if let Err(e) = swarm.listen_on(quic6_addr) {
        tracing::debug!("IPv6 QUIC listen failed (no v6 support?): {e}");
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
        tokio::spawn(async move {
            let mut tick = interval(Duration::from_secs(ANNOUNCE_INTERVAL));
            tick.tick().await; // burn the immediate first tick
            loop {
                tick.tick().await;
                let addrs: Vec<Multiaddr> = addrs_clone
                    .read()
                    .expect("listen_addrs lock")
                    .iter()
                    .filter(|a| is_globally_reachable(a))
                    .cloned()
                    .collect();
                match announce::build_record(peer_id_clone, &addrs, &backend_clone).await {
                    Ok(record) => {
                        if dht_tx_clone.send(record).await.is_err() {
                            break; // main loop exited
                        }
                    }
                    Err(e) => warn!("Announce build failed: {e}"),
                }
            }
        });

        // Event-driven announce: whenever ExternalAddrConfirmed fires, the
        // main loop sends a () through announce_tx so we re-publish right away
        // instead of waiting for the next 5-minute tick.
        let backend_event = b.clone();
        let dht_tx_event = dht_tx.clone();
        let addrs_event = listen_addrs.clone();
        tokio::spawn(async move {
            while announce_rx.recv().await.is_some() {
                let addrs: Vec<Multiaddr> = addrs_event
                    .read()
                    .expect("listen_addrs lock")
                    .iter()
                    .filter(|a| is_globally_reachable(a))
                    .cloned()
                    .collect();
                match announce::build_record(local_peer_id, &addrs, &backend_event).await {
                    Ok(record) => {
                        let _ = dht_tx_event.send(record).await;
                    }
                    Err(e) => warn!("Event-driven announce failed: {e}"),
                }
            }
        });
    }

    // -- Main event loop ----------------------------------------------------
    let mut state = NodeState {
        relay_banner_shown: false,
        relay_reserved_peers: std::collections::HashSet::new(),
        rendezvous_cookies: std::collections::HashMap::new(),
        rendezvous_registered: std::collections::HashSet::new(),
        rendezvous_servers: std::collections::HashSet::new(),
    };
    let mut rendezvous_tick = interval(Duration::from_secs(10));
    let ctx = EventContext {
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
                // Handle proxy inbound requests inline (needs async).
                if let SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Proxy(
                    request_response::Event::Message {
                        peer,
                        message: request_response::Message::Request {
                            request, channel, ..
                        },
                        ..
                    },
                )) = event
                {
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
                } else {
                    handle_event(&mut swarm, event, &ctx, &mut state);
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
                    let ns = clawshake_namespace();
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
// Event handler (non-proxy events)
// ---------------------------------------------------------------------------

/// libp2p relay v2 hop protocol — advertised by nodes that can relay traffic.
const RELAY_HOP_PROTOCOL: &str = "/libp2p/circuit/relay/0.2.0/hop";

/// Rendezvous protocol — advertised by nodes running a rendezvous server.
const RENDEZVOUS_PROTOCOL: &str = "/rendezvous/1.0.0";

/// Namespace used for clawshake peer discovery via rendezvous.
const RENDEZVOUS_NAMESPACE: &str = "clawshake";

/// Creates a `Namespace` for the clawshake rendezvous point.
fn clawshake_namespace() -> rendezvous::Namespace {
    rendezvous::Namespace::new(RENDEZVOUS_NAMESPACE.to_string()).expect("valid namespace")
}

/// Returns true if `addr` contains a publicly routable IP — i.e. not loopback,
/// link-local, RFC-1918 private, or a relay circuit address.  Used to select
/// relay circuit base addresses and as a building block for
/// [`is_globally_reachable`].
fn is_public_addr(addr: &Multiaddr) -> bool {
    use libp2p::multiaddr::Protocol;
    let mut has_ip = false;
    for proto in addr.iter() {
        match proto {
            Protocol::P2pCircuit => return false,
            Protocol::Ip4(ip) => {
                has_ip = true;
                if ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified()
                {
                    return false;
                }
            }
            Protocol::Ip6(ip) => {
                has_ip = true;
                let segments = ip.segments();
                let is_link_local = segments[0] == 0xfe80;
                let is_ula = (segments[0] & 0xfe00) == 0xfc00;
                if ip.is_loopback() || ip.is_unspecified() || is_link_local || is_ula {
                    return false;
                }
            }
            _ => {}
        }
    }
    has_ip
}

/// Returns true if `addr` is useful for remote peers — either a publicly
/// routable IP or a relay circuit address.  Relay circuit addresses are always
/// included since they are reachable through the relay node.
fn is_globally_reachable(addr: &Multiaddr) -> bool {
    use libp2p::multiaddr::Protocol;
    if addr.iter().any(|p| matches!(p, Protocol::P2pCircuit)) {
        return true;
    }
    is_public_addr(addr)
}

/// Mutable state tracked across the main event loop.
struct NodeState {
    relay_banner_shown: bool,
    relay_reserved_peers: std::collections::HashSet<PeerId>,
    rendezvous_cookies: std::collections::HashMap<PeerId, rendezvous::Cookie>,
    rendezvous_registered: std::collections::HashSet<PeerId>,
    rendezvous_servers: std::collections::HashSet<PeerId>,
}

/// Immutable context shared across event handling — avoids passing many
/// individual parameters through `handle_event`.
struct EventContext<'a> {
    table: &'a PeerTable,
    connected: &'a ConnectedPeers,
    relay_server: bool,
    local_peer_id: PeerId,
    listen_addrs: &'a RwLock<Vec<Multiaddr>>,
    /// Poked on `ExternalAddrConfirmed` so the announce task re-publishes
    /// immediately with the newly reachable address.
    announce_tx: &'a mpsc::Sender<()>,
}

fn handle_event(
    swarm: &mut libp2p::Swarm<ClawshakeBehaviour>,
    event: SwarmEvent<ClawshakeBehaviourEvent>,
    ctx: &EventContext<'_>,
    state: &mut NodeState,
) {
    match event {
        SwarmEvent::NewListenAddr { ref address, .. } => {
            info!("Listening on {address}");
            ctx.listen_addrs
                .write()
                .expect("listen_addrs lock")
                .push(address.clone());
        }

        SwarmEvent::ExpiredListenAddr { ref address, .. } => {
            info!("Listen address expired: {address}");
            ctx.listen_addrs
                .write()
                .expect("listen_addrs lock")
                .retain(|a| a != address);
        }

        SwarmEvent::ConnectionEstablished {
            peer_id,
            endpoint,
            num_established,
            ..
        } => {
            let addr = endpoint.get_remote_address();
            let is_relayed = addr
                .iter()
                .any(|p| matches!(p, libp2p::multiaddr::Protocol::P2pCircuit));
            let via = if is_relayed {
                if endpoint.is_dialer() {
                    "outbound-relay" // we dialed relay → DCUTR responder
                } else {
                    "inbound-relay" // peer dialed relay → DCUTR initiator
                }
            } else if endpoint.is_dialer() {
                "outbound"
            } else {
                "inbound"
            };
            info!("Connected to {peer_id} ({via}) addr={addr}");
            // Track as reachable for network.ping
            ctx.connected
                .write()
                .expect("connected peers lock poisoned")
                .insert(peer_id.to_string());
            // Fetch the peer's DHT announcement only on the *first* connection.
            // DCUTR upgrades create a second connection to the same peer;
            // re-querying would waste bandwidth.
            if num_established.get() == 1 {
                let key = kad::RecordKey::new(&peer_id.to_bytes());
                swarm.behaviour_mut().kademlia.get_record(key);
            }
        }

        SwarmEvent::ConnectionClosed {
            peer_id,
            cause,
            num_established,
            ..
        } => {
            info!("Disconnected from {peer_id}: {cause:?} (remaining={num_established})");
            // Only clean up when the *last* connection to this peer closes.
            // A peer may have multiple connections (e.g. relay + direct after
            // DCUTR) and we must not wipe state while one is still alive.
            if num_established == 0 {
                ctx.connected
                    .write()
                    .expect("connected peers lock poisoned")
                    .remove(&peer_id.to_string());
                state.relay_reserved_peers.remove(&peer_id);
                state.rendezvous_cookies.remove(&peer_id);
                state.rendezvous_registered.remove(&peer_id);
                state.rendezvous_servers.remove(&peer_id);
            }
        }

        // mDNS: dial newly discovered local peers (their GetRecord fills the table)
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Mdns(mdns::Event::Discovered(peers))) => {
            for (peer_id, addr) in peers {
                info!("mDNS discovered peer {peer_id} at {addr}");
                swarm.behaviour_mut().kademlia.add_address(&peer_id, addr);
                let _ = swarm.dial(peer_id);
            }
        }

        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Mdns(mdns::Event::Expired(peers))) => {
            for (peer_id, addr) in peers {
                info!("mDNS peer expired: {peer_id} at {addr}");
                swarm
                    .behaviour_mut()
                    .kademlia
                    .remove_address(&peer_id, &addr);
            }
        }

        // Identify
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Identify(identify::Event::Received {
            peer_id,
            info,
            ..
        })) => {
            info!(
                "Identified {peer_id}: agent=\"{}\" protocol=\"{}\" observed_addr={}",
                info.agent_version, info.protocol_version, info.observed_addr
            );
            // NOTE: Regular (NATted) nodes must NOT call
            // `swarm.add_external_address()` here — Identify internally emits
            // `NewExternalAddrCandidate` for the observed address and AutoNAT
            // is the authority for confirming it.  Confirming prematurely would
            // block DCUTR candidates (see commit 88e1ead).
            //
            // Relay servers are the exception: they ARE publicly reachable (the
            // operator asserts this with --relay-server) and they need their
            // external address confirmed BEFORE the first RESERVE request
            // arrives, otherwise the reservation response contains no addresses
            // and clients get `NoAddressesInReservation`.  AutoNAT's 15s boot
            // delay is too slow for this.
            if ctx.relay_server && is_public_addr(&info.observed_addr) {
                swarm.add_external_address(info.observed_addr.clone());
            }
            // Add publicly routable advertised addresses to Kademlia.
            // Private/loopback addresses are useless for remote peers.
            for addr in info.listen_addrs.iter().filter(|a| is_public_addr(a)) {
                swarm
                    .behaviour_mut()
                    .kademlia
                    .add_address(&peer_id, addr.clone());
            }

            // Direct-connect upgrade: when we learn a peer's addresses via
            // Identify over a *relay* connection, proactively try dialing
            // their private (LAN) listen addresses.  If we share a LAN this
            // establishes a direct path immediately — without waiting for
            // the relay's 120 s idle timeout — and seeds DCUTR's candidate
            // cache with LAN addresses for future hole-punch exchanges.
            //
            // We detect a relay-mediated Identify by a bare `/p2p/<id>`
            // observed_addr (no IP component).
            let is_relay_observed = !info.observed_addr.iter().any(|p| {
                matches!(
                    p,
                    libp2p::multiaddr::Protocol::Ip4(_) | libp2p::multiaddr::Protocol::Ip6(_)
                )
            });
            if is_relay_observed {
                let private_addrs: Vec<Multiaddr> = info
                    .listen_addrs
                    .iter()
                    .filter(|a| {
                        !is_public_addr(a)
                            && !a
                                .iter()
                                .any(|p| matches!(p, libp2p::multiaddr::Protocol::P2pCircuit))
                            && a.iter().any(|p| match p {
                                libp2p::multiaddr::Protocol::Ip4(ip) => {
                                    !ip.is_loopback() && !ip.is_unspecified()
                                }
                                libp2p::multiaddr::Protocol::Ip6(ip) => {
                                    !ip.is_loopback() && !ip.is_unspecified()
                                }
                                _ => false,
                            })
                    })
                    .cloned()
                    .collect();
                if !private_addrs.is_empty() {
                    tracing::debug!(
                        "Attempting direct dial to {peer_id} via {} private addr(s)",
                        private_addrs.len()
                    );
                    let opts = libp2p::swarm::dial_opts::DialOpts::peer_id(peer_id)
                        .addresses(private_addrs)
                        .condition(libp2p::swarm::dial_opts::PeerCondition::Always)
                        .build();
                    if let Err(e) = swarm.dial(opts) {
                        tracing::debug!("Direct dial to {peer_id} failed: {e}");
                    }
                }
            }

            // Rendezvous: if this peer runs a rendezvous server, discover peers
            // from it so we find other nodes immediately (no DHT polling wait).
            if !ctx.relay_server
                && info
                    .protocols
                    .iter()
                    .any(|p| p.as_ref() == RENDEZVOUS_PROTOCOL)
            {
                state.rendezvous_servers.insert(peer_id);
                let ns = clawshake_namespace();
                let cookie = state.rendezvous_cookies.get(&peer_id).cloned();
                swarm
                    .behaviour_mut()
                    .rendezvous_client
                    .discover(Some(ns), cookie, None, peer_id);
                info!("Rendezvous: discovering peers from {peer_id}");

                // Also try to register — will fail with NoExternalAddresses if
                // we don't have any yet; we retry on ExternalAddrConfirmed.
                if !state.rendezvous_registered.contains(&peer_id) {
                    let ns = clawshake_namespace();
                    match swarm
                        .behaviour_mut()
                        .rendezvous_client
                        .register(ns, peer_id, None)
                    {
                        Ok(_) => {
                            info!("Rendezvous: registering with {peer_id}");
                            state.rendezvous_registered.insert(peer_id);
                        }
                        Err(e) => {
                            tracing::debug!(
                                "Rendezvous: register deferred (no external addrs yet): {e:?}"
                            );
                        }
                    }
                }
            }

            // Auto-relay: if this peer advertises relay hop capability, reserve a
            // circuit slot on it so we are reachable through it even behind NAT.
            // Relay servers don't need to be relay clients — skip for them.
            if !ctx.relay_server
                && !state.relay_reserved_peers.contains(&peer_id)
                && info
                    .protocols
                    .iter()
                    .any(|p| p.as_ref() == RELAY_HOP_PROTOCOL)
            {
                // Prefer a publicly routable listen address for the circuit;
                // fall back to any non-loopback, non-circuit address.
                let circuit_base = info
                    .listen_addrs
                    .iter()
                    .find(|a| is_public_addr(a))
                    .or_else(|| {
                        info.listen_addrs.iter().find(|a| {
                            !a.iter().any(|p| {
                                matches!(p, libp2p::multiaddr::Protocol::Ip4(ip) if ip.is_loopback())
                                    || matches!(p, libp2p::multiaddr::Protocol::P2pCircuit)
                            })
                        })
                    });
                if let Some(base) = circuit_base {
                    // Strip any trailing /p2p component, then append /p2p/<relay>/p2p-circuit.
                    let mut circuit: Multiaddr = base
                        .iter()
                        .filter(|p| !matches!(p, libp2p::multiaddr::Protocol::P2p(_)))
                        .collect();
                    circuit.push(libp2p::multiaddr::Protocol::P2p(peer_id));
                    circuit.push(libp2p::multiaddr::Protocol::P2pCircuit);
                    info!("Auto-relay: reserving slot via {peer_id}");
                    if let Err(e) = swarm.listen_on(circuit) {
                        warn!("Auto-relay: listen_on failed: {e}");
                    }
                    state.relay_reserved_peers.insert(peer_id);
                }
            }
        }

        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Identify(identify::Event::Error {
            peer_id,
            error,
            ..
        })) => {
            warn!("Identify error for {peer_id}: {error}");
        }

        // Kademlia
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Kademlia(event)) => match event {
            kad::Event::RoutingUpdated { peer, .. } => {
                tracing::debug!("Kademlia routing table updated: peer={peer}");
            }
            kad::Event::RoutablePeer { peer, address } => {
                tracing::debug!("Kademlia routable peer: {peer} at {address}");
            }
            kad::Event::OutboundQueryProgressed {
                result: kad::QueryResult::PutRecord(Ok(kad::PutRecordOk { key })),
                ..
            } => {
                info!("DHT announce: record published (key={key:?})");
            }
            kad::Event::OutboundQueryProgressed {
                result: kad::QueryResult::PutRecord(Err(e)),
                ..
            } => {
                warn!("DHT announce: put_record error: {e:?}");
            }
            // Parse a fetched peer announcement and populate the peer table.
            kad::Event::OutboundQueryProgressed {
                result:
                    kad::QueryResult::GetRecord(Ok(kad::GetRecordOk::FoundRecord(kad::PeerRecord {
                        record,
                        ..
                    }))),
                ..
            } => match announce::AnnouncementRecord::from_bytes(&record.value) {
                Ok(ann) => {
                    let info = ann.to_peer_info();
                    info!(peer = %info.peer_id, tools = info.tools.len(), "Peer table updated from DHT");
                    ctx.table.upsert(info);
                }
                Err(e) => {
                    tracing::debug!("GetRecord: not a clawshake announcement: {e}");
                }
            },
            other => {
                tracing::debug!("Kademlia event: {other:?}");
            }
        },

        // Proxy outbound responses / send errors
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Proxy(event)) => match event {
            request_response::Event::Message {
                peer,
                message: request_response::Message::Response { response, .. },
                ..
            } => {
                info!(
                    "Received MCP response from {peer}: {} bytes",
                    response.len()
                );
            }
            request_response::Event::OutboundFailure { peer, error, .. } => {
                warn!("MCP outbound failure to {peer}: {error}");
            }
            request_response::Event::InboundFailure { peer, error, .. } => {
                warn!("MCP inbound failure from {peer}: {error}");
            }
            _ => {}
        },

        // Relay client — circuit relay reservations and connections
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::RelayClient(event)) => match event {
            relay::client::Event::ReservationReqAccepted { relay_peer_id, .. } => {
                info!("Relay slot reserved via {relay_peer_id}");
            }
            other => {
                tracing::debug!("Relay client event: {other:?}");
            }
        },

        // Relay server — log at debug only
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::RelayServer(event)) => {
            tracing::debug!("Relay server event: {event:?}");
        }

        // AutoNAT — log whenever NAT status changes; relay servers expose their
        // external address so peers can find them as relays via DHT.
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Autonat(
            autonat::Event::StatusChanged { new, .. },
        )) => {
            info!("NAT status: {new:?}");
            // Switch Kademlia mode based on NAT reachability.  NATted nodes
            // should not advertise themselves as DHT servers — queries to them
            // would have to traverse a relay, wasting resources.
            match &new {
                autonat::NatStatus::Public(_) => {
                    swarm
                        .behaviour_mut()
                        .kademlia
                        .set_mode(Some(kad::Mode::Server));
                }
                autonat::NatStatus::Private => {
                    if !ctx.relay_server {
                        swarm
                            .behaviour_mut()
                            .kademlia
                            .set_mode(Some(kad::Mode::Client));
                    }
                }
                _ => {}
            }
            if ctx.relay_server && matches!(new, autonat::NatStatus::Private) {
                warn!("--relay-server set but NAT detected — this node cannot relay for others; check port forwarding");
            }
        }

        // Address candidates are collected by DCUTR for hole-punching
        // automatically (via FromSwarm::NewExternalAddrCandidate).  We do NOT
        // confirm them here — AutoNAT is the authority for that.  Confirming
        // a NATted address would wrongly advertise it as reachable to other
        // protocols like Kademlia and Rendezvous.  The relay circuit address
        // (which *is* reachable) is confirmed by the relay client behaviour.
        SwarmEvent::NewExternalAddrCandidate { address } => {
            info!("DCUTR candidate address: {address}");
        }

        SwarmEvent::ExternalAddrConfirmed { address } => {
            if ctx.relay_server && !state.relay_banner_shown {
                // Strip any trailing /p2p component before appending our own,
                // since AutoNAT may already include it in the confirmed address.
                let mut base: Multiaddr = address
                    .iter()
                    .filter(|p| !matches!(p, libp2p::multiaddr::Protocol::P2p(_)))
                    .collect();
                base.push(libp2p::multiaddr::Protocol::P2p(ctx.local_peer_id));
                let full_addr = base.to_string();
                info!("╔══════════════════════════════════════════════════════╗");
                info!("║  RELAY SERVER READY — public address confirmed       ║");
                info!("║  {full_addr}");
                info!("╚══════════════════════════════════════════════════════╝");
                state.relay_banner_shown = true;
            } else if !ctx.relay_server {
                info!("External address confirmed: {address}");
            }

            // Trigger an immediate DHT announce so the peer is discoverable
            // as soon as it has a reachable address (relay circuit or public IP)
            // instead of waiting for the next 5-minute periodic tick.
            let _ = ctx.announce_tx.try_send(());

            // Rendezvous: now that we have an external address, register with
            // any known rendezvous servers we haven't registered with yet.
            if !ctx.relay_server {
                // Collect peer IDs first to avoid borrow conflict with swarm.
                let rz_peers: Vec<PeerId> = state
                    .rendezvous_servers
                    .iter()
                    .filter(|p| !state.rendezvous_registered.contains(p))
                    .copied()
                    .collect();
                for rz_peer in rz_peers {
                    let ns = clawshake_namespace();
                    match swarm
                        .behaviour_mut()
                        .rendezvous_client
                        .register(ns, rz_peer, None)
                    {
                        Ok(_) => {
                            info!("Rendezvous: registering with {rz_peer}");
                            state.rendezvous_registered.insert(rz_peer);
                        }
                        Err(e) => {
                            warn!("Rendezvous: register failed: {e:?}");
                        }
                    }
                }
            }
        }

        // DCUTR — hole punching
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Dcutr(dcutr::Event {
            remote_peer_id,
            result,
        })) => match result {
            Ok(conn_id) => info!("Hole punch succeeded with {remote_peer_id} (conn={conn_id:?})"),
            Err(e) => warn!("Hole punch failed with {remote_peer_id}: {e:?}"),
        },

        // UPnP — automatic port mapping
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Upnp(event)) => match event {
            upnp::Event::NewExternalAddr(addr) => {
                info!("UPnP: mapped external address {addr}");
                swarm.add_external_address(addr);
            }
            upnp::Event::GatewayNotFound => {
                tracing::debug!("UPnP: no gateway found (router may not support UPnP)");
            }
            upnp::Event::NonRoutableGateway => {
                tracing::debug!("UPnP: gateway is not routable");
            }
            upnp::Event::ExpiredExternalAddr(addr) => {
                info!("UPnP: mapping expired for {addr}");
                swarm.remove_external_address(&addr);
            }
        },

        // Rendezvous client — peer discovery and registration
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::RendezvousClient(event)) => match event {
            rendezvous::client::Event::Discovered {
                rendezvous_node,
                registrations,
                cookie,
            } => {
                if registrations.is_empty() {
                    tracing::debug!("Rendezvous: no new peers from {rendezvous_node}");
                } else {
                    info!(
                        "Rendezvous: discovered {} peer(s) from {rendezvous_node}",
                        registrations.len()
                    );
                }
                state.rendezvous_cookies.insert(rendezvous_node, cookie);
                for registration in registrations {
                    let peer_id = registration.record.peer_id();
                    if peer_id == ctx.local_peer_id {
                        continue; // skip ourselves
                    }
                    let addrs = registration.record.addresses();
                    info!(
                        "Rendezvous: found peer {peer_id} with {} addr(s)",
                        addrs.len()
                    );
                    for addr in addrs {
                        swarm
                            .behaviour_mut()
                            .kademlia
                            .add_address(&peer_id, addr.clone());
                    }
                    if let Err(e) = swarm.dial(peer_id) {
                        tracing::debug!("Rendezvous: skipped dial {peer_id}: {e}");
                    }
                }
            }
            rendezvous::client::Event::Registered {
                rendezvous_node,
                ttl,
                namespace,
            } => {
                info!("Rendezvous: registered at {rendezvous_node} ns={namespace} ttl={ttl}s");
            }
            rendezvous::client::Event::RegisterFailed {
                rendezvous_node,
                namespace,
                error,
            } => {
                warn!("Rendezvous: register failed at {rendezvous_node} ns={namespace}: {error:?}");
                // Allow retry on next ExternalAddrConfirmed
                state.rendezvous_registered.remove(&rendezvous_node);
            }
            rendezvous::client::Event::DiscoverFailed {
                rendezvous_node,
                error,
                ..
            } => {
                warn!("Rendezvous: discover failed at {rendezvous_node}: {error:?}");
            }
            rendezvous::client::Event::Expired { peer } => {
                info!("Rendezvous: registration expired at {peer}, re-registering");
                // Registration TTL lapsed — re-register so we stay visible.
                state.rendezvous_registered.remove(&peer);
                let ns = clawshake_namespace();
                match swarm
                    .behaviour_mut()
                    .rendezvous_client
                    .register(ns, peer, None)
                {
                    Ok(_) => {
                        state.rendezvous_registered.insert(peer);
                    }
                    Err(e) => {
                        warn!("Rendezvous: re-register failed: {e:?}");
                    }
                }
            }
        },

        // Rendezvous server — log registrations at info level
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::RendezvousServer(event)) => match event {
            rendezvous::server::Event::PeerRegistered { peer, registration } => {
                info!(
                    "Rendezvous server: {peer} registered ns={}",
                    registration.namespace
                );
            }
            rendezvous::server::Event::DiscoverServed {
                enquirer,
                registrations,
                ..
            } => {
                info!(
                    "Rendezvous server: served {} registration(s) to {enquirer}",
                    registrations.len()
                );
            }
            other => {
                tracing::debug!("Rendezvous server event: {other:?}");
            }
        },

        // Listener stopped \u{2014} a TCP or QUIC socket is no longer usable.
        SwarmEvent::ListenerClosed {
            addresses, reason, ..
        } => {
            warn!("Listener closed (addrs={addresses:?}): {reason:?}");
        }

        SwarmEvent::ListenerError { error, .. } => {
            warn!("Listener error: {error}");
        }

        SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
            tracing::debug!("Outgoing connection error (peer={peer_id:?}): {error:?}");
        }

        SwarmEvent::ExternalAddrExpired { address } => {
            info!("External address expired: {address}");
        }

        _ => {}
    }
}
