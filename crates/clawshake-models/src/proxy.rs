//! Local OpenAI-compatible proxy server.
//!
//! Exposes `GET /v1/models` and `POST /v1/chat/completions` on a local port.
//! Applications point `OPENAI_BASE_URL` at this address and get transparent
//! access to models running on any peer in the P2P network.
//!
//! Model IDs use the format `<model_name>@<peer_id>`.

use std::sync::Arc;

use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use clawshake_core::network_channel::OutboundStreamCallTx;
use clawshake_core::peer_table::PeerTable;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{info, warn};

/// Shared state for the proxy server.
pub struct ProxyState {
    /// Peer table — used to discover models from DHT announcements.
    pub peer_table: Arc<PeerTable>,
    /// Channel to send outbound stream calls through the P2P swarm.
    pub stream_call_tx: OutboundStreamCallTx,
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
    messages: Vec<MessageInput>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    max_tokens: Option<u64>,
    /// Whether to stream the response. Currently always collected into a
    /// single response for P2P transport; SSE streaming will be added later.
    #[serde(default)]
    #[allow(dead_code)]
    stream: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct MessageInput {
    role: String,
    content: String,
}

/// `POST /v1/chat/completions` — route to the appropriate peer.
///
/// Parses the `model` field as `model_name@peer_id`, opens a P2P stream
/// to that peer, and forwards the completion request.
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

    // Build the ModelRequest to send over P2P
    let model_request = clawshake_core::models::ModelRequest {
        model: model_name.clone(),
        messages: req
            .messages
            .into_iter()
            .map(|m| clawshake_core::models::Message {
                role: m.role,
                content: m.content,
            })
            .collect(),
        temperature: req.temperature,
        max_tokens: req.max_tokens,
        stream: true, // always stream over P2P, we collect on the peer side
    };

    let request_bytes = match serde_json::to_vec(&model_request) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": {
                        "message": format!("Failed to serialize request: {e}"),
                        "type": "server_error",
                    }
                })),
            )
                .into_response();
        }
    };

    // Send through the P2P stream protocol and await the response.
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    let call = clawshake_core::network_channel::OutboundStreamCall {
        peer_id: peer_id.clone(),
        request: request_bytes,
        response_tx,
    };

    if state.stream_call_tx.send(call).await.is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": {
                    "message": "P2P transport is not available",
                    "type": "server_error",
                }
            })),
        )
            .into_response();
    }

    // Wait for the response from the peer.
    match response_rx.await {
        Ok(Ok(response_bytes)) => {
            // The peer returns an OpenAI-compatible JSON response — pass it through.
            match serde_json::from_slice::<Value>(&response_bytes) {
                Ok(response_json) => {
                    // Check if it's an error response from the peer
                    if response_json.get("error").is_some() {
                        return (
                            StatusCode::BAD_GATEWAY,
                            Json(response_json),
                        )
                            .into_response();
                    }
                    info!(model = %model_name, peer = %peer_id, "Model completion received from peer");
                    (StatusCode::OK, Json(response_json)).into_response()
                }
                Err(e) => {
                    warn!("Failed to parse peer response as JSON: {e}");
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
        Ok(Err(err)) => {
            warn!(peer = %peer_id, "P2P stream call failed: {err}");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": {
                        "message": format!("P2P call failed: {err}"),
                        "type": "server_error",
                    }
                })),
            )
                .into_response()
        }
        Err(_) => {
            (
                StatusCode::GATEWAY_TIMEOUT,
                Json(json!({
                    "error": {
                        "message": "P2P call was dropped (peer may have disconnected)",
                        "type": "server_error",
                    }
                })),
            )
                .into_response()
        }
    }
}
