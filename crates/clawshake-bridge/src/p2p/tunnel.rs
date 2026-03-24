//! TCP-over-P2P tunnel splice logic and tunnel-based RPC.
//!
//! Provides the bidirectional byte-level splice between a libp2p stream
//! and a local TCP connection.  Also handles inbound tunnel stream acceptance
//! and outbound tunnel response interception.
//!
//! The special tunnel name `_rpc` is used for MCP JSON-RPC calls.  Inbound
//! `_rpc` streams are handled by reading the request, stamping the caller's
//! identity, checking permissions, and forwarding to the local MCP backend.
//! Outbound calls open a `_rpc` tunnel stream, send the request, and read
//! the response.
//!
//! **IO trait landscape**: libp2p streams implement `futures::io::{AsyncRead,
//! AsyncWrite}` while Tokio TCP streams implement `tokio::io::*`.  The
//! [`splice`] function bridges the two worlds by splitting each stream into
//! read/write halves and using the appropriate extension trait on each half.

use std::sync::Arc;

use anyhow::{Context, Result};
use clawshake_core::identity::AgentId;
use clawshake_core::mcp_client::McpClient;
use clawshake_core::network_channel::TunnelTable;
use clawshake_core::permissions::{Decision, PermissionStore};
use futures::io::{AsyncReadExt as FuturesReadExt, AsyncWriteExt as FuturesWriteExt};
use futures::StreamExt;
use libp2p::PeerId;
use libp2p_stream::IncomingStreams;
use serde_json::Value;
use tokio::io::{AsyncReadExt as TokioReadExt, AsyncWriteExt as TokioWriteExt};
use tokio::net::TcpStream;
use tracing::{info, warn};

use crate::codec;

/// Reserved tunnel name for MCP JSON-RPC forwarding.
const RPC_TUNNEL_NAME: &str = "_rpc";

/// Context needed for inbound RPC handling.
pub struct RpcContext {
    pub backend: Option<McpClient>,
    pub permissions: Arc<PermissionStore>,
}

/// Accept inbound tunnel streams in a long-lived task.
///
/// For each incoming stream:
/// 1. Read the tunnel header (JSON with `name` field).
/// 2. If `_rpc` — forward to the local MCP backend with identity stamping.
/// 3. Otherwise — look up the tunnel table, check peer allowlist, splice TCP.
pub async fn accept_inbound_tunnels(
    mut incoming: IncomingStreams,
    tunnel_table: TunnelTable,
    rpc_ctx: Arc<RpcContext>,
) {
    while let Some((peer, stream)) = incoming.next().await {
        let table = tunnel_table.clone();
        let ctx = rpc_ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_inbound_tunnel(peer, stream, &table, &ctx).await {
                warn!(%peer, "Inbound tunnel error: {e:#}");
            }
        });
    }
}

