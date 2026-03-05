use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::models::ModelAnnounce;

/// A name + description pair for a single tool on a remote peer.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolSummary {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Full JSON Schema for the tool's input parameters, if available.
    #[serde(
        rename = "inputSchema",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub input_schema: Option<Value>,
}

/// A discovered peer node on the network.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub peer_id: String,
    /// Human-readable description of the peer, e.g. "work laptop".
    /// `None` when the remote node hasn't set a description in config.
    pub description: Option<String>,
    /// Human-readable multiaddrs, e.g. "/ip4/192.168.1.5/tcp/7474"
    pub addrs: Vec<String>,
    /// Tools this peer has announced (name + description).
    pub tools: Vec<ToolSummary>,
    /// Models this peer serves (empty if no model server configured).
    pub models: Vec<ModelAnnounce>,
    /// Whether this peer was found via libp2p DHT or Tailscale.
    pub source: PeerSource,
    /// Unix timestamp (seconds) when this record was last observed.
    pub last_seen: u64,
    /// The raw DHT announcement JSON, if this peer came from the DHT.
    /// `None` for peers discovered via mDNS or other non-DHT sources.
    pub raw_record: Option<Value>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_peer(id: &str, tool_count: usize) -> PeerInfo {
        PeerInfo {
            peer_id: id.to_string(),
            description: None,
            addrs: vec!["/ip4/127.0.0.1/tcp/7474".to_string()],
            tools: (0..tool_count)
                .map(|i| ToolSummary {
                    name: format!("tool_{i}"),
                    description: String::new(),
                    input_schema: None,
                })
                .collect(),
            models: vec![],
            source: PeerSource::Libp2p,
            last_seen: 0,
            raw_record: None,
        }
    }

    #[test]
    fn upsert_and_get() {
        let table = PeerTable::new();
        table.upsert(make_peer("peer1", 2));
        let info = table.get("peer1").unwrap();
        assert_eq!(info.peer_id, "peer1");
        assert_eq!(info.tools.len(), 2);
        assert_eq!(info.addrs, ["/ip4/127.0.0.1/tcp/7474"]);
    }

    #[test]
    fn upsert_replaces() {
        let table = PeerTable::new();
        table.upsert(make_peer("peer1", 1));
        table.upsert(make_peer("peer1", 3));
        assert_eq!(table.get("peer1").unwrap().tools.len(), 3);
    }

    #[test]
    fn remove() {
        let table = PeerTable::new();
        table.upsert(make_peer("peer1", 1));
        table.remove("peer1");
        assert!(table.get("peer1").is_none());
    }

    #[test]
    fn all_returns_all_peers() {
        let table = PeerTable::new();
        table.upsert(make_peer("a", 0));
        table.upsert(make_peer("b", 0));
        table.upsert(make_peer("c", 0));
        assert_eq!(table.all().len(), 3);
    }

    #[test]
    fn len_and_is_empty() {
        let table = PeerTable::new();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        table.upsert(make_peer("peer1", 0));
        assert!(!table.is_empty());
        assert_eq!(table.len(), 1);
    }
}
