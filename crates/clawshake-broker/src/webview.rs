//! Webview channel — `FrameStore`, host page, and inline frame builder.
//!
//! The broker serves a host page at `GET /ui` that manages sandboxed iframes.
//! Agents create/update frames via `ui_render` / `ui_push` tool calls, and
//! user interactions flow back through the [`EventQueue`](crate::event_queue)
//! on the `channel.ui` topic.
//!
//! Agent-generated HTML uses standard Web APIs to communicate:
//! - **Page → Agent:** `window.parent.postMessage({event, data}, '*')`
//! - **Agent → Page:** `window.addEventListener('message', ...)` (receives `ui_push` data)
//!
//! Communication between the broker and the host page uses a WebSocket
//! at `WS /ui/ws`.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Frame store
// ---------------------------------------------------------------------------

/// Content backing a single frame.
#[derive(Clone, Debug)]
pub enum FrameContent {
    /// Agent-generated HTML — served by the broker with CSP + bridge script.
    Inline {
        html: String,
        css: String,
        js: String,
    },
    /// External URL (e.g. `http://localhost:5173`) — iframe navigates directly.
    Src(String),
}

/// A single webview frame.
#[derive(Clone, Debug)]
pub struct Frame {
    pub content: FrameContent,
    pub title: String,
    pub width: u32,
    pub height: u32,
    /// Optional window label — when set, only host pages running in that
    /// window will display this frame.  `None` means broadcast to all.
    pub window: Option<String>,
}

/// Outgoing message from broker → host page over WebSocket.
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum WsOutgoing {
    #[serde(rename = "render")]
    Render {
        frame_id: String,
        src: String,
        title: String,
        width: u32,
        height: u32,
        /// When set, only the host page in this window displays the frame.
        #[serde(skip_serializing_if = "Option::is_none")]
        window: Option<String>,
    },
    #[serde(rename = "push")]
    Push { frame_id: String, data: Value },
    #[serde(rename = "close")]
    Close { frame_id: String },
    #[serde(rename = "snapshot_request")]
    SnapshotRequest {
        frame_id: String,
        format: String,
        request_id: String,
    },
    // -- Window control messages ------------------------------------------
    #[serde(rename = "window_open")]
    WindowOpen {
        label: String,
        title: String,
        url: String,
        width: u32,
        height: u32,
    },
    #[serde(rename = "window_close")]
    WindowClose { label: String },
    #[serde(rename = "window_resize")]
    WindowResize {
        label: String,
        width: u32,
        height: u32,
    },
    #[serde(rename = "window_set_title")]
    WindowSetTitle { label: String, title: String },
    #[serde(rename = "window_focus")]
    WindowFocus { label: String },
    #[serde(rename = "window_notify")]
    WindowNotify { title: String, body: String },
    #[serde(rename = "window_list_request")]
    WindowListRequest { request_id: String },
}

/// Incoming message from host page → broker over WebSocket.
#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
pub enum WsIncoming {
    #[serde(rename = "interaction")]
    Interaction {
        frame_id: String,
        event: String,
        id: String,
        #[serde(default)]
        data: Value,
    },
    #[serde(rename = "close")]
    Close { frame_id: String },
    #[serde(rename = "snapshot_response")]
    SnapshotResponse {
        request_id: String,
        #[serde(default)]
        result: Option<String>,
        #[serde(default)]
        error: Option<String>,
    },
    #[serde(rename = "window_list_response")]
    WindowListResponse {
        request_id: String,
        #[serde(default)]
        windows: Value,
    },
    /// Request a full replay of all stored frames.  Sent by `clawshake-window`
    /// when a new Tauri window opens so it receives the current frame state.
    #[serde(rename = "replay_request")]
    ReplayRequest,
}

/// In-memory store of active webview frames.
///
/// Cheaply cloneable — all state is behind `Arc<RwLock<_>>`.
#[derive(Clone)]
pub struct FrameStore {
    frames: Arc<RwLock<HashMap<String, Frame>>>,
    /// Broadcast senders for connected host pages.
    ws_senders: Arc<RwLock<Vec<mpsc::UnboundedSender<String>>>>,
    /// Pending snapshot requests awaiting a response from the host page.
    snapshot_waiters: Arc<RwLock<HashMap<String, oneshot::Sender<Result<String, String>>>>>,
    /// Pending window_list requests awaiting a response from the window server.
    list_waiters: Arc<RwLock<HashMap<String, oneshot::Sender<Value>>>>,
}

