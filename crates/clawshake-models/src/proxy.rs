//! Local OpenAI-compatible proxy server.
//!
//! Exposes `GET /v1/models` and `POST /v1/chat/completions` on a local port.
//! Applications point `OPENAI_BASE_URL` at this address and get transparent
//! access to models running on any peer in the P2P network.
//!
//! When the client requests `stream: true`, the proxy opens a bidirectional
//! P2P stream (via `libp2p-stream`) and re-emits each model delta as an SSE
//! event — giving real-time, token-by-token streaming over the network.
//!
//! Model IDs use the format `<model_name>@<peer_id>`.

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
use clawshake_core::network_channel::{OutboundModelStreamingCallTx, OutboundStreamCallTx};
use clawshake_core::peer_table::PeerTable;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{info, warn};

/// Shared state for the proxy server.
pub struct ProxyState {
    /// Peer table — used to discover models from DHT announcements.
    pub peer_table: Arc<PeerTable>,
    /// Channel for non-streaming model calls (request-response).
    pub stream_call_tx: OutboundStreamCallTx,
    /// Channel for streaming model calls (libp2p-stream, frame-by-frame).
    pub model_streaming_tx: OutboundModelStreamingCallTx,
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
    /// Whether to stream the response as SSE events.
    #[serde(default)]
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
///
/// When `stream: true`, returns `text/event-stream` (SSE) with real-time
/// token deltas forwarded from the remote model backend.  Otherwise returns
/// a single JSON response.
async fn chat_completions(
    State(state): State<Arc<ProxyState>>,
    Json(req): Json<CompletionRequest>,
) -> impl IntoResponse {
    let wants_streaming = req.stream.unwrap_or(false);

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
        stream: true, // always stream over P2P, we collect on the peer side if needed
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

    if wants_streaming {
        chat_completions_streaming(state, model_name, peer_id, request_bytes)
            .await
            .into_response()
    } else {
        chat_completions_buffered(state, model_name, peer_id, request_bytes)
            .await
            .into_response()
    }
}

/// Non-streaming path: send request-response, wait for full result, return JSON.
async fn chat_completions_buffered(
    state: Arc<ProxyState>,
    model_name: String,
    peer_id: String,
    request_bytes: Vec<u8>,
) -> impl IntoResponse {
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

    match response_rx.await {
        Ok(Ok(response_bytes)) => {
            match serde_json::from_slice::<Value>(&response_bytes) {
                Ok(response_json) => {
                    if response_json.get("error").is_some() {
                        return (StatusCode::BAD_GATEWAY, Json(response_json)).into_response();
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
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(json!({
                "error": {
                    "message": "P2P call was dropped (peer may have disconnected)",
                    "type": "server_error",
                }
            })),
        )
            .into_response(),
    }
}

/// Streaming path: open a libp2p-stream, read frames, emit SSE events.
async fn chat_completions_streaming(
    state: Arc<ProxyState>,
    model_name: String,
    peer_id: String,
    request_bytes: Vec<u8>,
) -> impl IntoResponse {
    // Create an mpsc channel for streaming frames from the P2P layer.
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, String>>(64);

    let call = clawshake_core::network_channel::OutboundModelStreamingCall {
        peer_id: peer_id.clone(),
        request: request_bytes,
        frame_tx,
    };

    if state.model_streaming_tx.send(call).await.is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": {
                    "message": "P2P streaming transport is not available",
                    "type": "server_error",
                }
            })),
        )
            .into_response();
    }

    info!(model = %model_name, peer = %peer_id, "Starting SSE stream");

    // Build an SSE response body from the incoming frames.
    let sse_stream = async_stream::stream! {
        while let Some(result) = frame_rx.recv().await {
            match result {
                Ok(frame_bytes) => {
                    // Each frame is a StreamFrame JSON object.
                    // Parse to check type and re-emit as OpenAI SSE.
                    match clawshake_core::stream::StreamFrame::from_bytes(&frame_bytes) {
                        Ok(clawshake_core::stream::StreamFrame::Chunk { data, .. }) => {
                            // Forward the OpenAI delta JSON as an SSE data event.
                            let json_str = serde_json::to_string(&data).unwrap_or_default();
                            yield Ok::<_, std::convert::Infallible>(
                                format!("data: {json_str}\n\n")
                            );
                        }
                        Ok(clawshake_core::stream::StreamFrame::Done { .. }) => {
                            yield Ok(format!("data: [DONE]\n\n"));
                            break;
                        }
                        Ok(clawshake_core::stream::StreamFrame::Error { message, .. }) => {
                            let err = serde_json::json!({
                                "error": { "message": message, "type": "server_error" }
                            });
                            yield Ok(format!("data: {}\n\n", serde_json::to_string(&err).unwrap_or_default()));
                            yield Ok(format!("data: [DONE]\n\n"));
                            break;
                        }
                        Err(e) => {
                            warn!("Failed to parse stream frame: {e}");
                        }
                    }
                }
                Err(err) => {
                    let err_json = serde_json::json!({
                        "error": { "message": err, "type": "server_error" }
                    });
                    yield Ok(format!("data: {}\n\n", serde_json::to_string(&err_json).unwrap_or_default()));
                    yield Ok(format!("data: [DONE]\n\n"));
                    break;
                }
            }
        }
    };

    let body = Body::from_stream(sse_stream);

    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .body(body)
        .unwrap()
        .into_response()
}
