//! MCP HTTP SSE transport for `clawshake-broker --port <N>`.
//!
//! Implements the MCP 2024-11-05 HTTP+SSE transport:
//!
//!   GET  /sse                         — open SSE stream, receive `endpoint` event
//!   POST /messages?sessionId=<uuid>   — send a JSON-RPC request
//!
//! Flow:
//!   1. Client GETs `/sse` → receives `event: endpoint\ndata: /messages?sessionId=<id>`
//!   2. Client POSTs JSON-RPC to `/messages?sessionId=<id>`
//!   3. Server processes it and pushes `event: message\ndata: <json-rpc-response>` on the SSE stream
//!
//! VS Code MCP config:
//!   { "type": "sse", "url": "http://127.0.0.1:<port>/sse" }

use std::{
    collections::HashMap,
    convert::Infallible,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use anyhow::Result;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Json,
    },
    routing::{get, post},
    Router,
};
use clawshake_core::{
    permissions::PermissionStore,
    protocol::{JsonRpcRequest, JsonRpcResponse},
};
use futures::{Stream, StreamExt};
use tokio::sync::{mpsc, RwLock};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    mcp_server,
    watcher::{ManifestRegistry, McpServerMap},
};

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

type Sessions = Arc<RwLock<HashMap<String, mpsc::UnboundedSender<String>>>>;

#[derive(Clone)]
struct AppState {
    registry: ManifestRegistry,
    permissions: PermissionStore,
    sessions: Sessions,
    servers: McpServerMap,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Bind an MCP HTTP SSE server on `127.0.0.1:<port>` and serve forever.
pub async fn serve(
    port: u16,
    registry: ManifestRegistry,
    permissions: PermissionStore,
    servers: McpServerMap,
) -> Result<()> {
    let state = AppState {
        registry,
        permissions,
        sessions: Arc::new(RwLock::new(HashMap::new())),
        servers,
    };

    let app = Router::new()
        .route("/sse", get(sse_handler))
        .route("/messages", post(messages_handler))
        .route("/", post(direct_handler))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    info!("MCP HTTP server on http://{addr}/sse");
    info!("Add to VS Code settings.json: {{ \"type\": \"sse\", \"url\": \"http://{addr}/sse\" }}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// GET /sse
// ---------------------------------------------------------------------------

/// Open an SSE stream.
///
/// Immediately emits one `endpoint` event pointing the client to the POST URL
/// for this session, then streams `message` events for each JSON-RPC response.
async fn sse_handler(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let session_id = Uuid::new_v4().to_string();
    let (tx, rx) = mpsc::unbounded_channel::<String>();

    state.sessions.write().await.insert(session_id.clone(), tx);
    debug!(session = %session_id, "SSE session opened");

    // Tell the client where to POST requests.
    let endpoint_event = Ok(Event::default()
        .event("endpoint")
        .data(format!("/messages?sessionId={session_id}")));

    // Subsequent events: JSON-RPC responses pushed by `messages_handler`.
    let rx_stream = UnboundedReceiverStream::new(rx)
        .map(|data| Ok(Event::default().event("message").data(data)));

    // Wrap rx_stream so the session is removed from the map on disconnect.
    let response_stream = CleanupStream {
        inner: rx_stream,
        sessions: state.sessions.clone(),
        session_id,
    };

    let stream = futures::stream::once(std::future::ready(endpoint_event)).chain(response_stream);
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ---------------------------------------------------------------------------
// POST /messages
// ---------------------------------------------------------------------------

/// Receive a JSON-RPC request from the client.
///
/// Handles the request asynchronously and pushes the response back on the
/// session's SSE stream.  Returns 202 Accepted immediately.
async fn messages_handler(
    Query(params): Query<HashMap<String, String>>,
    State(state): State<AppState>,
    body: String,
) -> StatusCode {
    let session_id = match params.get("sessionId") {
        Some(id) => id.clone(),
        None => {
            warn!("POST /messages missing sessionId query param");
            return StatusCode::BAD_REQUEST;
        }
    };

    let tx = {
        let sessions = state.sessions.read().await;
        sessions.get(&session_id).cloned()
    };

    let tx = match tx {
        Some(tx) => tx,
        None => {
            warn!(session = %session_id, "POST /messages: unknown session");
            return StatusCode::NOT_FOUND;
        }
    };

    debug!(session = %session_id, body = %body, "← POST /messages");

    let registry = state.registry.clone();
    let permissions = state.permissions.clone();
    let servers = state.servers.clone();

    tokio::spawn(async move {
        let response = match serde_json::from_str::<JsonRpcRequest>(&body) {
            Err(e) => {
                let r = JsonRpcResponse::err(None, -32700, format!("Parse error: {e}"));
                serde_json::to_string(&r).unwrap()
            }
            Ok(req) => match mcp_server::handle(&req, &registry, &permissions, &servers).await {
                Some(resp) => serde_json::to_string(&resp).unwrap(),
                None => return, // notification — no response needed
            },
        };

        debug!(json = %response, "→ SSE message");
        let _ = tx.send(response);
    });

    StatusCode::ACCEPTED
}

// ---------------------------------------------------------------------------
// POST / — stateless direct JSON-RPC (used by clawshake-bridge --mcp-port)
// ---------------------------------------------------------------------------

/// Handle a single JSON-RPC request synchronously and return the response
/// directly as JSON.  No session or SSE stream required.
///
/// This is the transport used by `clawshake-bridge --mcp-port <N>`, which
/// POSTs JSON-RPC to the server root and expects a JSON response body.
async fn direct_handler(State(state): State<AppState>, body: String) -> impl IntoResponse {
    debug!(body = %body, "← POST /");
    let response = match serde_json::from_str::<JsonRpcRequest>(&body) {
        Err(e) => JsonRpcResponse::err(None, -32700, format!("Parse error: {e}")),
        Ok(req) => {
            match mcp_server::handle(&req, &state.registry, &state.permissions, &state.servers)
                .await
            {
                Some(resp) => resp,
                // Notification — no response body; return empty 204.
                None => {
                    return (StatusCode::NO_CONTENT, Json(serde_json::Value::Null)).into_response()
                }
            }
        }
    };
    debug!(resp = ?response, "→ POST /");
    Json(serde_json::to_value(response).unwrap()).into_response()
}
// ---------------------------------------------------------------------------

struct CleanupStream<S> {
    inner: S,
    sessions: Sessions,
    session_id: String,
}

impl<S, I> Stream for CleanupStream<S>
where
    S: Stream<Item = I> + Unpin,
{
    type Item = I;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<I>> {
        self.inner.poll_next_unpin(cx)
    }
}

impl<S> Drop for CleanupStream<S> {
    fn drop(&mut self) {
        let sessions = self.sessions.clone();
        let id = self.session_id.clone();
        tokio::spawn(async move {
            sessions.write().await.remove(&id);
            debug!(session = %id, "SSE session cleaned up");
        });
    }
}
