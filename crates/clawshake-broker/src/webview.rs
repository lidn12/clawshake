//! Webview channel — `FrameStore`, host page, and bridge script.
//!
//! The broker serves a host page at `GET /ui` that manages sandboxed iframes.
//! Agents create/update frames via `ui_render` / `ui_push` tool calls, and
//! user interactions flow back through the [`EventQueue`](crate::event_queue)
//! on the `channel.ui` topic.
//!
//! Communication between the broker and the host page uses a WebSocket
//! at `WS /ui/ws`.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, RwLock};

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
}

/// In-memory store of active webview frames.
///
/// Cheaply cloneable — all state is behind `Arc<RwLock<_>>`.
#[derive(Clone, Default)]
pub struct FrameStore {
    frames: Arc<RwLock<HashMap<String, Frame>>>,
    /// Broadcast senders for connected host pages.
    ws_senders: Arc<RwLock<Vec<mpsc::UnboundedSender<String>>>>,
    /// Pending snapshot requests awaiting a response from the host page.
    snapshot_waiters: Arc<RwLock<HashMap<String, oneshot::Sender<Result<String, String>>>>>,
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

    /// Register a new WebSocket sender.  Returns a receiver for messages
    /// from the host page (unused here — the WS handler reads directly).
    pub async fn add_ws_sender(&self, tx: mpsc::UnboundedSender<String>) {
        self.ws_senders.write().await.push(tx);
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
    body { margin: 0; font-family: system-ui, -apple-system, sans-serif;
           background: #1a1a1a; color: #e0e0e0; }
    #status { padding: 6px 12px; font-size: 12px; color: #888;
              border-bottom: 1px solid #333; }
    #status.connected { color: #6c6; }
    #status.disconnected { color: #c66; }
    .frame-container { display: flex; flex-wrap: wrap; gap: 12px; padding: 12px; }
    .frame-wrapper { border: 1px solid #333; border-radius: 6px;
                     overflow: hidden; background: #222; }
    .frame-header { padding: 6px 10px; background: #2a2a2a; font-size: 13px;
                    display: flex; justify-content: space-between;
                    align-items: center; border-bottom: 1px solid #333; }
    .frame-header .title { font-weight: 500; }
    .frame-header .close { cursor: pointer; opacity: 0.5; font-size: 16px;
                           padding: 0 4px; }
    .frame-header .close:hover { opacity: 1; }
    iframe { border: none; display: block; background: #fff; }
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

    function connect() {
      const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
      ws = new WebSocket(proto + '//' + location.host + '/ui/ws');

      ws.onopen = () => {
        document.getElementById('status').textContent = 'connected';
        document.getElementById('status').className = 'connected';
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
      };
    }
    connect();

    function hideEmpty() {
      const el = document.getElementById('empty-msg');
      if (el) el.style.display = 'none';
    }

    function renderFrame({ frame_id, src, title, width, height }) {
      hideEmpty();
      let wrapper = frames[frame_id];
      if (wrapper) {
        // Update existing frame
        const iframe = wrapper.querySelector('iframe');
        iframe.src = src;
        iframe.width = width || 800;
        iframe.height = height || 600;
        const titleEl = wrapper.querySelector('.title');
        if (titleEl) titleEl.textContent = title || 'Agent UI';
        return;
      }
      // Create new frame
      wrapper = document.createElement('div');
      wrapper.className = 'frame-wrapper';
      wrapper.dataset.frameId = frame_id;

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
      iframe.width = width || 800;
      iframe.height = height || 600;
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
      const wrapper = frames[frame_id];
      if (!wrapper) return;
      const iframe = wrapper.querySelector('iframe');
      if (iframe && iframe.contentWindow) {
        iframe.contentWindow.postMessage(
          { __clawshake_push: true, ...data }, '*'
        );
      }
    }

    function closeFrame(frame_id) {
      const wrapper = frames[frame_id];
      if (wrapper) {
        wrapper.remove();
        delete frames[frame_id];
      }
    }

    function handleSnapshot({ frame_id, format, request_id }) {
      const wrapper = frames[frame_id];
      if (!wrapper) {
        ws.send(JSON.stringify({
          type: 'snapshot_response', request_id,
          error: 'frame not found'
        }));
        return;
      }
      const iframe = wrapper.querySelector('iframe');
      try {
        let result;
        if (format === 'html') {
          result = iframe.contentDocument.documentElement.outerHTML;
        } else {
          result = iframe.contentDocument.body.innerText;
        }
        ws.send(JSON.stringify({ type: 'snapshot_response', request_id, result }));
      } catch (e) {
        ws.send(JSON.stringify({
          type: 'snapshot_response', request_id,
          error: 'Cross-origin frame — use text/html snapshot only for broker-served content, not external src URLs.'
        }));
      }
    }
  </script>
</body>
</html>"##;

// ---------------------------------------------------------------------------
// Bridge script injected into inline frames
// ---------------------------------------------------------------------------

/// JavaScript injected at the top of every broker-served inline frame.
///
/// Provides:
/// - Automatic `data-emit` attribute handling (clicks emit events)
/// - Automatic form submission capture
/// - `ui_push` message receiving via `window.addEventListener('clawshake', ...)`
pub const BRIDGE_SCRIPT: &str = r##"<script>
(function() {
  // Forward clicks on elements with data-emit attribute
  document.addEventListener('click', function(e) {
    var el = e.target.closest('[data-emit]');
    if (!el) return;
    var attrs = {};
    for (var i = 0; i < el.attributes.length; i++) {
      var a = el.attributes[i];
      if (a.name.startsWith('data-') && a.name !== 'data-emit') {
        attrs[a.name.slice(5)] = a.value;
      }
    }
    window.parent.postMessage({
      event: el.dataset.emit || 'click',
      id: el.id || el.getAttribute('name') || '',
      data: attrs
    }, '*');
  });

  // Capture form submissions
  document.addEventListener('submit', function(e) {
    e.preventDefault();
    var fd = new FormData(e.target);
    var obj = {};
    fd.forEach(function(v, k) { obj[k] = v; });
    window.parent.postMessage({
      event: 'submit',
      id: e.target.id || e.target.getAttribute('name') || '',
      data: obj
    }, '*');
  });

  // Receive ui_push messages from broker
  window.addEventListener('message', function(e) {
    if (!e.data || !e.data.__clawshake_push) return;
    var d = e.data;
    // Shortcut: selector + html replaces innerHTML
    if (d.selector && d.html) {
      var el = document.querySelector(d.selector);
      if (el) el.innerHTML = d.html;
    }
    // Dispatch custom event for frame JS
    window.dispatchEvent(new CustomEvent('clawshake', { detail: d.payload || d }));
  });
})();
</script>"##;

/// Content Security Policy for broker-served inline frames.
pub const INLINE_CSP: &str =
    "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data: blob:; font-src data:;";

/// Build the full HTML document for an inline frame, injecting CSP meta tag,
/// bridge script, optional CSS, optional JS, and the agent's HTML body.
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

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="{csp}">
{bridge}
{css_block}
</head>
<body>
{html}
{js_block}
</body>
</html>"#,
        csp = INLINE_CSP,
        bridge = BRIDGE_SCRIPT,
        css_block = css_block,
        html = html,
        js_block = js_block,
    )
}
