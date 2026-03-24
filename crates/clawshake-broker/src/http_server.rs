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
    extract::{
        ws::{Message, WebSocket},
        Path as AxumPath, Query, State, WebSocketUpgrade,
    },
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Json,
    },
    routing::{get, post},
    Router,
};
use clawshake_core::protocol::{JsonRpcRequest, JsonRpcResponse};
use futures::{SinkExt, Stream, StreamExt};
use tokio::sync::{mpsc, RwLock};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    mcp_server,
    router::BrokerContext,
    webview::{self, FrameContent, WsIncoming},
};

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

type Sessions = Arc<RwLock<HashMap<String, mpsc::UnboundedSender<String>>>>;

#[derive(Clone)]
struct AppState {
    ctx: BrokerContext,
    sessions: Sessions,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Bind an MCP HTTP SSE server on `127.0.0.1:<port>` and serve forever.
/// Bind an MCP HTTP SSE server on `127.0.0.1:<port>` and serve forever.
pub async fn serve(
    broker: BrokerContext,
    notify_rx: Option<tokio::sync::mpsc::Receiver<()>>,
) -> Result<()> {
    let state = AppState {
        sessions: Arc::new(RwLock::new(HashMap::new())),
        ctx: broker,
    };

    // When the manifest registry changes, broadcast notifications/tools/list_changed
    // to every open SSE session so clients refresh their tool list immediately.
    if let Some(mut rx) = notify_rx {
        let sessions = state.sessions.clone();
        let shim_cache = state.ctx.shim_cache.clone();
        tokio::spawn(async move {
            let msg = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/tools/list_changed"
            })
            .to_string();
            while rx.recv().await.is_some() {
                shim_cache.invalidate();
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

    let port = state.ctx.port;

    let app = Router::new()
        .route("/sse", get(sse_handler))
        .route("/messages", post(messages_handler))
        .route("/", post(direct_handler))
        .route("/invoke", post(invoke_handler))
        .route("/events", post(events_handler))
        // Webview channel routes
        .route("/ui", get(ui_host_page))
        .route("/ui/frame/{id}", get(ui_frame_content))
        .route("/ui/ws", get(ui_websocket_handler))
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

    let ctx = state.ctx.clone();

    tokio::spawn(async move {
        let dispatch = ctx.as_dispatch();
        let response = match serde_json::from_str::<JsonRpcRequest>(&body) {
            Err(e) => {
                let r = JsonRpcResponse::err(None, -32700, format!("Parse error: {e}"));
                serde_json::to_string(&r).expect("JSON-RPC response serializes to string")
            }
            Ok(req) => match mcp_server::handle(&req, &dispatch).await {
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
    let ctx = state.ctx.as_dispatch();
    let response = match serde_json::from_str::<JsonRpcRequest>(&body) {
        Err(e) => JsonRpcResponse::err(None, -32700, format!("Parse error: {e}")),
        Ok(req) => {
            match mcp_server::handle(&req, &ctx).await {
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
    let ctx = state.ctx.as_dispatch();
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
    let id = state
        .ctx
        .event_queue
        .push(&req.topic, &source, req.data)
        .await;

    let resp = serde_json::json!({"ok": true, "id": id});
    debug!(resp = %resp, "→ POST /events");
    Json(resp).into_response()
}

// ---------------------------------------------------------------------------
// Webview channel routes
// ---------------------------------------------------------------------------

/// Serve the webview host page at `GET /ui`.
async fn ui_host_page() -> Html<&'static str> {
    Html(webview::HOST_PAGE)
}

/// Serve inline frame content at `GET /ui/frame/:id`.
///
/// Returns the agent-generated HTML wrapped with CSP + bridge script.
/// For `Src` frames, this endpoint is not used (iframe navigates directly).
async fn ui_frame_content(
    AxumPath(frame_id): AxumPath<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let frame = state.ctx.frame_store.get(&frame_id).await;
    match frame {
        Some(f) => match &f.content {
            FrameContent::Inline { html, css, js } => {
                let body = webview::build_inline_frame(html, css, js);
                (
                    StatusCode::OK,
                    [("content-type", "text/html; charset=utf-8")],
                    body,
                )
                    .into_response()
            }
            FrameContent::Src(url) => {
                // Redirect to the src URL — shouldn't normally hit this path
                // since the host page sets iframe.src directly.
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [("location", url.as_str())],
                    String::new(),
                )
                    .into_response()
            }
        },
        None => (StatusCode::NOT_FOUND, "Frame not found").into_response(),
    }
}

/// WebSocket handler for the webview channel at `WS /ui/ws`.
///
/// The host page connects here. The broker pushes render/push/close/snapshot
/// messages, and the host page sends back interaction events and snapshot
/// responses.
async fn ui_websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ui_websocket(socket, state))
}

async fn ui_websocket(socket: WebSocket, state: AppState) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Channel for broker → host page messages.
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    // Replay all currently-open frames to the newly connected host page
    // before registering the sender so it sees the full current state.
    let port = state.ctx.port;
    let existing = state.ctx.frame_store.list_all().await;
    for (frame_id, frame) in existing {
        let src = match &frame.content {
            webview::FrameContent::Inline { .. } => {
                format!("http://127.0.0.1:{port}/ui/frame/{frame_id}")
            }
            webview::FrameContent::Src(url) => url.clone(),
        };
        let msg = webview::WsOutgoing::Render {
            frame_id,
            src,
            title: frame.title,
            width: frame.width,
            height: frame.height,
        };
        if let Ok(json) = serde_json::to_string(&msg) {
            let _ = tx.send(json);
        }
    }

    state.ctx.frame_store.add_ws_sender(tx).await;

    debug!("Webview WebSocket connected");

    // Forward outgoing messages from the broker to the WebSocket.
    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_tx.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    // Read incoming messages from the host page.
    let frame_store = state.ctx.frame_store.clone();
    let event_queue = state.ctx.event_queue.clone();

    while let Some(Ok(msg)) = ws_rx.next().await {
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Close(_) => break,
            _ => continue,
        };

        // Size guard — drop messages > 64 KB.
        if text.len() > 65536 {
            continue;
        }

        let incoming: WsIncoming = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(_) => continue,
        };

        match incoming {
            WsIncoming::Interaction {
                frame_id,
                event,
                id,
                data,
            } => {
                // Push into the event queue as a channel.ui event.
                let payload = serde_json::json!({
                    "frame_id": frame_id,
                    "event": event,
                    "id": id,
                    "data": data,
                });
                event_queue.push("channel.ui", "webview", payload).await;
            }
            WsIncoming::Close { frame_id } => {
                frame_store.remove(&frame_id).await;
            }
            WsIncoming::SnapshotResponse {
                request_id,
                result,
                error,
            } => {
                let res = match (result, error) {
                    (Some(text), _) => Ok(text),
                    (_, Some(err)) => Err(err),
                    _ => Err("empty snapshot response".to_string()),
                };
                frame_store.resolve_snapshot(&request_id, res).await;
            }
        }
    }

    // Clean up.
    send_task.abort();
    frame_store.prune_senders().await;
    debug!("Webview WebSocket disconnected");
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