impl Default for FrameStore {
    fn default() -> Self {
        Self {
            frames: Default::default(),
            ws_senders: Default::default(),
            snapshot_waiters: Default::default(),
            list_waiters: Default::default(),
        }
    }
}

impl FrameStore {
    pub fn new() -> Self {
        Self::default()
    }

    // -- Frame CRUD -------------------------------------------------------

    pub async fn insert(&self, frame_id: String, frame: Frame) {
        self.frames.write().await.insert(frame_id, frame);
    }

    pub async fn get(&self, frame_id: &str) -> Option<Frame> {
        self.frames.read().await.get(frame_id).cloned()
    }

    pub async fn remove(&self, frame_id: &str) -> Option<Frame> {
        self.frames.write().await.remove(frame_id)
    }

    /// Return all open frames as a list of `(frame_id, Frame)` pairs.
    pub async fn list_all(&self) -> Vec<(String, Frame)> {
        self.frames
            .read()
            .await
            .iter()
            .map(|(id, f)| (id.clone(), f.clone()))
            .collect()
    }

    // -- WebSocket broadcast ----------------------------------------------

    /// Register a new WebSocket sender.
    pub async fn add_ws_sender(&self, tx: mpsc::UnboundedSender<String>) {
        self.ws_senders.write().await.push(tx);
    }

    /// Returns `true` if at least one live WS client is connected.
    /// Prunes dead senders as a side-effect.
    pub async fn has_ws_client(&self) -> bool {
        let mut senders = self.ws_senders.write().await;
        senders.retain(|tx| !tx.is_closed());
        !senders.is_empty()
    }

    /// Broadcast a message to all connected host pages.
    pub async fn broadcast(&self, msg: &WsOutgoing) {
        let json = match serde_json::to_string(msg) {
            Ok(j) => j,
            Err(_) => return,
        };
        let senders = self.ws_senders.read().await;
        for tx in senders.iter() {
            let _ = tx.send(json.clone());
        }
    }

    /// Remove closed senders (call periodically or on send failure).
    pub async fn prune_senders(&self) {
        self.ws_senders.write().await.retain(|tx| !tx.is_closed());
    }

    // -- Snapshot request/response ----------------------------------------

    /// Register a pending snapshot request.  Returns a receiver that will
    /// deliver the result when the host page responds.
    pub async fn register_snapshot(
        &self,
        request_id: String,
    ) -> oneshot::Receiver<Result<String, String>> {
        let (tx, rx) = oneshot::channel();
        self.snapshot_waiters.write().await.insert(request_id, tx);
        rx
    }

    /// Resolve a pending snapshot request with the host page's response.
    pub async fn resolve_snapshot(&self, request_id: &str, result: Result<String, String>) {
        if let Some(tx) = self.snapshot_waiters.write().await.remove(request_id) {
            let _ = tx.send(result);
        }
    }

    // -- Window list request/response -------------------------------------

    /// Register a pending window_list request.  Returns a receiver.
    pub async fn register_list_request(&self, request_id: String) -> oneshot::Receiver<Value> {
        let (tx, rx) = oneshot::channel();
        self.list_waiters.write().await.insert(request_id, tx);
        rx
    }

    /// Resolve a pending window_list request with the window server's response.
    pub async fn resolve_list_request(&self, request_id: &str, windows: Value) {
        if let Some(tx) = self.list_waiters.write().await.remove(request_id) {
            let _ = tx.send(windows);
        }
    }
}

// ---------------------------------------------------------------------------
// Spawn the window server process
// ---------------------------------------------------------------------------