/// Handle a single inbound tunnel stream.
async fn handle_inbound_tunnel(
    peer: PeerId,
    mut stream: libp2p::swarm::Stream,
    tunnel_table: &TunnelTable,
    rpc_ctx: &RpcContext,
) -> Result<()> {
    // Read the tunnel header (same length-prefixed format as all our protocols).
    let header_bytes = codec::read_framed(&mut stream)
        .await
        .context("reading tunnel header")?;

    let header: serde_json::Value =
        serde_json::from_slice(&header_bytes).context("parsing tunnel header")?;

    let name = header["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("tunnel header missing 'name' field"))?;

    // Special case: _rpc tunnel — forward MCP JSON-RPC to the local backend.
    if name == RPC_TUNNEL_NAME {
        return handle_rpc_stream(peer, stream, rpc_ctx).await;
    }

    // Regular tunnel: look up the tunnel table.
    let entry = {
        let map = tunnel_table.read().expect("tunnel table lock");
        map.get(name).cloned()
    };
    let entry = entry.ok_or_else(|| anyhow::anyhow!("no active tunnel for name '{name}'"))?;

    // Check peer allowlist.
    let peer_str = peer.to_string();
    if let Some(ref allowed) = entry.peers {
        if !allowed.contains(&peer_str) {
            // Write a 1-byte rejection before closing.
            let _ = FuturesWriteExt::write_all(&mut stream, &[0x00]).await;
            anyhow::bail!("peer {peer_str} not in allowlist for tunnel '{name}'");
        }
    }

    // Write a 1-byte success indicator.
    FuturesWriteExt::write_all(&mut stream, &[0x01])
        .await
        .context("writing tunnel ack")?;

    // Connect to the local service.
    let tcp = TcpStream::connect(format!("127.0.0.1:{}", entry.port))
        .await
        .with_context(|| format!("connecting to localhost:{}", entry.port))?;

    info!(%peer, name, port = entry.port, "Tunnel accepted — splicing");

    // Bidirectional splice.
    splice(stream, tcp).await;

    Ok(())
}

// ---------------------------------------------------------------------------
// _rpc tunnel: MCP JSON-RPC forwarding
// ---------------------------------------------------------------------------

/// Handle an inbound `_rpc` tunnel stream.
///
/// The caller's identity is derived from the libp2p peer ID (transport-layer
/// verified via the Noise handshake) and checked against the permission store
/// before the call is forwarded.
///
/// Wire format: length-prefixed JSON-RPC request, length-prefixed JSON-RPC
/// response (same framing as `read_framed` / `write_framed`).
async fn handle_rpc_stream(
    peer: PeerId,
    mut stream: libp2p::swarm::Stream,
    ctx: &RpcContext,
) -> Result<()> {
    // Write 1-byte ack (same as regular tunnels for protocol consistency).
    FuturesWriteExt::write_all(&mut stream, &[0x01])
        .await
        .context("writing RPC ack")?;

    // Read the JSON-RPC request.
    let request_bytes = codec::read_framed(&mut stream)
        .await
        .context("reading RPC request")?;

    let response_bytes = match ctx.backend {
        Some(ref backend) => rpc_forward(backend, &ctx.permissions, &peer, request_bytes).await,
        None => serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {
                "code": -32603,
                "message": "No MCP backend configured on this node"
            }
        }))
        .unwrap_or_default(),
    };

    // Write the response back.
    codec::write_framed(&mut stream, &response_bytes)
        .await
        .context("writing RPC response")?;

    Ok(())
}

