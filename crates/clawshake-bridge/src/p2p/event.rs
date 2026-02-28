//! Per-protocol event handlers for the libp2p swarm.
//!
//! `handle_event` is the thin dispatcher called from the main event loop in
//! `mod.rs`.  Each protocol (Identify, Kademlia, Rendezvous, etc.) has its
//! own handler function so the match arms stay short and independently
//! readable.

use std::collections::{HashMap, HashSet};

use super::{addr, ClawshakeBehaviour, ClawshakeBehaviourEvent};
use crate::announce;
use clawshake_core::{network_channel::ConnectedPeers, peer_table::PeerTable};
use libp2p::{
    autonat, dcutr, identify, kad, mdns, relay, rendezvous, request_response,
    swarm::{ConnectionId, SwarmEvent},
    upnp, Multiaddr, PeerId,
};
use std::sync::RwLock;
use tokio::sync::mpsc;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Protocol constants
// ---------------------------------------------------------------------------

/// libp2p relay v2 hop protocol — advertised by nodes that can relay traffic.
const RELAY_HOP_PROTOCOL: &str = "/libp2p/circuit/relay/0.2.0/hop";

/// Rendezvous protocol — advertised by nodes running a rendezvous server.
const RENDEZVOUS_PROTOCOL: &str = "/rendezvous/1.0.0";

/// Namespace used for clawshake peer discovery via rendezvous.
const RENDEZVOUS_NAMESPACE: &str = "clawshake";

/// Creates a `Namespace` for the clawshake rendezvous point.
pub(super) fn clawshake_namespace() -> rendezvous::Namespace {
    rendezvous::Namespace::new(RENDEZVOUS_NAMESPACE.to_string()).expect("valid namespace")
}

// ---------------------------------------------------------------------------
// State types
// ---------------------------------------------------------------------------

/// Mutable state tracked across the main event loop.
pub(super) struct NodeState {
    pub relay_banner_shown: bool,
    pub relay_reserved_peers: HashSet<PeerId>,
    pub rendezvous_cookies: HashMap<PeerId, rendezvous::Cookie>,
    pub rendezvous_registered: HashSet<PeerId>,
    pub rendezvous_servers: HashSet<PeerId>,
    /// Track each connection and whether it goes through a relay.
    /// Used to prefer direct connections over relay and to re-establish
    /// relay paths when direct connections drop.
    pub peer_connections: HashMap<PeerId, Vec<(ConnectionId, bool)>>,
}

/// Immutable context shared across event handling — avoids passing many
/// individual parameters through every handler function.
pub(super) struct EventContext<'a> {
    pub table: &'a PeerTable,
    pub connected: &'a ConnectedPeers,
    pub relay_server: bool,
    pub local_peer_id: PeerId,
    pub listen_addrs: &'a RwLock<Vec<Multiaddr>>,
    /// Poked on `ExternalAddrConfirmed` so the announce task re-publishes
    /// immediately with the newly reachable address.
    pub announce_tx: &'a mpsc::Sender<()>,
}

// ---------------------------------------------------------------------------
// Main dispatcher
// ---------------------------------------------------------------------------