/// Locate the `clawshake-window` binary.  Searches:
/// 1. Same directory as the running broker binary
/// 2. PATH
fn find_window_binary() -> Result<std::path::PathBuf, String> {
    if let Ok(exe) = std::env::current_exe() {
        let dir = exe.parent().unwrap_or(std::path::Path::new("."));
        let candidate = if cfg!(windows) {
            dir.join("clawshake-window.exe")
        } else {
            dir.join("clawshake-window")
        };
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    let name = if cfg!(windows) {
        "clawshake-window.exe"
    } else {
        "clawshake-window"
    };
    warn!("clawshake-window not found next to broker binary, trying PATH");
    Ok(std::path::PathBuf::from(name))
}

/// Spawn a background task that routes `channel.ui.<frame_id>.response` events
/// from the event queue back to the corresponding webview frame via `ui_push`.
///
/// This allows agents to respond to UI interactions by simply emitting on the
/// response topic — no explicit `ui_push` call required.
pub fn spawn_ui_channel_router(
    event_queue: crate::event_queue::EventQueue,
    frame_store: FrameStore,
) {
    tokio::spawn(async move {
        let mut cursor = event_queue.cursor().await;
        // Listen for all channel.ui.* events (inbound and response).
        let prefix = clawshake_channels::TOPIC_UI.to_string();
        loop {
            let (events, new_cursor) = event_queue.wait_for(&[prefix.clone()], cursor, None).await;
            cursor = new_cursor;
            for event in events {
                let Some(frame_id) = clawshake_channels::parse_ui_response_frame_id(&event.topic)
                else {
                    continue; // Not a response topic — skip inbound events
                };

                frame_store
                    .broadcast(&WsOutgoing::Push {
                        frame_id: frame_id.to_string(),
                        data: event.data,
                    })
                    .await;
            }
        }
    });
}

/// Spawn the `clawshake-window` process at broker startup.
///
/// The window server runs persistently (zero visible windows) and creates
/// native windows on demand when the broker sends render/window_open messages.
pub fn spawn_window_server(port: u16) {
    match find_window_binary() {
        Ok(bin) => {
            info!(%port, bin = %bin.display(), "Spawning window server");
            match std::process::Command::new(&bin)
                .arg("--port")
                .arg(port.to_string())
                .spawn()
            {
                Ok(child) => debug!(pid = child.id(), "Window server spawned"),
                Err(e) => warn!("Failed to spawn window server: {e}"),
            }
        }
        Err(e) => warn!("Window server binary not found: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Host page HTML
// ---------------------------------------------------------------------------

/// The static host page served at `GET /ui`.
pub const HOST_PAGE: &str = r##"<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <title>clawshake ui</title>
  <style>
    * { box-sizing: border-box; }
    html, body { height: 100%; overflow: hidden; }
    body { margin: 0; font-family: system-ui, -apple-system, sans-serif;
           background: #1a1a1a; color: #e0e0e0; }
    /* Status: fixed dot, invisible when connected */
    #status { position: fixed; bottom: 10px; right: 10px;
              width: 8px; height: 8px; border-radius: 50%;
              font-size: 0; background: #555;
              opacity: 0; transition: opacity 0.4s;
              z-index: 1000; pointer-events: none; }
    #status.connected   { opacity: 0; background: #6c6; }
    #status.disconnected { opacity: 1; background: #c66; }
    .frame-container { display: flex; flex-wrap: wrap; gap: 12px; padding: 12px; height: 100vh; }
    /* Single frame: fill viewport */
    .frame-container:has(.frame-wrapper:only-child) { padding: 0; gap: 0; }
    .frame-container:has(.frame-wrapper:only-child) .frame-wrapper {
      border: none; border-radius: 0; resize: none; width: 100vw; height: 100vh; }
    .frame-wrapper { border: 1px solid #333; border-radius: 6px;
                     display: flex; flex-direction: column;
                     resize: both; overflow: hidden; position: relative;
                     min-width: 200px; min-height: 120px; background: #1a1a1a; }
    /* Frame header: hover-only overlay */
    .frame-header { position: absolute; top: 0; left: 0; right: 0; z-index: 10;
                    padding: 5px 10px; background: rgba(20,20,20,0.82);
                    backdrop-filter: blur(6px); font-size: 13px;
                    display: flex; justify-content: space-between; align-items: center;
                    opacity: 0; transition: opacity 0.18s; pointer-events: none; }
    .frame-wrapper:hover .frame-header { opacity: 1; pointer-events: auto; }
    .frame-header .title { font-weight: 500; }
    .frame-header .close { cursor: pointer; opacity: 0.5; font-size: 16px; padding: 0 4px; }
    .frame-header .close:hover { opacity: 1; }
    iframe { border: none; display: block; background: #fff; flex: 1; width: 100%; min-height: 0; }
    .empty { padding: 40px; text-align: center; color: #555; }
  </style>
</head>
<body>
  <div id="status" class="disconnected">disconnected</div>
  <div class="frame-container" id="frames">
    <div class="empty" id="empty-msg">Waiting for agent to render UI&hellip;</div>
  </div>
  <script>
    let ws;
    const frames = {};

    // Filters:
    //   ?frame=abc  — show only that one frame
    //   ?window=xyz — show frames targeted at xyz + broadcast frames
    //   (no params) — compositor mode: broadcast frames only (targeted frames excluded)
    const _params = new URLSearchParams(location.search);
    const frameFilter = _params.get('frame'); // null = no restriction
    const windowFilter = _params.get('window'); // null = untagged/broadcast only

    function accepts(frame_id, target_window) {
      if (frameFilter && frameFilter !== frame_id) return false;
      // Targeted frames only appear in their designated window.
      // Broadcast frames (target_window == null) appear everywhere.
      if (target_window && target_window !== windowFilter) return false;
      return true;
    }

    function connect() {
      const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
      const wsUrl = proto + '//' + location.host + '/ui/ws';
      document.getElementById('status').textContent = 'connecting…';
      ws = new WebSocket(wsUrl);

      ws.onopen = () => {
        document.getElementById('status').textContent = 'connected';
        document.getElementById('status').className = 'connected';
      };
      ws.onerror = () => {
        document.getElementById('status').textContent = 'connection error';
      };
      ws.onclose = () => {
        document.getElementById('status').textContent = 'disconnected — reconnecting…';
        document.getElementById('status').className = 'disconnected';
        setTimeout(connect, 2000);
      };
      ws.onmessage = (e) => {
        let msg;
        try { msg = JSON.parse(e.data); } catch { return; }
        if (msg.type === 'render') renderFrame(msg);
        else if (msg.type === 'push') pushToFrame(msg);
        else if (msg.type === 'close') closeFrame(msg.frame_id);
        else if (msg.type === 'snapshot_request') handleSnapshot(msg);
        // Window control messages
        else if (msg.type === 'window_open') handleWindowOpen(msg);
        else if (msg.type === 'window_close') handleWindowClose(msg);
        else if (msg.type === 'window_resize') handleWindowResize(msg);
        else if (msg.type === 'window_set_title') handleWindowSetTitle(msg);
        else if (msg.type === 'window_focus') handleWindowFocus(msg);
        else if (msg.type === 'window_notify') handleWindowNotify(msg);
      };
    }
    connect();

    // -----------------------------------------------------------------------
    // Window control handlers
    // -----------------------------------------------------------------------

    const isTauri = !!(window.__TAURI__);

    async function handleWindowOpen({ label, title, url, width, height }) {
      if (isTauri) {
        try {
          const { WebviewWindow } = window.__TAURI__.webviewWindow;
          const webview = new WebviewWindow(label, {
            title: title || 'clawshake',
            url: url || ('/ui'),
            width: width || 1200,
            height: height || 800,
          });
          webview.once('tauri://error', (e) => console.error('window_open error:', e));
        } catch (e) { console.error('window_open failed:', e); }
      } else {
        // Browser fallback: open a new tab/popup.
        const target = url || location.href;
        window.open(target, label || '_blank', `width=${width||1200},height=${height||800}`);
      }
    }

    async function handleWindowClose({ label }) {
      if (isTauri) {
        try {
          const { WebviewWindow } = window.__TAURI__.webviewWindow;
          const win = WebviewWindow.getByLabel(label || 'main');
          if (win) await win.close();
        } catch (e) { console.error('window_close failed:', e); }
      }
      // Browser: no reliable way to close tabs we didn't open.
    }

    async function handleWindowResize({ label, width, height }) {
      if (isTauri) {
        try {
          const { WebviewWindow } = window.__TAURI__.webviewWindow;
          const { LogicalSize } = window.__TAURI__.dpi;
          const win = WebviewWindow.getByLabel(label || 'main');
          if (win) await win.setSize(new LogicalSize(width, height));
        } catch (e) { console.error('window_resize failed:', e); }
      }
      // Browser: cannot resize window programmatically.
    }

    async function handleWindowSetTitle({ label, title }) {
      if (isTauri) {
        try {
          const { WebviewWindow } = window.__TAURI__.webviewWindow;
          const win = WebviewWindow.getByLabel(label || 'main');
          if (win) await win.setTitle(title);
        } catch (e) { console.error('window_set_title failed:', e); }
      } else {
        // Browser fallback: change document title.
        document.title = title;
      }
    }

    async function handleWindowFocus({ label }) {
      if (isTauri) {
        try {
          const { WebviewWindow } = window.__TAURI__.webviewWindow;
          const win = WebviewWindow.getByLabel(label || 'main');
          if (win) await win.setFocus();
        } catch (e) { console.error('window_focus failed:', e); }
      } else {
        window.focus();
      }
    }

    async function handleWindowNotify({ title, body }) {
      if (isTauri) {
        try {
          const { sendNotification, isPermissionGranted, requestPermission } = window.__TAURI__.notification;
          let ok = await isPermissionGranted();
          if (!ok) ok = (await requestPermission()) === 'granted';
          if (ok) sendNotification({ title, body: body || '' });
        } catch (e) { console.error('window_notify failed:', e); }
      } else {
        // Browser Web Notifications API.
        if (Notification.permission === 'granted') {
          new Notification(title, { body: body || '' });
        } else if (Notification.permission !== 'denied') {
          Notification.requestPermission().then(p => {
            if (p === 'granted') new Notification(title, { body: body || '' });
          });
        }
      }
    }

    function hideEmpty() {
      const el = document.getElementById('empty-msg');
      if (el) el.style.display = 'none';
    }

    function renderFrame({ frame_id, src, title, width, height, window: targetWindow }) {
      if (!accepts(frame_id, targetWindow)) return;
      hideEmpty();
      let wrapper = frames[frame_id];
      if (wrapper) {
        // Update existing frame
        const iframe = wrapper.querySelector('iframe');
        iframe.src = src;
        wrapper.style.width  = (width  || 800) + 'px';
        wrapper.style.height = (height || 600) + 'px';
        const titleEl = wrapper.querySelector('.title');
        if (titleEl) titleEl.textContent = title || 'Agent UI';
        return;
      }
      // Create new frame
      wrapper = document.createElement('div');
      wrapper.className = 'frame-wrapper';
      wrapper.dataset.frameId = frame_id;
      wrapper.style.width  = (width  || 800) + 'px';
      wrapper.style.height = (height || 600) + 'px';

      const header = document.createElement('div');
      header.className = 'frame-header';
      header.innerHTML =
        '<span class="title">' + (title || 'Agent UI') + '</span>' +
        '<span class="close" title="Close">&times;</span>';
      header.querySelector('.close').onclick = () => {
        closeFrame(frame_id);
        ws.send(JSON.stringify({ type: 'close', frame_id }));
      };

      const iframe = document.createElement('iframe');
      const isExternal = src.startsWith('http') && !src.includes(location.host);
      iframe.sandbox = isExternal
        ? 'allow-scripts allow-same-origin'
        : 'allow-scripts';
      iframe.src = src;

      wrapper.appendChild(header);
      wrapper.appendChild(iframe);
      document.getElementById('frames').appendChild(wrapper);
      frames[frame_id] = wrapper;

      // Relay postMessage from iframe -> WebSocket -> EventQueue
      window.addEventListener('message', function handler(e) {
        if (e.source !== iframe.contentWindow) return;
        if (!e.data || typeof e.data.event !== 'string') return;
        const payload = JSON.stringify({
          type: 'interaction',
          frame_id: frame_id,
          event: e.data.event,
          id: e.data.id || '',
          data: e.data.data || {}
        });
        if (payload.length <= 65536 && ws.readyState === 1) {
          ws.send(payload);
        }
      });
    }

    function pushToFrame({ frame_id, data }) {
      if (!accepts(frame_id)) return;
      const wrapper = frames[frame_id];
      if (!wrapper) return;
      const iframe = wrapper.querySelector('iframe');
      if (iframe && iframe.contentWindow) {
        iframe.contentWindow.postMessage(data, '*');
      }
    }

    function closeFrame(frame_id) {
      if (!accepts(frame_id)) return;
      const wrapper = frames[frame_id];
      if (wrapper) {
        wrapper.remove();
        delete frames[frame_id];
      }
    }

    function handleSnapshot({ frame_id, format, request_id }) {
      if (!accepts(frame_id)) return; // not our frame — another host page will respond
      const wrapper = frames[frame_id];
      if (!wrapper) {
        ws.send(JSON.stringify({
          type: 'snapshot_response', request_id,
          error: 'frame not found'
        }));
        return;
      }
      const iframe = wrapper.querySelector('iframe');
      // Try direct same-origin access first (only works if allow-same-origin is set).
      try {
        let result;
        if (format === 'html') {
          result = iframe.contentDocument.documentElement.outerHTML;
        } else {
          result = iframe.contentDocument.body.innerText;
        }
        ws.send(JSON.stringify({ type: 'snapshot_response', request_id, result }));
        return;
      } catch (_) { /* sandboxed null-origin — fall through to postMessage */ }
      // Cross-origin fallback via postMessage bridge injected into inline frames.
      let settled = false;
      const listener = function(e) {
        if (e.source !== iframe.contentWindow) return;
        if (!e.data || e.data.type !== 'snapshot_response') return;
        if (e.data.request_id !== request_id) return;
        if (settled) return;
        settled = true;
        window.removeEventListener('message', listener);
        if (e.data.error) {
          ws.send(JSON.stringify({ type: 'snapshot_response', request_id, error: e.data.error }));
        } else {
          ws.send(JSON.stringify({ type: 'snapshot_response', request_id, result: e.data.result }));
        }
      };
      window.addEventListener('message', listener);
      iframe.contentWindow.postMessage({ type: 'snapshot_request', request_id, format: format || 'text' }, '*');
      setTimeout(function() {
        if (settled) return;
        settled = true;
        window.removeEventListener('message', listener);
        ws.send(JSON.stringify({ type: 'snapshot_response', request_id, error: 'snapshot timed out' }));
      }, 5000);
    }
  </script>
</body>
</html>"##;

/// Content Security Policy for broker-served inline frames.
///
/// Allows inline scripts and styles so the agent can write standard HTML+JS.
/// No external network access — the page communicates with the agent via
/// `window.parent.postMessage` (outbound) and `window.addEventListener('message')`
/// (inbound from `ui_push`).
pub const INLINE_CSP: &str =
    "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data: blob:; font-src data:;";

/// Build the full HTML document for an inline frame.
///
/// Wraps the agent's HTML/CSS/JS in a minimal document with a CSP meta tag.
/// No bridge script is injected — the agent uses standard Web APIs
/// (`postMessage` / `addEventListener`) directly.
pub fn build_inline_frame(html: &str, css: &str, js: &str) -> String {
    let css_block = if css.is_empty() {
        String::new()
    } else {
        format!("<style>{css}</style>")
    };
    let js_block = if js.is_empty() {
        String::new()
    } else {
        format!("<script>{js}</script>")
    };

    // Bridge script: lets the host page request a DOM snapshot via postMessage
    // even when the host page and the frame are cross-origin (Tauri mode).
    let bridge = r#"<script>
(function() {
  window.addEventListener('message', function(e) {
    if (e.source !== window.parent) return;
    var msg = e.data;
    if (!msg || msg.type !== 'snapshot_request') return;
    try {
      var result = msg.format === 'html'
        ? document.documentElement.outerHTML
        : (document.body ? document.body.innerText : '');
      window.parent.postMessage({ type: 'snapshot_response', request_id: msg.request_id, result: result }, '*');
    } catch(err) {
      window.parent.postMessage({ type: 'snapshot_response', request_id: msg.request_id, error: String(err) }, '*');
    }
  });
})();
</script>"#;

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="{csp}">
{css_block}
</head>
<body>
{html}
{bridge}
{js_block}
</body>
</html>"#,
        csp = INLINE_CSP,
        css_block = css_block,
        html = html,
        bridge = bridge,
        js_block = js_block,
    )
}
