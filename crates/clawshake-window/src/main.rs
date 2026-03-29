//! Tauri v2 window shell for clawshake.
//!
//! Opens a native window serving a local frontend that communicates with
//! the clawshake broker via a Rust-side WebSocket bridge.  The host page
//! JS never touches the network directly — all communication flows through
//! Tauri IPC:
//!
//!   Broker WS  ←——→  Window Rust (WS client)  ←—IPC—→  Host Page JS
//!
//! Window control messages (open/close/resize/title/focus/notify) from the
//! broker are handled entirely in Rust via the Tauri window API — the JS
//! layer only deals with iframe management (render/push/close/snapshot).
//!
//! Usage:
//!   clawshake-window                  # defaults to port 7475
//!   clawshake-window --port 8080      # custom broker port

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// State shared between Tauri commands and the WS bridge task
// ---------------------------------------------------------------------------

/// A handle to send messages from JS → broker over the WebSocket.
type WsTx = Arc<
    Mutex<
        Option<
            futures_util::stream::SplitSink<
                tokio_tungstenite::WebSocketStream<
                    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
                >,
                Message,
            >,
        >,
    >,
>;

struct BridgeState {
    ws_tx: WsTx,
    broker_port: u16,
    /// Messages buffered before JS registered its listener.
    pending: Mutex<Vec<String>>,
    /// Set to true once JS calls `client_ready`.
    ready: AtomicBool,
}

// ---------------------------------------------------------------------------
// Tauri commands (JS → Rust)
// ---------------------------------------------------------------------------

/// Send a raw JSON string from the host page to the broker via WS.
#[tauri::command]
async fn send_to_broker(state: tauri::State<'_, BridgeState>, msg: String) -> Result<(), String> {
    let mut guard = state.ws_tx.lock().await;
    if let Some(tx) = guard.as_mut() {
        tx.send(Message::Text(msg.into()))
            .await
            .map_err(|e| e.to_string())
    } else {
        Err("WebSocket not connected".into())
    }
}

/// Return the broker port so the host page can construct iframe src URLs.
#[tauri::command]
fn get_broker_port(state: tauri::State<'_, BridgeState>) -> u16 {
    state.broker_port
}

/// Called by JS after it registers its `listen('broker-msg')` handler.
/// Returns any messages that arrived before the listener was ready so JS
/// can process them synchronously — avoids the race where `app.emit` in a
/// command handler doesn't reach a just-registered Tauri event listener.
#[tauri::command]
async fn client_ready(state: tauri::State<'_, BridgeState>) -> Result<Vec<String>, String> {
    state.ready.store(true, Ordering::SeqCst);
    let pending: Vec<String> = {
        let mut buf = state.pending.lock().await;
        std::mem::take(&mut *buf)
    };
    info!("JS ready — returning {} buffered messages", pending.len());
    Ok(pending)
}

// ---------------------------------------------------------------------------
// Broker message types (subset needed for window control)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
struct BrokerMsg {
    r#type: String,
    // Window control fields (optional — only present in window_* messages)
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
}

// ---------------------------------------------------------------------------
// WebSocket bridge task
// ---------------------------------------------------------------------------

