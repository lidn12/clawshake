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
use clawshake_core::models::ModelAnnounce;
use clawshake_core::peer_table::{PeerInfo, PeerSource, ToolSummary};
use clawshake_core::permissions::PermissionStore;
use libp2p::{kad, Multiaddr, PeerId};
use serde::{Deserialize, Serialize};
use tracing::info;

use clawshake_core::mcp_client::McpClient;

/// Build a `kad::RecordKey` for a peer's DHT announcement.
pub fn record_key(peer_id: &PeerId) -> kad::RecordKey {
    kad::RecordKey::new(&peer_id.to_bytes())
}

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
    /// Schema version.  1 = tools only, 2 = tools + models.
    pub v: u8,
    /// String form of the publishing peer's PeerId.
    pub peer_id: String,
    /// Tool entries with name, description, and input schema.
    pub tools: Vec<ToolAnnounce>,
    /// Models available on this peer.  Empty if no model server configured.
    /// Added in schema v2; `#[serde(default)]` ensures v1 records parse fine.
    #[serde(default)]
    pub models: Vec<ModelAnnounce>,
    /// Human-readable description of this node, e.g. "work laptop".
    /// Added in schema v2; older records without this field parse as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
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
        let models = self.models.clone();
        PeerInfo {
            peer_id: self.peer_id.clone(),
            description: self.description.clone(),
            addrs: self.addrs.clone(),
            tools,
            models,
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

/// Build a Kademlia record for this node ready for `kad::Behaviour::put_record`.
///
/// Always succeeds: when `backend` is `None` the record is published with an
/// empty tool list, so the node's description and addresses are still visible
/// to peers even on relay-only or no-backend configurations.
///
/// When a backend is present, only tools that are "network-exposed" (at least
/// one remote agent pattern has `allow`) are included.  Tools with no remote
/// allow are omitted — discoverable locally but not via the DHT.
pub async fn build_record(
    peer_id: PeerId,
    listen_addrs: &[Multiaddr],
    backend: Option<&McpClient>,
    permissions: &PermissionStore,
    models: Vec<ModelAnnounce>,
    description: Option<String>,
) -> Result<kad::Record> {
    let mut tools: Vec<ToolAnnounce> = Vec::new();
    if let Some(backend) = backend {
        let raw_tools = backend.tools_list().await?;
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
    }

    let tool_count = tools.len();
    let model_count = models.len();
    let version = if models.is_empty() { 1 } else { 2 };

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let record = AnnouncementRecord {
        v: version,
        peer_id: peer_id.to_string(),
        tools,
        models,
        description,
        addrs: listen_addrs.iter().map(|a| a.to_string()).collect(),
        ts,
    };

    info!(tools = tool_count, models = model_count, peer = %peer_id, "Built DHT announcement");

    Ok(kad::Record {
        key: record_key(&peer_id),
        value: record.to_bytes(),
        publisher: Some(peer_id),
        expires: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawshake_core::peer_table::PeerSource;

    fn make_record() -> AnnouncementRecord {
        AnnouncementRecord {
            v: 2,
            peer_id: "12D3KooWTestPeer".to_string(),
            tools: vec![
                ToolAnnounce {
                    name: "tool_a".to_string(),
                    description: "First tool".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
                ToolAnnounce {
                    name: "tool_b".to_string(),
                    description: "Second tool".to_string(),
                    input_schema: serde_json::json!({"type": "object", "properties": {"q": {"type": "string"}}}),
                },
            ],
            models: vec![ModelAnnounce {
                name: "llama3".to_string(),
                context_length: Some(8192),
                params: Some("8B".to_string()),
            }],
            description: Some("Test desktop".to_string()),
            addrs: vec![
                "/ip4/1.2.3.4/tcp/7474".to_string(),
                "/ip4/5.6.7.8/tcp/7474".to_string(),
            ],
            ts: 1_700_000_000,
        }
    }

    #[test]
    fn announcement_round_trip() {
        let original = make_record();
        let bytes = original.to_bytes();
        let decoded = AnnouncementRecord::from_bytes(&bytes).unwrap();

        assert_eq!(decoded.v, original.v);
        assert_eq!(decoded.peer_id, original.peer_id);
        assert_eq!(decoded.tools.len(), 2);
        assert_eq!(decoded.tools[0].name, "tool_a");
        assert_eq!(decoded.tools[1].name, "tool_b");
        assert_eq!(decoded.models.len(), 1);
        assert_eq!(decoded.models[0].name, "llama3");
        assert_eq!(decoded.addrs, original.addrs);
        assert_eq!(decoded.ts, original.ts);
        assert_eq!(decoded.description, Some("Test desktop".to_string()));
    }

    #[test]
    fn announcement_to_peer_info() {
        let record = make_record();
        let info = record.to_peer_info();

        assert_eq!(info.peer_id, "12D3KooWTestPeer");
        assert_eq!(info.description, Some("Test desktop".to_string()));
        assert_eq!(info.addrs.len(), 2);
        assert_eq!(info.tools.len(), 2);
        assert_eq!(info.tools[0].name, "tool_a");
        assert_eq!(info.tools[0].description, "First tool");
        assert!(info.tools[1].input_schema.is_some());
        assert_eq!(info.models.len(), 1);
        assert_eq!(info.models[0].name, "llama3");
        assert_eq!(info.source, PeerSource::Libp2p);
        assert_eq!(info.last_seen, 1_700_000_000);

        // raw_record must be Some and round-trip back to the original peer_id.
        // If to_value silently failed or serialised the wrong struct, this fails.
        let raw = info.raw_record.as_ref().expect("raw_record must be Some");
        assert_eq!(
            raw["peer_id"].as_str(),
            Some("12D3KooWTestPeer"),
            "raw_record must contain the serialised peer_id"
        );
        assert_eq!(
            raw["tools"][0]["name"].as_str(),
            Some("tool_a"),
            "raw_record tools must preserve tool order and names"
        );
    }

    #[test]
    fn v1_record_without_models() {
        // Version-1 records have no "models" field — serde default must give empty vec.
        let json = r#"{"v":1,"peer_id":"testpeer","tools":[],"addrs":[],"ts":0}"#;
        let record: AnnouncementRecord = serde_json::from_str(json).unwrap();
        assert_eq!(record.v, 1);
        assert!(
            record.models.is_empty(),
            "v1 record must default models to []"
        );
        assert!(
            record.description.is_none(),
            "v1 record must default description to None"
        );
    }
}
