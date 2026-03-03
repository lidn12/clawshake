//! Inbound MCP proxy over libp2p.
//!
//! Remote agents connect via libp2p and send MCP JSON-RPC requests using the
//! `/clawshake/mcp/1.0.0` request-response protocol.  The bridge:
//!
//!   1. Stamps the caller's identity as `AgentId::P2p(peer_id)`.
//!   2. Checks permissions via the PermissionStore (P2P callers denied by default).
//!   3. Forwards the raw JSON-RPC bytes to the local MCP backend.
//!   4. Returns the response over the same libp2p stream.
//!
//! Wire format: each request and response is length-prefixed with a 4-byte
//! big-endian u32 followed by that many bytes of UTF-8 JSON.

use clawshake_core::{
    identity::AgentId,
    permissions::{Decision, PermissionStore},
};
use libp2p::{
    request_response::{self, ProtocolSupport},
    PeerId, StreamProtocol,
};
use serde_json::Value;
use tracing::{info, warn};

use clawshake_core::mcp_client::McpClient;

use crate::codec::LengthPrefixedCodec;

// ---------------------------------------------------------------------------
// Behaviour type alias + constructor
// ---------------------------------------------------------------------------

pub type Behaviour = request_response::Behaviour<LengthPrefixedCodec>;
pub type Event = request_response::Event<Vec<u8>, Vec<u8>>;

pub fn new_behaviour() -> Behaviour {
    request_response::Behaviour::<LengthPrefixedCodec>::new(
        vec![(
            StreamProtocol::new("/clawshake/mcp/1.0.0"),
            ProtocolSupport::Full,
        )],
        request_response::Config::default(),
    )
}

// ---------------------------------------------------------------------------
// Request forwarding
// ---------------------------------------------------------------------------

/// Forward a raw MCP JSON-RPC request (as bytes) to the backend and return
/// the raw response bytes.  Errors are wrapped in a JSON-RPC error response
/// so the remote caller always gets a well-formed reply.
///
/// The caller's identity is derived from the libp2p peer ID (transport-layer
/// verified via the Noise handshake) and checked against the permission store
/// before the call is forwarded.
///
/// `network_*` tools are **not** intercepted here.  This function is the P2P
/// path only — remote callers are denied `network_*` by the permission store
/// default (`p2p:* → * → deny`).  Network tools are served on the local
/// caller surface, which does not go through this proxy.
pub async fn forward(
    backend: &McpClient,
    store: &PermissionStore,
    caller: &PeerId,
    raw: Vec<u8>,
) -> Vec<u8> {
    let req: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(e) => {
            warn!(%caller, "Failed to parse inbound MCP request: {e}");
            return error_response(None, -32700, "Parse error");
        }
    };

    let id = req.get("id").cloned();
    let method = req
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_owned(); // owned so it outlives the move of `req` into backend.call()

    // Identity is stamped from the transport (Noise-verified peer ID).
    // The caller has no say in this value.
    let agent_id = AgentId::P2p(caller.to_string());

    match store.check(&agent_id, &method).await {
        Decision::Allow => {
            info!(%caller, method, "Permission granted — proxying inbound MCP call");
        }
        Decision::Ask => {
            // `Ask` means "prompt locally" — no UI for remote callers, auto-deny.
            warn!(%caller, method, "Permission denied (ask → deny for P2P callers)");
            return permission_denied_response(id, &method, caller);
        }
        Decision::Deny => {
            warn!(%caller, method, "Permission denied");
            return permission_denied_response(id, &method, caller);
        }
    }

    match backend.call(req).await {
        Ok(mut resp) => {
            // Restore the caller's original id (which may be a string, number, or null).
            // backend.call() stamps a fresh u64 id before sending; the echoed numeric id
            // in `resp` must not leak back to the caller.
            if let Some(ref orig_id) = id {
                resp["id"] = orig_id.clone();
            }
            serde_json::to_vec(&resp).unwrap_or_else(|_| {
                error_response(id, -32603, "Internal error: failed to serialise response")
            })
        }
        Err(e) => {
            warn!(%caller, "Backend call failed: {e}");
            error_response(
                id,
                -32603,
                &format!(
                    "The local broker failed to process the request for method '{}': {}. \
                     This may indicate the broker is misconfigured or the tool is unavailable.",
                    method, e
                ),
            )
        }
    }
}

fn error_response(id: Option<Value>, code: i64, message: &str) -> Vec<u8> {
    let r = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    });
    serde_json::to_vec(&r).expect("JSON-RPC error serializes")
}

/// Return a well-formed MCP `tools/call` result with `isError: true`.
/// Per MCP spec, tool-level errors (including permission denied) should be
/// reported as successful JSON-RPC responses with error content, not as
/// JSON-RPC error objects.
fn permission_denied_response(id: Option<Value>, tool_name: &str, caller: &PeerId) -> Vec<u8> {
    let r = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{ "type": "text", "text": format!(
                "Permission denied: peer '{}' is not allowed to call '{}'. \
                 The node operator can grant access with: clawshake permissions allow p2p:{} {}",
                caller, tool_name, caller, tool_name
            ) }],
            "isError": true
        }
    });
    serde_json::to_vec(&r).expect("JSON-RPC response serializes")
}
