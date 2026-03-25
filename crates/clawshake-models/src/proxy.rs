//! Local OpenAI-compatible proxy server.
//!
//! Exposes `GET /v1/models` and `POST /v1/chat/completions` on a local port.
//! Applications point `OPENAI_BASE_URL` at this address and get transparent
//! access to models running on any peer in the P2P network.
//!
//! The proxy establishes TCP tunnels to remote model backends via
//! `connect_models` (P2P tunnel infrastructure) and then proxies raw HTTP —
//! both streaming (SSE) and non-streaming work natively over the tunnel.
//!
//! Model IDs use the format `<model_name>@<peer_id>`.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use axum::{
    body::Body,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use clawshake_core::network_channel::OutboundCallTx;
use clawshake_core::peer_table::PeerTable;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Shared state for the proxy server.
pub struct ProxyState {
    /// Peer table — used to discover models from DHT announcements.
    pub peer_table: Arc<PeerTable>,
    /// Channel for outbound P2P calls (used to call `connect_models`).
    pub call_tx: OutboundCallTx,
    /// Cache of established tunnels: `peer_id → local_url`.
    tunnels: RwLock<HashMap<String, String>>,
    /// Reusable HTTP client for upstream requests.
    client: reqwest::Client,
}

impl ProxyState {
    /// Create a new `ProxyState` with the given peer table and call channel.
    pub fn new(peer_table: Arc<PeerTable>, call_tx: OutboundCallTx) -> Self {
        Self {
            peer_table,
            call_tx,
            tunnels: RwLock::new(HashMap::new()),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("building reqwest client"),
        }
    }
}