/// Forward a raw MCP JSON-RPC request to the local backend with identity
/// stamping and permission checking.
///
/// This is the tunnel-based equivalent of the old `proxy::forward()`.
async fn rpc_forward(
    backend: &McpClient,
    store: &PermissionStore,
    caller: &PeerId,
    raw: Vec<u8>,
) -> Vec<u8> {
    let req: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(e) => {
            warn!(%caller, "Failed to parse inbound MCP request: {e}");
            return rpc_error_response(None, -32700, "Parse error");
        }
    };

    let id = req.get("id").cloned();
    let method = req
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_owned();

    // Identity is stamped from the transport (Noise-verified peer ID).
    let agent_id = AgentId::P2p(caller.to_string());

    match store.check(&agent_id, &method).await {
        Decision::Allow => {
            info!(%caller, method, "Permission granted — forwarding RPC call");
        }
        Decision::Ask => {
            warn!(%caller, method, "Permission denied (ask → deny for P2P callers)");
            return rpc_permission_denied(id, &method, caller);
        }
        Decision::Deny => {
            warn!(%caller, method, "Permission denied");
            return rpc_permission_denied(id, &method, caller);
        }
    }

    match backend.call(req).await {
        Ok(mut resp) => {
            if let Some(ref orig_id) = id {
                resp["id"] = orig_id.clone();
            }
            serde_json::to_vec(&resp).unwrap_or_else(|_| {
                rpc_error_response(id, -32603, "Internal error: failed to serialise response")
            })
        }
        Err(e) => {
            warn!(%caller, "Backend call failed: {e}");
            rpc_error_response(
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

fn rpc_error_response(id: Option<Value>, code: i64, message: &str) -> Vec<u8> {
    let r = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    });
    serde_json::to_vec(&r).expect("JSON-RPC error serializes")
}

fn rpc_permission_denied(id: Option<Value>, tool_name: &str, caller: &PeerId) -> Vec<u8> {
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

// ---------------------------------------------------------------------------
// Outbound RPC via tunnel
// ---------------------------------------------------------------------------

/// Send an outbound MCP JSON-RPC request to a remote peer via `_rpc` tunnel.
///
/// Opens a `/clawshake/tunnel/1.0.0` stream, sends the `_rpc` header, then
/// does a single request-response exchange.  The response is checked for
/// tunnel authorization (connect_* responses) and rewritten if needed.
pub async fn send_rpc_via_tunnel(
    peer_id: PeerId,
    request: Vec<u8>,
    stream_control: &mut libp2p_stream::Control,
    tunnel_protocol: libp2p::StreamProtocol,
) -> Result<Vec<u8>, String> {
    // Open a stream to the remote peer.
    let mut stream = stream_control
        .open_stream(peer_id, tunnel_protocol.clone())
        .await
        .map_err(|e| format!("Failed to open tunnel stream to {peer_id}: {e}"))?;

    // Send the _rpc tunnel header.
    let header = serde_json::json!({ "name": RPC_TUNNEL_NAME });
    let header_bytes = serde_json::to_vec(&header).unwrap();
    codec::write_framed(&mut stream, &header_bytes)
        .await
        .map_err(|e| format!("Failed to write tunnel header: {e}"))?;

    // Read the 1-byte ack.
    let mut ack = [0u8; 1];
    FuturesReadExt::read_exact(&mut stream, &mut ack)
        .await
        .map_err(|e| format!("Tunnel ack read failed: {e}"))?;
    if ack[0] != 0x01 {
        return Err("Tunnel rejected by remote bridge".to_string());
    }

    // Send the JSON-RPC request.
    codec::write_framed(&mut stream, &request)
        .await
        .map_err(|e| format!("Failed to write RPC request: {e}"))?;

    // Read the JSON-RPC response.
    let response = codec::read_framed(&mut stream)
        .await
        .map_err(|e| format!("Failed to read RPC response: {e}"))?;

    // Check if this is a tunnel authorization response.
    // If so, open the TCP tunnel and rewrite the response.
    match maybe_open_tunnel(&response, peer_id, stream_control, tunnel_protocol).await {
        Some(modified) => Ok(modified),
        None => Ok(response),
    }
}

/// Intercept a `network_call` response and check if it's a tunnel
/// authorization.  If so, open the tunnel and return a modified response
/// with `{ local_port, local_url }`.
///
/// Returns `None` if the response is not a tunnel authorization (caller
/// should deliver the original response as-is).
pub async fn maybe_open_tunnel(
    raw_response: &[u8],
    target_peer: PeerId,
    stream_control: &mut libp2p_stream::Control,
    tunnel_protocol: libp2p::StreamProtocol,
) -> Option<Vec<u8>> {
    // Parse the MCP JSON-RPC response envelope.
    let resp: serde_json::Value = serde_json::from_slice(raw_response).ok()?;

    // Extract text content from the MCP result.
    let result = resp.get("result")?;
    let content = result.get("content")?.as_array()?;
    let text = content.first()?.get("text")?.as_str()?;

    // Parse the inner JSON to check for tunnel authorization.
    let inner: serde_json::Value = serde_json::from_str(text).ok()?;
    if inner.get("authorized")?.as_bool()? != true {
        return None;
    }
    let _remote_port = inner.get("port")?.as_u64()? as u16;
    let requested_local_port = inner
        .get("local_port")
        .and_then(|v| v.as_u64())
        .map(|p| p as u16);

    // The connect_* handler includes `name` in its response so the caller's
    // bridge knows which tunnel to request on the remote side.
    let name = inner
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    info!(%target_peer, name, "Tunnel authorization received — opening accept loop");

    // Bind a local TCP listener first so we can return the port immediately.
    let bind_addr = format!("127.0.0.1:{}", requested_local_port.unwrap_or(0));
    let listener = match tokio::net::TcpListener::bind(&bind_addr).await {
        Ok(l) => l,
        Err(e) => {
            warn!("Failed to bind local tunnel port {bind_addr}: {e}");
            return None;
        }
    };
    let local_port = listener.local_addr().ok()?.port();

    info!(local_port, %target_peer, name, "Tunnel listener ready");

    // Clone stream_control so the accept loop can open new streams per connection.
    let sc = stream_control.clone();
    let proto = tunnel_protocol;
    let name_owned = name.to_string();

    // Persistent accept loop: each TCP connection gets its own P2P stream,
    // allowing multiple concurrent clients (e.g. repeated HTTP requests).
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((tcp_stream, client_addr)) => {
                    let mut sc2 = sc.clone();
                    let proto2 = proto.clone();
                    let name2 = name_owned.clone();
                    tokio::spawn(async move {
                        // Open a fresh P2P stream to the remote peer.
                        let mut p2p_stream = match sc2.open_stream(target_peer, proto2).await {
                            Ok(s) => s,
                            Err(e) => {
                                warn!(%target_peer, %client_addr,
                                        "Failed to open tunnel stream: {e}");
                                return;
                            }
                        };
                        // Send the tunnel header.
                        let header = serde_json::json!({ "name": &name2 });
                        let header_bytes = match serde_json::to_vec(&header) {
                            Ok(b) => b,
                            Err(_) => return,
                        };
                        if codec::write_framed(&mut p2p_stream, &header_bytes)
                            .await
                            .is_err()
                        {
                            warn!(%target_peer, "Tunnel header write failed");
                            return;
                        }
                        // Read ack.
                        let mut ack = [0u8; 1];
                        if FuturesReadExt::read_exact(&mut p2p_stream, &mut ack)
                            .await
                            .is_err()
                            || ack[0] != 0x01
                        {
                            warn!(%target_peer, "Tunnel ack failed");
                            return;
                        }
                        splice(p2p_stream, tcp_stream).await;
                    });
                }
                Err(e) => {
                    warn!("Tunnel accept loop exited: {e}");
                    break;
                }
            }
        }
    });

    // Build the modified MCP response.
    let local_url = format!("http://localhost:{local_port}");
    let tunnel_result = serde_json::json!({
        "local_port": local_port,
        "local_url": local_url,
    });
    let tunnel_text = serde_json::to_string(&tunnel_result).ok()?;

    // Re-wrap in the MCP JSON-RPC envelope.
    let modified_response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": resp.get("id").cloned().unwrap_or(serde_json::json!(1)),
        "result": {
            "content": [{ "type": "text", "text": tunnel_text }],
            "isError": false
        }
    });
    serde_json::to_vec(&modified_response).ok()
}

