use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// A discovered peer node on the network.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub peer_id: String,
    /// Human-readable multiaddrs, e.g. "/ip4/192.168.1.5/tcp/7474"
    pub addrs: Vec<String>,
    /// Tool names this peer has announced.
    pub tools: Vec<String>,
    /// Whether this peer was found via libp2p DHT or Tailscale.
    pub source: PeerSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerSource {
    Libp2p,
    Tailscale,
}

/// Shared in-memory table of discovered peers.
/// Both the DHT layer and the Tailscale layer write into this;
/// the invoke router reads from it — neither knows which layer populated it.
#[derive(Debug, Clone, Default)]
pub struct PeerTable {
    inner: Arc<RwLock<HashMap<String, PeerInfo>>>,
}

impl PeerTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert(&self, info: PeerInfo) {
        let mut map = self.inner.write().expect("peer table lock poisoned");
        map.insert(info.peer_id.clone(), info);
    }

    pub fn remove(&self, peer_id: &str) {
        let mut map = self.inner.write().expect("peer table lock poisoned");
        map.remove(peer_id);
    }

    pub fn all(&self) -> Vec<PeerInfo> {
        let map = self.inner.read().expect("peer table lock poisoned");
        map.values().cloned().collect()
    }

    pub fn get(&self, peer_id: &str) -> Option<PeerInfo> {
        let map = self.inner.read().expect("peer table lock poisoned");
        map.get(peer_id).cloned()
    }

    pub fn len(&self) -> usize {
        self.inner.read().expect("peer table lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
