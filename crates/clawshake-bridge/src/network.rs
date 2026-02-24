//! Built-in `network.*` tool namespace.
//!
//! These five tools are served by the bridge itself — no backend call needed.
//! They expose a read-only view over the [`PeerTable`] the bridge already
//! maintains from DHT discovery.
//!
//! The tools are injected into every `tools/list` response so that any MCP
//! client can discover and call them alongside the backend's own tools.

use std::{
    collections::HashSet,
    sync::{Arc, RwLock},
};

use clawshake_core::peer_table::PeerTable;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Shared type for connected-peer tracking
// ---------------------------------------------------------------------------

/// A shared, cheaply-cloneable set of currently-connected peer ID strings.
/// Populated by the p2p event loop via `ConnectionEstablished` /
/// `ConnectionClosed` events.
pub type ConnectedPeers = Arc<RwLock<HashSet<String>>>;

pub fn new_connected_peers() -> ConnectedPeers {
    Arc::new(RwLock::new(HashSet::new()))
}

// ---------------------------------------------------------------------------
// Tool schema definitions (injected into tools/list)
// ---------------------------------------------------------------------------

/// Returns MCP tool schema objects for all five `network.*` tools.
pub fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "network.peers",
            "description": "List all discovered bridge nodes on the network with their peer IDs, addresses, tool counts, and last-seen timestamps.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        }),
        json!({
            "name": "network.tools",
            "description": "Get the full tool list (name + description) for a specific peer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": {
                        "type": "string",
                        "description": "libp2p peer ID string (from network.peers)"
                    }
                },
                "required": ["peer_id"]
            }
        }),
        json!({
            "name": "network.search",
            "description": "Search for tools across all known peers by tool name or description substring.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Case-insensitive substring to match against tool names and descriptions"
                    }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "network.describe",
            "description": "Get the description for a specific tool on a specific peer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": {
                        "type": "string",
                        "description": "libp2p peer ID string"
                    },
                    "tool_name": {
                        "type": "string",
                        "description": "Fully-qualified tool name (e.g. \"spotify.play\")"
                    }
                },
                "required": ["peer_id", "tool_name"]
            }
        }),
        json!({
            "name": "network.ping",
            "description": "Check whether a peer currently has an active connection to this node.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": {
                        "type": "string",
                        "description": "libp2p peer ID string"
                    }
                },
                "required": ["peer_id"]
            }
        }),
    ]
}

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

/// Handle a `network.*` tool call.
///
/// Returns the result value to embed in the MCP `tools/call` response content.
/// Never panics — all errors are returned as `{ "error": "..." }`.
pub fn handle(
    method: &str,
    params: Option<&Value>,
    table: &PeerTable,
    connected: &ConnectedPeers,
) -> Value {
    let empty = Value::Object(Default::default());
    let params = params.unwrap_or(&empty);

    match method {
        "network.peers" => peers(table),
        "network.tools" => tools(params, table),
        "network.search" => search(params, table),
        "network.describe" => describe(params, table),
        "network.ping" => ping(params, connected),
        _ => err(&format!("unknown network method: {}", method)),
    }
}

// ---------------------------------------------------------------------------
// Individual handlers
// ---------------------------------------------------------------------------

fn peers(table: &PeerTable) -> Value {
    let list: Vec<Value> = table
        .all()
        .into_iter()
        .map(|p| {
            json!({
                "peer_id":    p.peer_id,
                "addrs":      p.addrs,
                "tool_count": p.tools.len(),
                "last_seen":  p.last_seen,
            })
        })
        .collect();
    json!({ "peers": list })
}

fn tools(params: &Value, table: &PeerTable) -> Value {
    let peer_id = match params["peer_id"].as_str() {
        Some(s) => s,
        None => return err("missing required parameter: peer_id"),
    };
    match table.get(peer_id) {
        Some(peer) => {
            let tools: Vec<Value> = peer
                .tools
                .iter()
                .map(|t| json!({ "name": t.name, "description": t.description }))
                .collect();
            json!({ "peer_id": peer_id, "tools": tools })
        }
        None => err(&format!("peer {} not found in table", peer_id)),
    }
}

fn search(params: &Value, table: &PeerTable) -> Value {
    let query = match params["query"].as_str() {
        Some(s) => s.to_lowercase(),
        None => return err("missing required parameter: query"),
    };
    let mut results: Vec<Value> = Vec::new();
    for peer in table.all() {
        for tool in &peer.tools {
            if tool.name.to_lowercase().contains(&query)
                || tool.description.to_lowercase().contains(&query)
            {
                results.push(json!({
                    "peer_id":     peer.peer_id,
                    "tool_name":   tool.name,
                    "description": tool.description,
                    "addrs":       peer.addrs,
                }));
            }
        }
    }
    json!({ "query": query, "results": results })
}

fn describe(params: &Value, table: &PeerTable) -> Value {
    let peer_id = match params["peer_id"].as_str() {
        Some(s) => s,
        None => return err("missing required parameter: peer_id"),
    };
    let tool_name = match params["tool_name"].as_str() {
        Some(s) => s,
        None => return err("missing required parameter: tool_name"),
    };
    match table.get(peer_id) {
        Some(peer) => match peer.tools.iter().find(|t| t.name == tool_name) {
            Some(tool) => json!({
                "peer_id":     peer_id,
                "tool_name":   tool.name,
                "description": tool.description,
            }),
            None => err(&format!("tool {} not found on peer {}", tool_name, peer_id)),
        },
        None => err(&format!("peer {} not found in table", peer_id)),
    }
}

fn ping(params: &Value, connected: &ConnectedPeers) -> Value {
    let peer_id = match params["peer_id"].as_str() {
        Some(s) => s,
        None => return err("missing required parameter: peer_id"),
    };
    let set = connected.read().expect("connected peers lock poisoned");
    json!({ "peer_id": peer_id, "reachable": set.contains(peer_id) })
}

fn err(msg: &str) -> Value {
    json!({ "error": msg })
}
