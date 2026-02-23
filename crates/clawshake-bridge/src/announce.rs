//! DHT tool announcement.
//!
//! Each bridge node publishes a `kad::Record` under `key = peer_id.to_bytes()`
//! containing a JSON announcement of its available tools and listen addresses.
//! The record is published on startup and refreshed every 5 minutes so it
//! does not expire from the DHT.
//!
//! Any other node that knows a peer's ID can do a DHT GET with the same key
//! to discover its tool list — this is how `network.tools(peer_id)` will work.

use anyhow::Result;
use libp2p::{kad, Multiaddr, PeerId};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::backend::McpBackend;

/// The value stored in the DHT for each clawshake-bridge node.
#[derive(Debug, Serialize, Deserialize)]
pub struct AnnouncementRecord {
    /// Schema version, always 1 for now.
    pub v: u8,
    /// String form of the publishing peer's PeerId.
    pub peer_id: String,
    /// Qualified tool names exposed by this node (e.g. `"spotify.play"`).
    pub tools: Vec<String>,
    /// Listen multiaddrs for direct connections.
    pub addrs: Vec<String>,
    /// Unix timestamp (seconds) when this record was built.
    pub ts: u64,
}

impl AnnouncementRecord {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("AnnouncementRecord serialization cannot fail")
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

/// Query the backend's `tools/list` and build a Kademlia record ready for
/// `kad::Behaviour::put_record`.
pub async fn build_record(
    peer_id: PeerId,
    listen_addrs: &[Multiaddr],
    backend: &McpBackend,
) -> Result<kad::Record> {
    let tools = backend.tools_list().await?;
    let tool_names: Vec<String> = tools
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();

    let count = tool_names.len();

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let record = AnnouncementRecord {
        v: 1,
        peer_id: peer_id.to_string(),
        tools: tool_names,
        addrs: listen_addrs.iter().map(|a| a.to_string()).collect(),
        ts,
    };

    info!(tools = count, peer = %peer_id, "Built DHT announcement");

    Ok(kad::Record {
        key: kad::RecordKey::new(&peer_id.to_bytes()),
        value: record.to_bytes(),
        publisher: Some(peer_id),
        expires: None,
    })
}