/// Bidirectional byte-level splice between a libp2p stream and a TCP stream.
///
/// The libp2p side speaks `futures::io` while the TCP side speaks `tokio::io`.
/// We split each into read/write halves and shuttle bytes between them.
async fn splice(p2p: libp2p::swarm::Stream, tcp: TcpStream) {
    let (mut p2p_read, mut p2p_write) = FuturesReadExt::split(p2p);
    let (mut tcp_read, mut tcp_write) = tcp.into_split();

    let p2p_to_tcp = async {
        let mut buf = [0u8; 8192];
        loop {
            let n = FuturesReadExt::read(&mut p2p_read, &mut buf).await?;
            if n == 0 {
                break;
            }
            TokioWriteExt::write_all(&mut tcp_write, &buf[..n]).await?;
        }
        Ok::<_, std::io::Error>(())
    };

    let tcp_to_p2p = async {
        let mut buf = [0u8; 8192];
        loop {
            let n = TokioReadExt::read(&mut tcp_read, &mut buf).await?;
            if n == 0 {
                break;
            }
            FuturesWriteExt::write_all(&mut p2p_write, &buf[..n]).await?;
        }
        Ok::<_, std::io::Error>(())
    };

    tokio::select! {
        r = p2p_to_tcp => {
            if let Err(e) = r {
                tracing::debug!("Tunnel p2p→tcp ended: {e}");
            }
        }
        r = tcp_to_p2p => {
            if let Err(e) = r {
                tracing::debug!("Tunnel tcp→p2p ended: {e}");
            }
        }
    }
}
