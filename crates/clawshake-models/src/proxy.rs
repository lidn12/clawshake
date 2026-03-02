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
use clawshake_core::peer_table::PeerTable;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{info, warn};

/// Shared state for the proxy server.
pub struct ProxyState {
    /// Peer table — used to discover models from DHT announcements.
    pub peer_table: Arc<PeerTable>,
    // TODO: Add P2P transport handle for routing completions to peers.
    // TODO: Add permission store for model access checks.
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
#[allow(dead_code)] // Fields used when P2P transport is wired
struct CompletionRequest {
    /// Model ID in `model_name@peer_id` format.
    model: String,
    messages: Vec<MessageInput>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    stream: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
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

    // TODO: Open /clawshake/stream/1.0.0 to peer, send ModelRequest,
    // forward StreamFrame chunks back as SSE events.
    //
    // For now, return a clear error indicating the transport is not yet wired.
    warn!(
        model = %model_name,
        peer = %peer_id,
        "Model proxy routing not yet implemented"
    );

    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": {
                "message": "Model proxy P2P transport not yet implemented. \
                            The proxy server is running but cannot route requests to peers yet.",
                "type": "server_error",
            }
        })),
    )
        .into_response()
}
