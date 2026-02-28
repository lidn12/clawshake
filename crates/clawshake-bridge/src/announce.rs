//! DHT tool announcement.
//!
//! Each bridge node publishes a `kad::Record` under `key = peer_id.to_bytes()`
//! containing a JSON announcement of its available tools and listen addresses.
//! The record is published on startup and refreshed every 5 minutes so it
//! does not expire from the DHT.
//!
//! Any other node that knows a peer's ID can do a DHT GET with the same key
//! to discover its tool list — this is how `network_tools(peer_id)` will work.

use anyhow::Result;
use clawshake_core::peer_table::{PeerInfo, PeerSource, ToolSummary};
use clawshake_core::permissions::PermissionStore;
use libp2p::{kad, Multiaddr, PeerId};
use serde::{Deserialize, Serialize};
use tracing::info;

use clawshake_core::mcp_client::McpClient;

/// A single tool entry in the DHT announcement.
/// Matches the MCP `tools/list` tool definition shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolAnnounce {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Full JSON Schema for the tool's input parameters.
    /// Defaults to `{"type": "object"}` per MCP spec.
    #[serde(rename = "inputSchema", default = "default_input_schema")]
    pub input_schema: serde_json::Value,
}

fn default_input_schema() -> serde_json::Value {
    serde_json::json!({ "type": "object" })
}

/// The value stored in the DHT for each clawshake-bridge node.
#[derive(Debug, Serialize, Deserialize)]
pub struct AnnouncementRecord {
    /// Schema version, always 1 for now.
    pub v: u8,
    /// String form of the publishing peer's PeerId.
    pub peer_id: String,
    /// Tool entries with name, description, and input schema.
    pub tools: Vec<ToolAnnounce>,
    /// Listen multiaddrs for direct connections.
    pub addrs: Vec<String>,
    /// Unix timestamp (seconds) when this record was built.
    pub ts: u64,
}

impl AnnouncementRecord {
    /// Convert this announcement into a `PeerInfo` suitable for the peer table.
    pub fn to_peer_info(&self) -> PeerInfo {
        let tools: Vec<ToolSummary> = self
            .tools
            .iter()
            .map(|t| ToolSummary {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: Some(t.input_schema.clone()),
            })
            .collect();
        PeerInfo {
            peer_id: self.peer_id.clone(),
            addrs: self.addrs.clone(),
            tools,
            source: PeerSource::Libp2p,
            last_seen: self.ts,
            raw_record: serde_json::to_value(self).ok(),
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
///
/// Only tools that are "network-exposed" (at least one remote agent pattern
/// has `allow`) are included in the announcement.  Tools with no remote allow
/// are omitted from the DHT — they remain available locally but are not
/// discoverable by other peers.
pub async fn build_record(
    peer_id: PeerId,
    listen_addrs: &[Multiaddr],
    backend: &McpClient,
    permissions: &PermissionStore,
) -> Result<kad::Record> {
    let raw_tools = backend.tools_list().await?;

    let mut tools: Vec<ToolAnnounce> = Vec::new();
    for t in &raw_tools {
        let name = match t["name"].as_str() {
            Some(n) => n,
            None => continue,
        };
        if !permissions.is_network_exposed(name).await {
            continue;
        }
        tools.push(ToolAnnounce {
            name: name.to_string(),
            description: t["description"].as_str().unwrap_or("").to_string(),
            input_schema: t
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({ "type": "object" })),
        });
    }

    let count = tools.len();

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let record = AnnouncementRecord {
        v: 1,
        peer_id: peer_id.to_string(),
        tools,
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