pub(super) fn handle_event(
    swarm: &mut libp2p::Swarm<ClawshakeBehaviour>,
    event: SwarmEvent<ClawshakeBehaviourEvent>,
    ctx: &EventContext<'_>,
    state: &mut NodeState,
) {
    match event {
        SwarmEvent::NewListenAddr { ref address, .. } => {
            info!("Listening on {address}");
            let mut addrs = ctx.listen_addrs.write().expect("listen_addrs lock");
            if !addrs.contains(address) {
                addrs.push(address.clone());
            }
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
            connection_id,
            ref endpoint,
            num_established,
            ..
        } => {
            on_connection_established(
                swarm,
                peer_id,
                connection_id,
                endpoint,
                num_established,
                ctx,
                state,
            );
        }

        SwarmEvent::ConnectionClosed {
            peer_id,
            connection_id,
            cause,
            num_established,
            ..
        } => {
            on_connection_closed(
                swarm, peer_id, connection_id, cause, num_established, ctx, state,
            );
        }

        // -- mDNS ----------------------------------------------------------
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

        // -- Identify ------------------------------------------------------
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Identify(
            identify::Event::Received { peer_id, info, .. },
        )) => {
            on_identify_received(swarm, peer_id, info, ctx, state);
        }

        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Identify(identify::Event::Error {
            peer_id,
            error,
            ..
        })) => {
            warn!("Identify error for {peer_id}: {error}");
        }

        // -- Kademlia ------------------------------------------------------
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Kademlia(event)) => {
            on_kademlia(event, ctx);
        }

        // -- Proxy residual (InboundFailure only — request/response/outbound
        //    are handled inline in the main event loop) --------------------
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Proxy(event)) => match event {
            request_response::Event::InboundFailure { peer, error, .. } => {
                warn!("MCP inbound failure from {peer}: {error}");
            }
            _ => {}
        },

        // -- Relay client --------------------------------------------------
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::RelayClient(event)) => match event {
            relay::client::Event::ReservationReqAccepted {
                relay_peer_id, ..
            } => {
                info!("Relay slot reserved via {relay_peer_id}");
            }
            other => {
                info!("Relay client event: {other:?}");
            }
        },

        // -- Relay server --------------------------------------------------
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::RelayServer(event)) => {
            info!("Relay server event: {event:?}");
        }

        // -- AutoNAT -------------------------------------------------------
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Autonat(
            autonat::Event::StatusChanged { new, .. },
        )) => {
            on_autonat_status_changed(swarm, &new, ctx);
        }

        // -- External address lifecycle ------------------------------------

        // Candidates are collected by DCUTR for hole-punching automatically.
        // NOT confirmed here — AutoNAT is the authority for that.  Confirming
        // a NATted address would wrongly advertise it as reachable to other
        // protocols like Kademlia and Rendezvous.
        SwarmEvent::NewExternalAddrCandidate { address } => {
            info!("DCUTR candidate address: {address}");
        }

        SwarmEvent::ExternalAddrConfirmed { address } => {
            on_external_addr_confirmed(swarm, &address, ctx, state);
        }

        SwarmEvent::ExternalAddrExpired { address } => {
            info!("External address expired: {address}");
        }

        // -- DCUTR ---------------------------------------------------------
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Dcutr(dcutr::Event {
            remote_peer_id,
            result,
        })) => match result {
            Ok(conn_id) => {
                info!("Hole punch succeeded with {remote_peer_id} (conn={conn_id:?})")
            }
            Err(e) => warn!("Hole punch failed with {remote_peer_id}: {e:?}"),
        },

        // -- UPnP ----------------------------------------------------------
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Upnp(event)) => match event {
            upnp::Event::NewExternalAddr(a) => {
                info!("UPnP: mapped external address {a}");
                swarm.add_external_address(a);
            }
            upnp::Event::GatewayNotFound => {
                info!("UPnP: no gateway found (router may not support UPnP)");
            }
            upnp::Event::NonRoutableGateway => {
                info!("UPnP: gateway is not routable");
            }
            upnp::Event::ExpiredExternalAddr(a) => {
                info!("UPnP: mapping expired for {a}");
                swarm.remove_external_address(&a);
            }
        },

        // -- Rendezvous client ---------------------------------------------
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::RendezvousClient(event)) => {
            on_rendezvous_client(swarm, event, ctx, state);
        }

        // -- Rendezvous server ---------------------------------------------
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

        // -- Listener lifecycle --------------------------------------------
        SwarmEvent::ListenerClosed {
            addresses, reason, ..
        } => {
            warn!("Listener closed (addrs={addresses:?}): {reason:?}");
        }

        SwarmEvent::ListenerError { error, .. } => {
            warn!("Listener error: {error}");
        }

        SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
            info!("Outgoing connection error (peer={peer_id:?}): {error:?}");
        }

        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Per-protocol handlers
