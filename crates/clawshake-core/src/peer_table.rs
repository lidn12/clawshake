use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

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

/// A model available on a remote peer.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelSummary {
    pub name: String,
    /// Maximum context length in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,
    /// Human-readable parameter count (e.g. "70B").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<String>,
}

/// A discovered peer node on the network.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub peer_id: String,
    /// Human-readable multiaddrs, e.g. "/ip4/192.168.1.5/tcp/7474"
    pub addrs: Vec<String>,
    /// Tools this peer has announced (name + description).
    pub tools: Vec<ToolSummary>,
    /// Models this peer serves (empty if no model server configured).
    pub models: Vec<ModelSummary>,
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