/// Connect to the broker's WS endpoint and bridge messages.
///
/// - Broker → Window: window_* messages handled in Rust; everything else
///   emitted as a `broker-msg` Tauri event for the JS host page.
/// - Window → Broker: the `send_to_broker` command writes to `ws_tx`.
async fn ws_bridge(app: AppHandle, port: u16, ws_tx: WsTx) {
    let url = format!("ws://127.0.0.1:{port}/ui/ws");

    loop {
        info!("Connecting to broker at {url}");
        let conn = tokio_tungstenite::connect_async(&url).await;

        match conn {
            Ok((stream, _response)) => {
                info!("Connected to broker WebSocket");
                let (write, mut read) = stream.split();

                // Store the write half so Tauri commands can send messages.
                {
                    let mut guard = ws_tx.lock().await;
                    *guard = Some(write);
                }

                // Read messages from broker.
                while let Some(result) = read.next().await {
                    match result {
                        Ok(Message::Text(text)) => {
                            let text = text.to_string();
                            // Try to parse as a broker message to check for window control.
                            if let Ok(msg) = serde_json::from_str::<BrokerMsg>(&text) {
                                match msg.r#type.as_str() {
                                    "window_open" => {
                                        handle_window_open(&app, &msg);
                                    }
                                    "window_close" => {
                                        handle_window_close(&app, &msg);
                                    }
                                    "window_resize" => {
                                        handle_window_resize(&app, &msg);
                                    }
                                    "window_set_title" => {
                                        handle_window_set_title(&app, &msg);
                                    }
                                    "window_focus" => {
                                        handle_window_focus(&app, &msg);
                                    }
                                    "window_notify" => {
                                        handle_window_notify(&app, &msg);
                                    }
                                    "window_list_request" => {
                                        handle_window_list(&app, &msg, &ws_tx).await;
                                    }
                                    // All other messages (render, push, close, snapshot_request)
                                    // → forward to JS via Tauri event, or buffer if not ready.
                                    // If it's a render/push, ensure a window exists first.
                                    _ => {
                                        if msg.r#type == "render" || msg.r#type == "push" {
                                            ensure_main_window(&app);
                                        }
                                        emit_or_buffer(&app, text.clone()).await;
                                    }
                                }
                            }
                        }
                        Ok(Message::Close(_)) => {
                            info!("Broker closed WebSocket");
                            break;
                        }
                        Err(e) => {
                            warn!("WebSocket read error: {e}");
                            break;
                        }
                        _ => {} // Ping/Pong/Binary — ignore
                    }
                }

                // Clear the write half on disconnect.
                {
                    let mut guard = ws_tx.lock().await;
                    *guard = None;
                }
                warn!("Disconnected from broker, reconnecting in 2s…");
            }
            Err(e) => {
                warn!("Failed to connect to broker: {e}");
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

// ---------------------------------------------------------------------------
// Window control handlers (Rust-native, no JS involvement)
// ---------------------------------------------------------------------------

/// Emit a broker message to JS, or buffer it if JS isn't ready yet.
async fn emit_or_buffer(app: &AppHandle, text: String) {
    let state: tauri::State<'_, BridgeState> = app.state();
    if state.ready.load(Ordering::SeqCst) {
        if let Err(e) = app.emit("broker-msg", &text) {
            warn!("Failed to emit broker-msg: {e}");
        }
    } else {
        state.pending.lock().await.push(text);
    }
}

/// Ensure the main window exists.  Called lazily when the first
/// render/push arrives.  No-op if `"main"` already exists.
fn ensure_main_window(app: &AppHandle) {
    if app.get_webview_window("main").is_some() {
        return;
    }
    match WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("clawshake")
        .inner_size(1200.0, 800.0)
        .min_inner_size(600.0, 400.0)
        .build()
    {
        Ok(_) => info!("Created main window on first render"),
        Err(e) => error!("Failed to create main window: {e}"),
    }
}

fn handle_window_open(app: &AppHandle, msg: &BrokerMsg) {
    let label = msg.label.as_deref().unwrap_or("secondary");
    let title = msg.title.as_deref().unwrap_or("clawshake");
    let width = msg.width.unwrap_or(1200) as f64;
    let height = msg.height.unwrap_or(800) as f64;

    // Guard: skip if a window with this label already exists.
    if app.get_webview_window(label).is_some() {
        debug!("Window '{label}' already exists — skipping open");
        return;
    }

    let url = if let Some(u) = &msg.url {
        match u.parse::<url::Url>() {
            Ok(parsed) => WebviewUrl::External(parsed),
            Err(_) => WebviewUrl::App("index.html".into()),
        }
    } else {
        WebviewUrl::App("index.html".into())
    };

    match WebviewWindowBuilder::new(app, label, url)
        .title(title)
        .inner_size(width, height)
        .build()
    {
        Ok(_) => debug!("Opened window '{label}'"),
        Err(e) => error!("Failed to open window '{label}': {e}"),
    }
}

fn handle_window_close(app: &AppHandle, msg: &BrokerMsg) {
    let label = msg.label.as_deref().unwrap_or("main");
    if let Some(win) = app.get_webview_window(label) {
        if let Err(e) = win.close() {
            error!("Failed to close window '{label}': {e}");
        }
    }
}

fn handle_window_resize(app: &AppHandle, msg: &BrokerMsg) {
    let label = msg.label.as_deref().unwrap_or("main");
    let width = msg.width.unwrap_or(1200);
    let height = msg.height.unwrap_or(800);
    if let Some(win) = app.get_webview_window(label) {
        let size = tauri::LogicalSize::new(width as f64, height as f64);
        if let Err(e) = win.set_size(tauri::Size::Logical(size)) {
            error!("Failed to resize window '{label}': {e}");
        }
    }
}

fn handle_window_set_title(app: &AppHandle, msg: &BrokerMsg) {
    let label = msg.label.as_deref().unwrap_or("main");
    let title = msg.title.as_deref().unwrap_or("clawshake");
    if let Some(win) = app.get_webview_window(label) {
        if let Err(e) = win.set_title(title) {
            error!("Failed to set title on '{label}': {e}");
        }
    }
}

fn handle_window_focus(app: &AppHandle, msg: &BrokerMsg) {
    let label = msg.label.as_deref().unwrap_or("main");
    if let Some(win) = app.get_webview_window(label) {
        if let Err(e) = win.set_focus() {
            error!("Failed to focus window '{label}': {e}");
        }
    }
}

fn handle_window_notify(app: &AppHandle, msg: &BrokerMsg) {
    let title = msg.title.as_deref().unwrap_or("clawshake");
    let body = msg.body.as_deref().unwrap_or("");

    // Use tauri-plugin-notification.
    use tauri_plugin_notification::NotificationExt;
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        error!("Failed to show notification: {e}");
    }
}

/// Respond to the broker's `window_list_request` with actual Tauri window data.
async fn handle_window_list(app: &AppHandle, msg: &BrokerMsg, ws_tx: &WsTx) {
    let request_id = msg.request_id.as_deref().unwrap_or("");
    let windows: Vec<serde_json::Value> = app
        .webview_windows()
        .iter()
        .map(|(label, win)| {
            let (w, h) = win
                .inner_size()
                .map(|s| (s.width, s.height))
                .unwrap_or((0, 0));
            let title = win.title().unwrap_or_default();
            serde_json::json!({
                "label": label,
                "title": title,
                "width": w,
                "height": h,
            })
        })
        .collect();
    let resp = serde_json::json!({
        "type": "window_list_response",
        "request_id": request_id,
        "windows": windows,
    });
    let mut guard = ws_tx.lock().await;
    if let Some(tx) = guard.as_mut() {
        let _ = tx.send(Message::Text(resp.to_string().into())).await;
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "clawshake_window=info".into()),
        )
        .init();

    // Parse --port flag (default: 7475)
    let port = std::env::args()
        .position(|a| a == "--port")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(7475);

    let ws_tx: WsTx = Arc::new(Mutex::new(None));

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .manage(BridgeState {
            ws_tx: ws_tx.clone(),
            broker_port: port,
            pending: Mutex::new(Vec::new()),
            ready: AtomicBool::new(false),
        })
        .invoke_handler(tauri::generate_handler![
            send_to_broker,
            get_broker_port,
            client_ready
        ])
        .setup(move |app| {
            // No default window — we're a window server.
            // Windows are created on demand via broker messages (window_open / ui_render).

            // Spawn the WS bridge task on Tauri's async runtime.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(ws_bridge(handle, port, ws_tx));

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build clawshake-window")
        .run(|_app, event| {
            // Prevent exit when all windows are closed — we're a persistent
            // window server that creates/destroys windows on demand.
            if let tauri::RunEvent::ExitRequested { api, .. } = event {
                api.prevent_exit();
            }
        });
}
