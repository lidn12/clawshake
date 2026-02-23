use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use libp2p::futures::StreamExt;
use libp2p::{
    identify, kad, mdns, noise,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, Multiaddr, PeerId, StreamProtocol, SwarmBuilder,
};
use tokio::select;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Composite behaviour
// ---------------------------------------------------------------------------

#[derive(NetworkBehaviour)]
struct ClawshakeBehaviour {
    kademlia: kad::Behaviour<kad::store::MemoryStore>,
    identify: identify::Behaviour,
    mdns: mdns::tokio::Behaviour,
}

// ---------------------------------------------------------------------------
// Keypair persistence
// ---------------------------------------------------------------------------

/// Load the node keypair from the given path (or `~/.clawshake/identity.key`
/// by default), generating and saving a new Ed25519 keypair if absent.
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
) -> Result<()> {
    let keypair = load_or_create_keypair(identity.as_deref())?;
    let local_peer_id = PeerId::from(&keypair.public());
    info!("Local peer ID: {local_peer_id}");
    info!("Bootstrap this node with: /ip4/127.0.0.1/tcp/{p2p_port}/p2p/{local_peer_id}");

    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|key| {
            let peer_id = key.public().to_peer_id();

            // Kademlia — use server mode so we participate in the DHT actively.
            let kad_protocol = StreamProtocol::try_from_owned("/clawshake/kad/1.0.0".to_string())
                .expect("valid protocol string");
            let mut kad_config = kad::Config::new(kad_protocol);
            kad_config.set_query_timeout(Duration::from_secs(30));
            let kademlia = kad::Behaviour::with_config(
                peer_id,
                kad::store::MemoryStore::new(peer_id),
                kad_config,
            );

            // Identify — lets peers learn our listen addresses.
            let identify = identify::Behaviour::new(identify::Config::new(
                "/clawshake/1.0.0".to_string(),
                key.public(),
            ));

            // mDNS — zero-config local peer discovery (works on the same LAN).
            let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id)?;

            Ok(ClawshakeBehaviour {
                kademlia,
                identify,
                mdns,
            })
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

    // Start listening.
    let listen_addr: Multiaddr = format!("/ip4/0.0.0.0/tcp/{p2p_port}").parse()?;
    swarm.listen_on(listen_addr)?;

    // Set Kademlia to server mode so we can accept incoming queries.
    swarm
        .behaviour_mut()
        .kademlia
        .set_mode(Some(kad::Mode::Server));

    // Dial explicit bootstrap peers (e.g. --boot /ip4/127.0.0.1/tcp/7474/p2p/<peer-id>).
    for addr_str in &boot_peers {
        match addr_str.parse::<Multiaddr>() {
            Ok(addr) => {
                // Extract the PeerId from the trailing /p2p/<id> component so we can
                // pre-populate the Kademlia routing table with the transport address.
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

    // Main event loop.
    loop {
        select! {
            event = swarm.select_next_some() => {
                handle_event(&mut swarm, event);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Event handler
// ---------------------------------------------------------------------------

fn handle_event(
    swarm: &mut libp2p::Swarm<ClawshakeBehaviour>,
    event: SwarmEvent<ClawshakeBehaviourEvent>,
) {
    match event {
        // ── Swarm ──────────────────────────────────────────────────────────
        SwarmEvent::NewListenAddr { address, .. } => {
            info!("Listening on {address}");
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
        }

        SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
            info!("Disconnected from {peer_id}: {cause:?}");
        }

        // ── mDNS — local peer discovery ────────────────────────────────────
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

        // ── Identify — learn peer addresses ───────────────────────────────
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Identify(identify::Event::Received {
            peer_id,
            info,
            ..
        })) => {
            info!(
                "Identified {peer_id}: agent=\"{}\" protocol_version=\"{}\"",
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

        // ── Kademlia ───────────────────────────────────────────────────────
        SwarmEvent::Behaviour(ClawshakeBehaviourEvent::Kademlia(event)) => match event {
            kad::Event::RoutingUpdated { peer, .. } => {
                info!("Kademlia routing table updated: peer={peer}");
            }
            kad::Event::RoutablePeer { peer, address } => {
                info!("Kademlia routable peer: {peer} at {address}");
            }
            other => {
                tracing::debug!("Kademlia event: {other:?}");
            }
        },

        _ => {}
    }
}