// ---------------------------------------------------------------------------

/// New connection established — classify (direct vs relay), scrub Kademlia,
/// track for connection-preference logic, fetch DHT record on first contact.
fn on_connection_established(
    swarm: &mut libp2p::Swarm<ClawshakeBehaviour>,
    peer_id: PeerId,
    connection_id: ConnectionId,
    endpoint: &libp2p::core::ConnectedPoint,
    num_established: std::num::NonZeroU32,
    ctx: &EventContext<'_>,
    state: &mut NodeState,
) {
    let remote_addr = endpoint.get_remote_address();

    // Classify the connection as relay or direct.
    let has_circuit = remote_addr
        .iter()
        .any(|p| matches!(p, libp2p::multiaddr::Protocol::P2pCircuit));
    // A bare `/p2p/<id>` inbound address (no IP/transport layer) comes from a
    // relay circuit that libp2p doesn't tag with P2pCircuit.
    let is_relayed = has_circuit || !addr::has_transport(remote_addr);

    let via = if is_relayed {
        if endpoint.is_dialer() {
            "outbound-relay"
        } else {
            "inbound-relay"
        }
    } else if endpoint.is_dialer() {
        "outbound"
    } else {
        "inbound"
    };
    info!("Connected to {peer_id} ({via}) addr={remote_addr}");

    // Kademlia's internal handler adds the endpoint address to the routing
    // table.  For relay connections this is a bare `/p2p/<id>` or circuit
    // path — neither is directly dialable.  Scrub immediately.
    if is_relayed {
        swarm
            .behaviour_mut()
            .kademlia
            .remove_address(&peer_id, remote_addr);
    }

    // Track connection type for preference logic.
    state
        .peer_connections
        .entry(peer_id)
        .or_default()
        .push((connection_id, is_relayed));

    // Connection preference: direct beats relay — close relay connections
    // when a direct path is established to free relay slots.
    if !is_relayed {
        if let Some(conns) = state.peer_connections.get(&peer_id) {
            let relay_conns: Vec<ConnectionId> = conns
                .iter()
                .filter(|(cid, relayed)| *relayed && *cid != connection_id)
                .map(|(cid, _)| *cid)
                .collect();
            for cid in &relay_conns {
                info!("Closing relay connection {cid:?} to {peer_id} (direct path available)");
                let _ = swarm.close_connection(*cid);
            }
        }
    }

    // Track as reachable for network_ping.
    ctx.connected
        .write()
        .expect("connected peers lock poisoned")
        .insert(peer_id.to_string());

    // Only fetch DHT record on the *first* connection (DCUTR upgrades create
    // extra connections to the same peer; re-querying wastes bandwidth).
    if num_established.get() == 1 {
        let key = kad::RecordKey::new(&peer_id.to_bytes());
        swarm.behaviour_mut().kademlia.get_record(key);
    }
}

