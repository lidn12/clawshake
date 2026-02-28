//! `network_*` tool handlers.
//!
//! This module owns all handler logic for the six built-in P2P network tools.
//! The MCP schemas for these tools live in [`crate::schema`].
//!
//! Called by:
//! - `clawshake-bridge::ipc` — inbound from any local process via the IPC socket
//! - `clawshake-bridge`'s inbound proxy path — remote P2P callers (subject to
//!   permission store; `network_*` is blocked for remote callers by default)

use clawshake_core::{
    network_channel::{ConnectedPeers, OutboundCall, OutboundCallTx},
    peer_table::PeerTable,
};
use serde_json::{json, Value};
use tokio::sync::oneshot;

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

/// Handle a `network_*` tool call.
///
/// Returns the result value to embed in the MCP `tools/call` response content.
/// Never panics — all errors are returned as `{ "error": "..." }`.
///
/// `call_tx` must be `Some` for `network_call`; the other five handlers ignore it.
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
        "network_peers" => peers(table),
        "network_tools" => tools(params, table),
        "network_search" => search(params, table),
        "network_ping" => ping(params, connected),
        "network_call" => call(params, call_tx).await,
        "network_record" => record(params, table),
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
                .map(|t| {
                    let mut entry = json!({
                        "name":        t.name,
                        "description": t.description,
                    });
                    if let Some(schema) = &t.input_schema {
                        entry["inputSchema"] = schema.clone();
                    } else {
                        entry["inputSchema"] = json!({ "type": "object" });
                    }
                    entry
                })
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

fn record(params: &Value, table: &PeerTable) -> Value {
    let peer_id = match params["peer_id"].as_str() {
        Some(s) => s,
        None => return err("missing required parameter: peer_id"),
    };
    match table.get(peer_id) {
        Some(peer) => match peer.raw_record {
            Some(raw) => raw,
            None => err(&format!("no raw DHT record stored for peer {} (discovered via mDNS or not yet seen via DHT)", peer_id)),
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
        None => return err("network_call: no P2P call channel available"),
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
        return err("network_call: P2P call channel closed");
    }

    let raw = match response_rx.await {
        Ok(Ok(bytes)) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(v) => v,
            Err(e) => return err(&format!("failed to parse response from peer: {e}")),
        },
        Ok(Err(e)) => return err(&format!("P2P call failed: {e}")),
        Err(_) => return err("network_call: response channel dropped unexpectedly"),
    };

    // Unwrap the MCP JSON-RPC envelope so the caller sees the same content
    // shape as a local tools/call — plain text (or error text), not a nested
    // JSON-RPC response serialised inside a string.
    //
    // Success: {"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"..."}],"isError":false}}
    // Error:   {"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"..."}}
    if let Some(result) = raw.get("result") {
        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // Collect text from all content entries.
        let text = result
            .get("content")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if is_error {
            err(&text)
        } else {
            json!({ "result": text })
        }
    } else if let Some(error) = raw.get("error") {
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        err(&format!("Remote node returned JSON-RPC error: {message}"))
    } else {
        // Unrecognised response shape — return as-is.
        raw
    }
}

fn err(msg: &str) -> Value {
    json!({ "error": msg })
}
