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
    event_queue::EventQueue,
    invoke::codemode::ShimCache,
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
    shim_cache: ShimCache,
    port: u16,
    code_mode: bool,
    event_queue: EventQueue,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Bind an MCP HTTP SSE server on `127.0.0.1:<port>` and serve forever.
/// Bind an MCP HTTP SSE server on `127.0.0.1:<port>` and serve forever.
pub async fn serve(
    port: u16,
    registry: ManifestRegistry,
    permissions: PermissionStore,
    servers: McpServerMap,
    notify_rx: Option<tokio::sync::mpsc::Receiver<()>>,
    shim_cache: ShimCache,
    code_mode: bool,
    event_queue: EventQueue,
) -> Result<()> {
    let state = AppState {
        registry,
        permissions,
        sessions: Arc::new(RwLock::new(HashMap::new())),
        servers,
        shim_cache,
        port,
        code_mode,
        event_queue,
    };

    // When the manifest registry changes, broadcast notifications/tools/list_changed
    // to every open SSE session so clients refresh their tool list immediately.
    if let Some(mut rx) = notify_rx {
        let sessions = state.sessions.clone();
        tokio::spawn(async move {
            let msg = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/tools/list_changed"
            })
            .to_string();
            while rx.recv().await.is_some() {
                let sessions = sessions.read().await;
                let count = sessions.len();
                for tx in sessions.values() {
                    let _ = tx.send(msg.clone());
                }
                if count > 0 {
                    debug!("notifications/tools/list_changed → {count} session(s)");
                }
            }
        });
    }

    let app = Router::new()
        .route("/sse", get(sse_handler))
        .route("/messages", post(messages_handler))
        .route("/", post(direct_handler))
        .route("/invoke", post(invoke_handler))
        .route("/events", post(events_handler))
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
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(5))
            .text("ping"),
    )
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
    let shim_cache = state.shim_cache.clone();
    let port = state.port;
    let code_mode = state.code_mode;
    let event_queue = state.event_queue.clone();

    tokio::spawn(async move {
        let response = match serde_json::from_str::<JsonRpcRequest>(&body) {
            Err(e) => {
                let r = JsonRpcResponse::err(None, -32700, format!("Parse error: {e}"));
                serde_json::to_string(&r).expect("JSON-RPC response serializes to string")
            }
            Ok(req) => match mcp_server::handle(
                &req,
                &registry,
                &permissions,
                &servers,
                &shim_cache,
                port,
                code_mode,
                &event_queue,
            )
            .await
            {
                Some(resp) => {
                    serde_json::to_string(&resp).expect("JSON-RPC response serializes to string")
                }
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
            match mcp_server::handle(
                &req,
                &state.registry,
                &state.permissions,
                &state.servers,
                &state.shim_cache,
                state.port,
                state.code_mode,
                &state.event_queue,
            )
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
    Json(serde_json::to_value(response).expect("response serializes to JSON")).into_response()
}

// ---------------------------------------------------------------------------
// POST /invoke — synchronous tool dispatch for code-mode Node.js callbacks
// ---------------------------------------------------------------------------

/// Synchronous REST endpoint used by the Node.js subprocess spawned in
/// code mode.  Receives a tool name and arguments, dispatches through
/// `router::dispatch`, and returns the result as JSON.
///
/// Request:  `{"tool": "mail_send", "arguments": {"to": "...", ...}}`
/// Response: `{"result": "...", "is_error": false}`
async fn invoke_handler(State(state): State<AppState>, body: String) -> axum::response::Response {
    debug!(body = %body, "← POST /invoke");
    let ctx = crate::router::DispatchContext {
        registry: &state.registry, servers: &state.servers,
        event_queue: &state.event_queue, permissions: &state.permissions,
        shim_cache: &state.shim_cache, port: state.port,
    };
    crate::router::dispatch_invoke(&body, &ctx).await
}

// ---------------------------------------------------------------------------
// POST /events — external event ingestion
// ---------------------------------------------------------------------------

/// Accept an event from an external adapter (webhook bot, sidecar daemon,
/// etc.) and push it into the local EventQueue.
///
/// Request:  `{"topic": "telegram.message", "data": {...}, "source": "telegram-bot"}`
/// Response: `{"ok": true, "id": 42}`
///
/// `source` is optional — defaults to `"webhook"`.
/// Localhost-only (same bind as all other endpoints).
async fn events_handler(State(state): State<AppState>, body: String) -> impl IntoResponse {
    debug!(body = %body, "← POST /events");

    #[derive(serde::Deserialize)]
    struct EventRequest {
        topic: String,
        #[serde(default)]
        data: serde_json::Value,
        #[serde(default)]
        source: Option<String>,
    }

    let req: EventRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            let resp = serde_json::json!({"ok": false, "error": format!("Bad request: {e}")});
            return (StatusCode::BAD_REQUEST, Json(resp)).into_response();
        }
    };

    let source = req.source.unwrap_or_else(|| "webhook".to_string());
    let id = state.event_queue.push(&req.topic, &source, req.data).await;

    let resp = serde_json::json!({"ok": true, "id": id});
    debug!(resp = %resp, "→ POST /events");
    Json(resp).into_response()
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