/// Connection closed — update tracking, re-establish relay fallback if a
/// direct connection was lost, clean up state on last-connection-close.
fn on_connection_closed(
    swarm: &mut libp2p::Swarm<ClawshakeBehaviour>,
    peer_id: PeerId,
    connection_id: ConnectionId,
    cause: Option<libp2p::swarm::ConnectionError>,
    num_established: u32,
    ctx: &EventContext<'_>,
    state: &mut NodeState,
) {
    // Check if the closing connection was direct (non-relay).
    let was_direct = state
        .peer_connections
        .get(&peer_id)
        .and_then(|conns| conns.iter().find(|(cid, _)| *cid == connection_id))
        .map(|(_, relayed)| !relayed)
        .unwrap_or(false);

    // Remove this connection from tracking.
    if let Some(conns) = state.peer_connections.get_mut(&peer_id) {
        conns.retain(|(cid, _)| *cid != connection_id);
        if conns.is_empty() {
            state.peer_connections.remove(&peer_id);
        }
    }

    info!("Disconnected from {peer_id}: {cause:?} (remaining={num_established})");

    // Re-establish relay path: if a *direct* connection dropped and no
    // connections remain, try re-dialing the peer so the relay client can
    // re-establish the circuit.
    if was_direct
        && num_established == 0
        && !ctx.relay_server
        && peer_id != ctx.local_peer_id
    {
        info!("Direct connection to {peer_id} lost, re-dialing for relay fallback");
        let _ = swarm.dial(peer_id);
    }

    // Only clean up when the *last* connection to this peer closes.
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

/// Identify::Received — the richest handler.  When we learn a peer's
/// advertised addresses and supported protocols, we:
///
/// 1. Confirm external address for relay servers (AutoNAT is too slow)
/// 2. Add publicly routable addresses to Kademlia
/// 3. Scrub non-dialable addresses from the routing table
/// 4. Proactively direct-dial private LAN addresses (relay upgrade)
/// 5. Discover + register via Rendezvous (if the peer is a server)
/// 6. Auto-reserve a relay circuit slot (if the peer advertises relay hop)
fn on_identify_received(
    swarm: &mut libp2p::Swarm<ClawshakeBehaviour>,
    peer_id: PeerId,
    info: identify::Info,
    ctx: &EventContext<'_>,
    state: &mut NodeState,
) {
    info!(
        "Identified {peer_id}: agent=\"{}\" protocol=\"{}\" observed_addr={}",
        info.agent_version, info.protocol_version, info.observed_addr
    );

    // --- 1. Relay servers confirm external address immediately -------------
    // NOTE: Regular (NATted) nodes must NOT call add_external_address here —
    // Identify internally emits NewExternalAddrCandidate and AutoNAT is the
    // authority for confirming it.  Confirming prematurely blocks DCUTR.
    //
    // Relay servers are the exception: --relay-server asserts public
    // reachability, and AutoNAT's 15s boot delay is too slow for the first
    // RESERVE request.
    if ctx.relay_server && addr::is_public_addr(&info.observed_addr) {
        swarm.add_external_address(info.observed_addr.clone());
    }

    // --- 2. Add publicly routable listen addrs to Kademlia -----------------
    for a in info.listen_addrs.iter().filter(|a| addr::is_public_addr(a)) {
        swarm
            .behaviour_mut()
            .kademlia
            .add_address(&peer_id, a.clone());
    }

    // --- 3. Scrub non-dialable addresses from Kademlia ---------------------
    // libp2p's built-in Identify handler adds ALL listen_addrs to the routing
    // table.  Loopback, unspecified, and bare `/p2p/<id>` entries cause
    // WrongPeerId / MultiaddrNotSupported errors when dialed.
    for a in &info.listen_addrs {
        if !addr::has_transport(a) || addr::has_bad_ip(a) {
            swarm
                .behaviour_mut()
                .kademlia
                .remove_address(&peer_id, a);
        }
    }

    // --- 4. Proactive direct dial ------------------------------------------
    // When Identify arrives over a relay (observed_addr has no IP component),
    // try the peer's private LAN addresses for a direct path — without
    // waiting for DCUTR's relay timeout.
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
                !addr::is_public_addr(a)
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
            info!(
                "Attempting direct dial to {peer_id} via {} private addr(s)",
                private_addrs.len()
            );
            let opts = libp2p::swarm::dial_opts::DialOpts::peer_id(peer_id)
                .addresses(private_addrs)
                .condition(libp2p::swarm::dial_opts::PeerCondition::Always)
                .build();
            if let Err(e) = swarm.dial(opts) {
                info!("Direct dial to {peer_id} failed: {e}");
            }
        }
    }

    // --- 5. Rendezvous discover + register ---------------------------------
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

        // Also try to register — will fail with NoExternalAddresses if we
        // don't have any yet; retry happens on ExternalAddrConfirmed.
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
                    info!("Rendezvous: register deferred (no external addrs yet): {e:?}");
                }
            }
        }
    }

    // --- 6. Auto-relay reservation -----------------------------------------
    if !ctx.relay_server
        && !state.relay_reserved_peers.contains(&peer_id)
        && info
            .protocols
            .iter()
            .any(|p| p.as_ref() == RELAY_HOP_PROTOCOL)
    {
        // Prefer a publicly routable listen address for the circuit base;
        // fall back to any non-loopback, non-circuit address.
        let circuit_base = info
            .listen_addrs
            .iter()
            .find(|a| addr::is_public_addr(a))
            .or_else(|| {
                info.listen_addrs.iter().find(|a| {
                    !a.iter().any(|p| {
                        matches!(p, libp2p::multiaddr::Protocol::Ip4(ip) if ip.is_loopback())
                            || matches!(p, libp2p::multiaddr::Protocol::P2pCircuit)
                    })
                })
            });
        if let Some(base) = circuit_base {
            // Strip any trailing /p2p, then append /p2p/<relay>/p2p-circuit.
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

/// Kademlia query results — routing updates, record publish confirmations,
/// and fetched peer announcement records.
fn on_kademlia(event: kad::Event, ctx: &EventContext<'_>) {
    match event {
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
    }
}

/// AutoNAT status change — switch Kademlia mode (Server when public, Client
/// when NATted) and warn relay servers that appear to be behind NAT.
fn on_autonat_status_changed(
    swarm: &mut libp2p::Swarm<ClawshakeBehaviour>,
    new: &autonat::NatStatus,
    ctx: &EventContext<'_>,
) {
    info!("NAT status: {new:?}");
    match new {
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

/// External address confirmed — show relay banner, trigger immediate DHT
/// announce, and register with any pending rendezvous servers.
fn on_external_addr_confirmed(
    swarm: &mut libp2p::Swarm<ClawshakeBehaviour>,
    address: &Multiaddr,
    ctx: &EventContext<'_>,
    state: &mut NodeState,
) {
    if ctx.relay_server && !state.relay_banner_shown {
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

    // Trigger an immediate DHT announce so the peer is discoverable as soon
    // as it has a reachable address instead of waiting for the 5-min tick.
    let _ = ctx.announce_tx.try_send(());

    // Rendezvous: now that we have an external address, register with any
    // known rendezvous servers we haven't registered with yet.
    if !ctx.relay_server {
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

/// Rendezvous client events — peer discovery, registration lifecycle.
fn on_rendezvous_client(
    swarm: &mut libp2p::Swarm<ClawshakeBehaviour>,
    event: rendezvous::client::Event,
    ctx: &EventContext<'_>,
    state: &mut NodeState,
) {
    match event {
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
                // Only add globally-reachable addresses (public IPs and relay
                // circuits).  Private/loopback addresses from the remote
                // node's listen set would cause WrongPeerId errors.
                for a in addrs {
                    if addr::is_globally_reachable(a) {
                        swarm
                            .behaviour_mut()
                            .kademlia
                            .add_address(&peer_id, a.clone());
                    } else {
                        tracing::debug!(
                            "Rendezvous: skipped non-routable addr for {peer_id}: {a}"
                        );
                    }
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
            info!(
                "Rendezvous: registered at {rendezvous_node} ns={namespace} ttl={ttl}s"
            );
        }
        rendezvous::client::Event::RegisterFailed {
            rendezvous_node,
            namespace,
            error,
        } => {
            warn!(
                "Rendezvous: register failed at {rendezvous_node} ns={namespace}: {error:?}"
            );
            // Allow retry on next ExternalAddrConfirmed.
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
    }
}
