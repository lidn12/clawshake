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
    network_channel::{ConnectedPeers, DhtLookupTx, OutboundCall, OutboundCallTx},
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
/// `call_tx` must be `Some` for `network_call`; `dht_tx` must be `Some` for
/// `network_tools` and `network_record` (live DHT queries).
pub async fn handle(
    method: &str,
    params: Option<&Value>,
    table: &PeerTable,
    connected: &ConnectedPeers,
    call_tx: Option<&OutboundCallTx>,
    dht_tx: Option<&DhtLookupTx>,
) -> Value {
    let empty = Value::Object(Default::default());
    let params = params.unwrap_or(&empty);

    match method {
        "network_peers" => peers(table),
        "network_tools" => tools(params, table, dht_tx).await,
        "network_search" => search(params, table),
        "network_ping" => ping(params, connected),
        "network_call" => call(params, call_tx).await,
        "network_record" => record(params, table, dht_tx).await,
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

fn tools<'a>(
    params: &'a Value,
    table: &'a PeerTable,
    dht_tx: Option<&'a DhtLookupTx>,
) -> impl std::future::Future<Output = Value> + 'a {
    // Capture params synchronously before the async block.
    let peer_id = params["peer_id"].as_str().map(|s| s.to_string());
    async move {
        let peer_id = match peer_id {
            Some(s) => s,
            None => return err("missing required parameter: peer_id"),
        };

        // Try a live DHT lookup first; fall back to the cached peer table.
        let peer = match dht_lookup_peer(&peer_id, dht_tx).await {
            Some(p) => p,
            None => match table.get(&peer_id) {
                Some(p) => p,
                None => return err(&format!("peer {} not found in table or DHT", peer_id)),
            },
        };

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
        json!({ "tools": tools })
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

fn record<'a>(
    params: &'a Value,
    table: &'a PeerTable,
    dht_tx: Option<&'a DhtLookupTx>,
) -> impl std::future::Future<Output = Value> + 'a {
    let peer_id = params["peer_id"].as_str().map(|s| s.to_string());
    async move {
        let peer_id = match peer_id {
            Some(s) => s,
            None => return err("missing required parameter: peer_id"),
        };

        // Try a live DHT lookup first; fall back to the cached peer table.
        let peer = match dht_lookup_peer(&peer_id, dht_tx).await {
            Some(p) => p,
            None => match table.get(&peer_id) {
                Some(p) => p,
                None => return err(&format!("peer {} not found in table or DHT", peer_id)),
            },
        };

        match peer.raw_record {
            Some(raw) => raw,
            None => err(&format!("no raw DHT record stored for peer {} (discovered via mDNS or not yet seen via DHT)", peer_id)),
        }
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

// ---------------------------------------------------------------------------
// DHT lookup helper
// ---------------------------------------------------------------------------

use clawshake_core::{network_channel::DhtLookup, peer_table::PeerInfo};

/// Send a live DHT GET for `peer_id` through the swarm event loop and wait
/// for the result.  Returns `None` if no `dht_tx` is available (e.g. when
/// called in-process without the P2P stack) or if the lookup fails/times out.
///
/// On success the swarm event loop also upserts the result into the peer
/// table, so subsequent `network_peers` / `network_search` calls see
/// fresh data.
async fn dht_lookup_peer(peer_id: &str, dht_tx: Option<&DhtLookupTx>) -> Option<PeerInfo> {
    let tx = dht_tx?;
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    let lookup = DhtLookup {
        peer_id: peer_id.to_string(),
        response_tx,
    };
    tx.send(lookup).await.ok()?;
    match tokio::time::timeout(std::time::Duration::from_secs(10), response_rx).await {
        Ok(Ok(Ok(info))) => Some(info),
        _ => None,
    }
}
