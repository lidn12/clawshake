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
use tokio::sync::{mpsc, oneshot};

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
// Outbound P2P call channel
// ---------------------------------------------------------------------------

/// A single outbound P2P tool call to be routed through the swarm.
/// `p2p::run()` owns the receiver and drives the send; callers hold a sender.
pub struct OutboundCall {
    /// Target peer ID string — parsed to `PeerId` inside the swarm loop.
    pub peer_id: String,
    /// Raw MCP JSON-RPC request bytes to send via the proxy behaviour.
    pub request: Vec<u8>,
    /// Oneshot channel to deliver the raw response bytes (or an error string).
    pub response_tx: oneshot::Sender<Result<Vec<u8>, String>>,
}

/// Sender half of the outbound P2P call channel.
pub type OutboundCallTx = mpsc::Sender<OutboundCall>;

/// Create a new outbound call channel.
/// Pass the receiver to `p2p::run()`; keep the sender for network call handlers.
pub fn new_outbound_call_channel() -> (OutboundCallTx, mpsc::Receiver<OutboundCall>) {
    mpsc::channel(16)
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
        json!({
            "name": "network.call",
            "description": "Invoke a tool on a specific remote peer over the P2P network and return its result. The peer must be currently connected (use network.ping to check). Arguments must match the tool's inputSchema (use network.describe to inspect it).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": {
                        "type": "string",
                        "description": "libp2p peer ID string of the target peer"
                    },
                    "tool": {
                        "type": "string",
                        "description": "Fully-qualified tool name to invoke on the remote peer (e.g. \"spotify.play\")"
                    },
                    "arguments": {
                        "type": "object",
                        "description": "Arguments to pass to the tool. Must match the tool's inputSchema. Omit or pass {} for tools with no required arguments."
                    }
                },
                "required": ["peer_id", "tool"]
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
///
/// `call_tx` must be `Some` for `network.call`; the other five handlers ignore it.
pub async fn handle(
    method: &str,
    params: Option<&Value>,
    table: &PeerTable,
    connected: &ConnectedPeers,
    call_tx: Option<&OutboundCallTx>,
) -> Value {
    let empty = Value::Object(Default::default());
    let params = params.unwrap_or(&empty);

    match method {
        "network.peers" => peers(table),
        "network.tools" => tools(params, table),
        "network.search" => search(params, table),
        "network.describe" => describe(params, table),
        "network.ping" => ping(params, connected),
        "network.call" => call(params, call_tx).await,
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

async fn call(params: &Value, call_tx: Option<&OutboundCallTx>) -> Value {
    let tx = match call_tx {
        Some(tx) => tx,
        None => return err("network.call: no P2P call channel available"),
    };
    let peer_id = match params["peer_id"].as_str() {
        Some(s) => s,
        None => return err("missing required parameter: peer_id"),
    };
    let tool = match params["tool"].as_str() {
        Some(s) => s,
        None => return err("missing required parameter: tool"),
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    // Build a standards-compliant MCP tools/call JSON-RPC request.
    let mcp_request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": tool,
            "arguments": arguments
        }
    });
    let request_bytes = match serde_json::to_vec(&mcp_request) {
        Ok(b) => b,
        Err(e) => return err(&format!("failed to serialize request: {e}")),
    };

    let (response_tx, response_rx) = oneshot::channel();
    let outbound = OutboundCall {
        peer_id: peer_id.to_string(),
        request: request_bytes,
        response_tx,
    };

    if tx.send(outbound).await.is_err() {
        return err("network.call: P2P call channel closed");
    }

    match response_rx.await {
        Ok(Ok(bytes)) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(v) => v,
            Err(e) => err(&format!("failed to parse response from peer: {e}")),
        },
        Ok(Err(e)) => err(&format!("P2P call failed: {e}")),
        Err(_) => err("network.call: response channel dropped unexpectedly"),
    }
}

fn err(msg: &str) -> Value {
    json!({ "error": msg })
}
