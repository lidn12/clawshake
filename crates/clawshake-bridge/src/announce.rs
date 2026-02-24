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
use clawshake_core::peer_table::{PeerInfo, PeerSource, ToolSummary};
use libp2p::{kad, Multiaddr, PeerId};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::backend::McpBackend;

/// A single tool entry in the DHT announcement (v2+).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolAnnounce {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// The value stored in the DHT for each clawshake-bridge node.
#[derive(Debug, Serialize, Deserialize)]
pub struct AnnouncementRecord {
    /// Schema version, always 1 for now.
    pub v: u8,
    /// String form of the publishing peer's PeerId.
    pub peer_id: String,
    /// Qualified tool names exposed by this node (e.g. `"spotify.play"`).
    /// Kept for backward-compat with v1 readers; v2+ readers use tool_details.
    pub tools: Vec<String>,
    /// Full tool entries with name + description.
    #[serde(default)]
    pub tool_details: Vec<ToolAnnounce>,
    /// Listen multiaddrs for direct connections.
    pub addrs: Vec<String>,
    /// Unix timestamp (seconds) when this record was built.
    pub ts: u64,
}

impl AnnouncementRecord {
    /// Convert this announcement into a `PeerInfo` suitable for the peer table.
    pub fn to_peer_info(&self) -> PeerInfo {
        let tools: Vec<ToolSummary> = if !self.tool_details.is_empty() {
            self.tool_details
                .iter()
                .map(|t| ToolSummary {
                    name: t.name.clone(),
                    description: t.description.clone(),
                })
                .collect()
        } else {
            // Fallback for v1 records that only have names.
            self.tools
                .iter()
                .map(|n| ToolSummary {
                    name: n.clone(),
                    description: String::new(),
                })
                .collect()
        };
        PeerInfo {
            peer_id: self.peer_id.clone(),
            addrs: self.addrs.clone(),
            tools,
            source: PeerSource::Libp2p,
            last_seen: self.ts,
        }
    }
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

    let tool_details: Vec<ToolAnnounce> = tools
        .iter()
        .filter_map(|t| {
            t["name"].as_str().map(|name| ToolAnnounce {
                name: name.to_string(),
                description: t["description"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect();

    let tool_names: Vec<String> = tool_details.iter().map(|t| t.name.clone()).collect();
    let count = tool_names.len();

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let record = AnnouncementRecord {
        v: 1,
        peer_id: peer_id.to_string(),
        tools: tool_names,
        tool_details,
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
