use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use clawshake_core::{peer_table::PeerTable, permissions::PermissionStore};
use libp2p::futures::StreamExt;
use libp2p::{
    autonat, dcutr, identify, kad, mdns, noise, relay, request_response,
    swarm::{NetworkBehaviour, SwarmEvent},
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
    relay_server: relay::Behaviour,
    relay_client: relay::client::Behaviour,
    autonat: autonat::Behaviour,
    dcutr: dcutr::Behaviour,
}

// ---------------------------------------------------------------------------
// Well-known bootstrap peers
// ---------------------------------------------------------------------------

/// Hardcoded bootstrap peers dialed on startup when `--no-default-boot` is
/// not set.  Add the multiaddr printed by a dedicated Clawshake bootstrap
/// node (`--bootstrap-mode`) here once one is running.
///
/// Format: `/ip4/<ip>/tcp/7474/p2p/<peer-id>`
const BOOTSTRAP_PEERS: &[&str] =
    &["/ip4/43.143.33.106/tcp/7474/p2p/12D3KooWDi1ntKAkUYpHfijLNExUTsirFyofnkEB3yjC8P3EGcY5"];

/// Default port for bootstrap-mode nodes.
pub const BOOTSTRAP_DEFAULT_PORT: u16 = 7474;

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
    bootstrap_mode: bool,
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
        .with_dns()?
        .with_relay_client(noise::Config::new, yamux::Config::default)?
        .with_behaviour(|key, relay_client| {
            let peer_id = key.public().to_peer_id();

            let kad_protocol = StreamProtocol::try_from_owned("/clawshake/kad/1.0.0".to_string())
                .expect("valid protocol string");
            let mut kad_config = kad::Config::new(kad_protocol);
            kad_config.set_query_timeout(Duration::from_secs(30));
            let kademlia = kad::Behaviour::with_config(
                peer_id,
                kad::store::MemoryStore::new(peer_id),
                kad_config,
            );

            let identify = identify::Behaviour::new(identify::Config::new(
                "/clawshake/1.0.0".to_string(),
                key.public(),
            ));

            let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id)?;

            let proxy = proxy::new_behaviour();

            let relay_server = relay::Behaviour::new(peer_id, relay::Config::default());
            let autonat = autonat::Behaviour::new(peer_id, autonat::Config::default());
            let dcutr = dcutr::Behaviour::new(peer_id);

            Ok(ClawshakeBehaviour {
                kademlia,
                identify,
                mdns,
                proxy,
                relay_server,
                relay_client,
                autonat,
                dcutr,
            })
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

    let listen_addr: Multiaddr = format!("/ip4/0.0.0.0/tcp/{p2p_port}").parse()?;
    swarm.listen_on(listen_addr)?;

    swarm
        .behaviour_mut()
        .kademlia
        .set_mode(Some(kad::Mode::Server));

    // Build the complete list of bootstrap peers to dial on startup.
    // User-supplied --boot peers always take effect.
    // Hardcoded default peers are added unless --no-default-boot is set.
    let all_boot_peers: Vec<String> = {
        let mut peers = boot_peers.clone();
        if !no_default_boot {
            peers.extend(BOOTSTRAP_PEERS.iter().map(|s| s.to_string()));
        }
        peers
    };

    // Map relay peer-id → circuit-relay listen address.  Used in the event loop
    // to reserve a relay slot as soon as we connect to a known relay/bootstrap node.
    let relay_peers: std::collections::HashMap<PeerId, Multiaddr> = all_boot_peers
        .iter()
        .filter_map(|s| {
            let addr: Multiaddr = s.parse().ok()?;
            if let Some(libp2p::multiaddr::Protocol::P2p(peer_id)) = addr.iter().last() {
                let mut circuit = addr.clone();
                circuit.push(libp2p::multiaddr::Protocol::P2pCircuit);
                Some((peer_id, circuit))
            } else {
                None
            }
        })
        .collect();

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

    // -- Channels -----------------------------------------------------------

    // Async proxy responses: (response_channel, response_bytes)
    // The proxy handling task sends back here; main loop delivers to swarm.
    let (resp_tx, mut resp_rx) =
        mpsc::channel::<(request_response::ResponseChannel<Vec<u8>>, Vec<u8>)>(64);

    // DHT announce records from the background announce task.
    let (dht_tx, mut dht_rx) = mpsc::channel::<kad::Record>(4);

    // -- Announce task ------------------------------------------------------
    // If a backend is configured, query its tools/list on startup and then
    // every ANNOUNCE_INTERVAL seconds.
    const ANNOUNCE_INTERVAL: u64 = 300; // 5 minutes

    if let Some(ref b) = backend {
        let backend_clone = b.clone();
        let dht_tx_clone = dht_tx.clone();
        let peer_id_clone = local_peer_id;
        // Snapshot of current listen addrs (populated once the swarm starts
        // listening; we'll update on subsequent ticks via the event loop).
        // For the initial announce the list is empty — it gets populated after
        // the first NewListenAddr events.  A smarter approach (Milestone 3)
        // will collect real addrs before first announce.
        tokio::spawn(async move {
            let mut tick = interval(Duration::from_secs(ANNOUNCE_INTERVAL));
            loop {
                tick.tick().await;
                match announce::build_record(peer_id_clone, &[], &backend_clone).await {
                    Ok(record) => {
                        if dht_tx_clone.send(record).await.is_err() {
                            break; // main loop exited
                        }
                    }
                    Err(e) => warn!("Announce build failed: {e}"),
                }
            }
        });

        // Trigger the first announce immediately (don't wait 5 minutes).
        let backend_first = b.clone();
        let dht_tx_first = dht_tx.clone();
        tokio::spawn(async move {
            match announce::build_record(local_peer_id, &[], &backend_first).await {
                Ok(record) => {
                    let _ = dht_tx_first.send(record).await;
                }
                Err(e) => warn!("Initial announce failed: {e}"),
            }
        });
    }

    // -- Main event loop ----------------------------------------------------
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
                    // Reserve a relay slot when we first connect to a relay/bootstrap peer.
                    if !bootstrap_mode {
                        if let SwarmEvent::ConnectionEstablished { peer_id, .. } = &event {
                            if let Some(circuit_addr) = relay_peers.get(peer_id) {
                                info!("Reserving relay slot via {circuit_addr}");
                                if let Err(e) = swarm.listen_on(circuit_addr.clone()) {
                                    warn!("Failed to listen on relay addr {circuit_addr}: {e}");
                                }
                            }
                        }
                    }
                    handle_event(&mut swarm, event, &table, &connected, bootstrap_mode, local_peer_id);
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
        }
    }
}