/// Start the local model proxy server on the given port.
///
/// This runs until the process is shut down.
pub async fn serve(state: Arc<ProxyState>, port: u16) -> Result<()> {
    let app = Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    info!(%addr, "Model proxy listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// `GET /v1/models` — aggregate model list from all known peers.
///
/// Returns models in OpenAI format with IDs as `model_name@peer_id`.
async fn list_models(State(state): State<Arc<ProxyState>>) -> Json<Value> {
    let peers = state.peer_table.all();
    let mut data = Vec::new();

    for peer in &peers {
        for model in &peer.models {
            let model_id = format!("{}@{}", model.name, peer.peer_id);
            data.push(json!({
                "id": model_id,
                "object": "model",
                "created": peer.last_seen,
                "owned_by": peer.peer_id,
            }));
        }
    }

    Json(json!({
        "object": "list",
        "data": data,
    }))
}

/// Request body for `POST /v1/chat/completions`.
#[derive(Debug, Deserialize)]
struct CompletionRequest {
    /// Model ID in `model_name@peer_id` format.
    model: String,
    messages: Vec<Value>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    max_tokens: Option<u64>,
    /// Whether to stream the response as SSE events.
    #[serde(default)]
    stream: Option<bool>,
    /// Catch-all for extra fields (top_p, etc.) to forward unchanged.
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

/// `POST /v1/chat/completions` — route to the appropriate peer.
///
/// Parses the `model` field as `model_name@peer_id`, establishes a tunnel
/// to the peer's model backend (if not already cached), and proxies the
/// HTTP request through the tunnel.
async fn chat_completions(
    State(state): State<Arc<ProxyState>>,
    Json(req): Json<CompletionRequest>,
) -> impl IntoResponse {
    // Parse model@peer_id
    let (model_name, peer_id) = match req.model.rsplit_once('@') {
        Some((m, p)) => (m.to_string(), p.to_string()),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": format!(
                            "Invalid model ID '{}'. Expected format: model_name@peer_id. \
                             Use GET /v1/models to list available models.",
                            req.model
                        ),
                        "type": "invalid_request_error",
                    }
                })),
            )
                .into_response();
        }
    };

    // Verify the peer exists in our table
    if state.peer_table.get(&peer_id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": {
                    "message": format!("Peer '{}' not found in peer table", peer_id),
                    "type": "invalid_request_error",
                }
            })),
        )
            .into_response();
    }

    // Get or establish a tunnel to the peer's model backend.
    let base_url = match get_tunnel_url(&state, &peer_id).await {
        Ok(url) => url,
        Err(e) => {
            warn!(peer = %peer_id, "Failed to establish model tunnel: {e}");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": {
                        "message": format!("Failed to connect to peer's model backend: {e}"),
                        "type": "server_error",
                    }
                })),
            )
                .into_response();
        }
    };

    // Build the upstream request body — rewrite model to bare name (no @peer).
    let mut body = json!({
        "model": model_name,
        "messages": req.messages,
    });
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(m) = req.max_tokens {
        body["max_tokens"] = json!(m);
    }
    if let Some(s) = req.stream {
        body["stream"] = json!(s);
    }
    // Forward extra fields.
    for (k, v) in &req.extra {
        body[k] = v.clone();
    }

    let url = format!("{base_url}/v1/chat/completions");
    let wants_streaming = req.stream.unwrap_or(false);

    let client = &state.client;
    let upstream_resp = match client.post(&url).json(&body).send().await {
        Ok(r) => r,
        Err(e) => {
            // Clear cached tunnel — it may be stale.
            state.tunnels.write().await.remove(&peer_id);
            warn!(peer = %peer_id, "Upstream request to tunnel failed: {e}");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": {
                        "message": format!("Request to peer's model backend failed: {e}"),
                        "type": "server_error",
                    }
                })),
            )
                .into_response();
        }
    };

    let status = upstream_resp.status();
    if !status.is_success() && !wants_streaming {
        // Forward error responses as-is.
        let body = upstream_resp.text().await.unwrap_or_default();
        return (
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            body,
        )
            .into_response();
    }

    if wants_streaming {
        info!(model = %model_name, peer = %peer_id, "Streaming via tunnel");
        // Forward the SSE byte stream directly — no re-framing needed.
        let byte_stream = upstream_resp.bytes_stream();
        let body = Body::from_stream(byte_stream);

        axum::response::Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .header("Connection", "keep-alive")
            .body(body)
            .unwrap()
            .into_response()
    } else {
        // Non-streaming: read full JSON and return.
        match upstream_resp.json::<Value>().await {
            Ok(response_json) => {
                info!(model = %model_name, peer = %peer_id, "Model completion received via tunnel");
                (StatusCode::OK, Json(response_json)).into_response()
            }
            Err(e) => {
                warn!("Failed to read upstream response: {e}");
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({
                        "error": {
                            "message": format!("Invalid response from peer: {e}"),
                            "type": "server_error",
                        }
                    })),
                )
                    .into_response()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tunnel management
// ---------------------------------------------------------------------------

/// Get the local tunnel URL for a peer's model backend, establishing a new
/// tunnel via `connect_models` if needed.
async fn get_tunnel_url(state: &ProxyState, peer_id: &str) -> Result<String> {
    // Check cache first.
    {
        let cache = state.tunnels.read().await;
        if let Some(url) = cache.get(peer_id) {
            return Ok(url.clone());
        }
    }

    // No cached tunnel — establish one via network_call(connect_models).
    let url = establish_tunnel(state, peer_id).await?;

    // Cache for future requests.
    state.tunnels.write().await.insert(peer_id.to_string(), url.clone());

    Ok(url)
}

/// Send a `connect_models` tool call to the target peer.  The bridge
/// intercepts the tunnel-authorization response, opens the TCP tunnel,
/// and returns `{ local_port, local_url }`.
async fn establish_tunnel(state: &ProxyState, peer_id: &str) -> Result<String> {
    let json_rpc = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "connect_models",
            "arguments": {}
        }
    });
    let request_bytes = serde_json::to_vec(&json_rpc)?;

    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    let call = clawshake_core::network_channel::OutboundCall {
        peer_id: peer_id.to_string(),
        request: request_bytes,
        response_tx,
    };

    state
        .call_tx
        .send(call)
        .await
        .map_err(|_| anyhow::anyhow!("P2P transport is not available"))?;

    let response_bytes = response_rx
        .await
        .map_err(|_| anyhow::anyhow!("P2P call was dropped"))?
        .map_err(|e| anyhow::anyhow!("P2P call failed: {e}"))?;

    // Parse the MCP JSON-RPC response to extract local_url.
    let resp: Value = serde_json::from_slice(&response_bytes)
        .map_err(|e| anyhow::anyhow!("Invalid response: {e}"))?;

    // Check for JSON-RPC error.
    if let Some(err) = resp.get("error") {
        let msg = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        anyhow::bail!("Remote peer returned error: {msg}");
    }

    // Extract text content from MCP result.
    let text = resp
        .pointer("/result/content/0/text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Unexpected response format"))?;

    let inner: Value = serde_json::from_str(text)
        .map_err(|e| anyhow::anyhow!("Failed to parse tunnel response: {e}"))?;

    let local_url = inner
        .get("local_url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Response missing local_url — tunnel may not have opened"))?;

    info!(peer = %peer_id, url = %local_url, "Model tunnel established");

    Ok(local_url.to_string())
}
