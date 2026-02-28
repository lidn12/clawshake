//! Multiaddr classification and filtering utilities.
//!
//! These helpers determine which addresses are useful for DHT announcements,
//! Kademlia routing, and proactive direct-dial attempts.  Centralising them
//! here eliminates the duplicate filter logic that previously appeared in both
//! the periodic and event-driven announce tasks.

use libp2p::multiaddr::Protocol;
use libp2p::Multiaddr;
use std::sync::RwLock;

// ---------------------------------------------------------------------------
// Address classification
// ---------------------------------------------------------------------------

/// Returns true if `addr` contains a publicly routable IP — i.e. not loopback,
/// link-local, RFC-1918 private, or a relay circuit address.  Used to select
/// relay circuit base addresses and as a building block for
/// [`is_globally_reachable`].
pub(super) fn is_public_addr(addr: &Multiaddr) -> bool {
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
pub(super) fn is_globally_reachable(addr: &Multiaddr) -> bool {
    if addr.iter().any(|p| matches!(p, Protocol::P2pCircuit)) {
        return true;
    }
    is_public_addr(addr)
}

/// For a relay circuit multiaddr (`.../p2p/<relay>/p2p-circuit/...`), returns
/// the relay peer's raw multihash bytes — used to keep exactly one circuit
/// address per relay when deduplicating the announce address list.
/// Returns `None` for non-circuit addresses.
pub(super) fn relay_circuit_key(addr: &Multiaddr) -> Option<Vec<u8>> {
    let mut last_peer: Option<Vec<u8>> = None;
    for p in addr.iter() {
        match p {
            Protocol::P2p(h) => last_peer = Some(h.to_bytes()),
            Protocol::P2pCircuit => return last_peer,
            _ => {}
        }
    }
    None
}

/// Returns true if the address has a transport-layer component (IP or DNS).
/// Addresses without one (e.g. bare `/p2p/<id>`) are not directly dialable.
pub(super) fn has_transport(addr: &Multiaddr) -> bool {
    addr.iter().any(|p| {
        matches!(
            p,
            Protocol::Ip4(_)
                | Protocol::Ip6(_)
                | Protocol::Dns(_)
                | Protocol::Dns4(_)
                | Protocol::Dns6(_)
        )
    })
}

/// Returns true if the address contains a loopback or unspecified IP.
/// These addresses resolve to our own host and cause `WrongPeerId` errors
/// when dialed for a remote peer.
pub(super) fn has_bad_ip(addr: &Multiaddr) -> bool {
    addr.iter().any(|p| match p {
        Protocol::Ip4(ip) => ip.is_loopback() || ip.is_unspecified(),
        Protocol::Ip6(ip) => ip.is_loopback() || ip.is_unspecified(),
        _ => false,
    })
}

// ---------------------------------------------------------------------------
// Announce address filtering
// ---------------------------------------------------------------------------

/// Filter and deduplicate addresses for a DHT announcement — keep only
/// globally reachable addresses and at most one relay circuit per relay peer.
pub(super) fn dedup_announce_addrs(raw: &RwLock<Vec<Multiaddr>>) -> Vec<Multiaddr> {
    let raw = raw.read().expect("listen_addrs lock");
    let mut seen_relays = std::collections::HashSet::<Vec<u8>>::new();
    raw.iter()
        .filter(|a| is_globally_reachable(a))
        .filter(|a| match relay_circuit_key(a) {
            Some(key) => seen_relays.insert(key),
            None => true,
        })
        .cloned()
        .collect()
}