// ---------------------------------------------------------------------------
// Event handler (non-proxy events)
// ---------------------------------------------------------------------------

fn handle_event(
    swarm: &mut libp2p::Swarm<ClawshakeBehaviour>,
    event: SwarmEvent<ClawshakeBehaviourEvent>,
    table: &PeerTable,
    connected: &ConnectedPeers,
    bootstrap_mode: bool,
    local_peer_id: PeerId,
) {
    match event {
        SwarmEvent::NewListenAddr { ref address, .. } => {
            info!("Listening on {address}");
            if bootstrap_mode {
                let full_addr = format!("{}/p2p/{}", address, local_peer_id);
                info!("╔══════════════════════════════════════════════════════╗");
                info!("║         BOOTSTRAP NODE READY                        ║");
                info!("║                                                      ║");
                info!("║  Bind addr (may be internal if behind NAT):         ║");
                info!("║  {full_addr}");
                info!("║                                                      ║");
                info!("║  If behind cloud/router NAT, use your public IP:    ║");
                info!("║  /ip4/<public-ip>/tcp/7474/p2p/{local_peer_id}");
                info!("║                                                      ║");
                info!("╚══════════════════════════════════════════════════════╝");
            }
        }

        SwarmEvent::ConnectionEstablished {
            peer_id, endpoint, ..
        } => {
            info!(
                "Connected to {peer_id} ({})",
                if endpoint.is_dialer() {
                    "outbound"
                } else {
                    "inbound"
                }
            );
            // Track as reachable for network.ping
            connected
                .write()
                .expect("connected peers lock poisoned")
                .insert(peer_id.to_string());
            // Fetch the peer's DHT announcement to populate the peer table.
            let key = kad::RecordKey::new(&peer_id.to_bytes());
            swarm.behaviour_mut().kademlia.get_record(key);
        }

        SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
            info!("Disconnected from {peer_id}: {cause:?}");
            connected
                .write()
                .expect("connected peers lock poisoned")
                .remove(&peer_id.to_string());
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
                "Identified {peer_id}: agent=\"{}\" protocol=\"{}\"",
                info.agent_version, info.protocol_version
            );
            for addr in info.listen_addrs {
                swarm.behaviour_mut().kademlia.add_address(&peer_id, addr);
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
                info!("Kademlia routing table updated: peer={peer}");
            }
            kad::Event::RoutablePeer { peer, address } => {
                info!("Kademlia routable peer: {peer} at {address}");
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
                    table.upsert(info);
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

        // AutoNAT — log whenever NAT status changes
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Autonat(
            autonat::Event::StatusChanged { new, .. },
        )) => {
            info!("NAT status: {new:?}");
        }

        // DCUTR — hole punching
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Dcutr(dcutr::Event {
            remote_peer_id,
            result,
        })) => match result {
            Ok(_) => info!("Hole punch succeeded with {remote_peer_id}"),
            Err(e) => warn!("Hole punch failed with {remote_peer_id}: {e:?}"),
        },

        _ => {}
    }
}
