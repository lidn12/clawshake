//! Invoke handlers for webview UI tools.
//!
//! - `ui_render` — create or update a sandboxed webview frame
//! - `ui_push`   — push partial data to an open frame
//! - `ui_snapshot` — capture DOM text/html from a frame
//! - `ui_close`  — tear down a frame

use anyhow::{bail, Result};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::webview::{Frame, FrameContent, FrameStore, WsOutgoing};

/// Handle a `ui_render` tool call.
///
/// Creates or replaces a webview frame and notifies connected host pages.
pub async fn handle_render(
    arguments: &Value,
    frame_store: &FrameStore,
    port: u16,
) -> Result<String> {
    let html = arguments.get("html").and_then(|v| v.as_str());
    let src = arguments.get("src").and_then(|v| v.as_str());

    if html.is_some() && src.is_some() {
        bail!("'html' and 'src' are mutually exclusive — provide one or the other");
    }
    if html.is_none() && src.is_none() {
        bail!("either 'html' or 'src' is required");
    }

    let css = arguments
        .get("css")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let js = arguments
        .get("js")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let title = arguments
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("Agent UI")
        .to_string();
    let width = arguments
        .get("width")
        .and_then(|v| v.as_u64())
        .unwrap_or(800) as u32;
    let height = arguments
        .get("height")
        .and_then(|v| v.as_u64())
        .unwrap_or(600) as u32;

    let frame_id = arguments
        .get("frame_id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let (content, serve_src) = if let Some(html_str) = html {
        // Inline mode — broker serves the content at /ui/frame/<id>
        let content = FrameContent::Inline {
            html: html_str.to_string(),
            css,
            js,
        };
        let src_url = format!("http://127.0.0.1:{port}/ui/frame/{frame_id}");
        (content, src_url)
    } else {
        // Src mode — iframe navigates directly to the URL
        let src_url = src.unwrap().to_string();
        let content = FrameContent::Src(src_url.clone());
        (content, src_url)
    };

    let frame = Frame {
        content,
        title: title.clone(),
        width,
        height,
    };

    frame_store.insert(frame_id.clone(), frame).await;

    // Notify host page(s) to render the frame.
    frame_store
        .broadcast(&WsOutgoing::Render {
            frame_id: frame_id.clone(),
            src: serve_src.clone(),
            title,
            width,
            height,
        })
        .await;

    let ui_url = format!("http://127.0.0.1:{port}/ui");
    let result = json!({
        "frame_id": frame_id,
        "frame_url": serve_src,
        "ui_url": ui_url,
    });
    Ok(serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()))
}

/// Handle a `ui_push` tool call.
///
/// Push a partial update to an open frame via postMessage.
pub async fn handle_push(arguments: &Value, frame_store: &FrameStore) -> Result<String> {
    let frame_id = arguments
        .get("frame_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required field: frame_id"))?;

    // Verify frame exists.
    if frame_store.get(frame_id).await.is_none() {
        bail!("no open frame with id '{frame_id}' — call ui_render first");
    }

    let data = arguments
        .get("data")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));

    frame_store
        .broadcast(&WsOutgoing::Push {
            frame_id: frame_id.to_string(),
            data,
        })
        .await;

    Ok(json!({"ok": true}).to_string())
}

/// Handle a `ui_snapshot` tool call.
///
/// Requests the host page to capture the current frame DOM and waits for the
/// response.  Only works for broker-served frames (same-origin).
pub async fn handle_snapshot(arguments: &Value, frame_store: &FrameStore) -> Result<String> {
    let frame_id = arguments
        .get("frame_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required field: frame_id"))?;

    if frame_store.get(frame_id).await.is_none() {
        bail!("no open frame with id '{frame_id}'");
    }

    let format = arguments
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("text")
        .to_string();

    if format == "screenshot" {
        bail!(
            "screenshot format requires a headless capture sidecar (not yet implemented). \
             Use 'text' or 'html' format for broker-served frames."
        );
    }

    let request_id = Uuid::new_v4().to_string();

    // Register waiter before sending the request.
    let rx = frame_store.register_snapshot(request_id.clone()).await;

    frame_store
        .broadcast(&WsOutgoing::SnapshotRequest {
            frame_id: frame_id.to_string(),
            format,
            request_id: request_id.clone(),
        })
        .await;

    // Wait up to 10 seconds for the host page to respond.
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), rx).await;

    match result {
        Ok(Ok(Ok(text))) => {
            let resp = json!({ "snapshot": text });
            Ok(serde_json::to_string_pretty(&resp).unwrap_or_else(|_| resp.to_string()))
        }
        Ok(Ok(Err(err))) => bail!("snapshot failed: {err}"),
        Ok(Err(_)) => bail!("snapshot response channel closed — host page disconnected"),
        Err(_) => bail!("snapshot timed out after 10s — is the /ui page open in a browser?"),
    }
}

/// Handle a `ui_close` tool call.
///
/// Removes the frame from the store and notifies the host page.
pub async fn handle_close(arguments: &Value, frame_store: &FrameStore) -> Result<String> {
    let frame_id = arguments
        .get("frame_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required field: frame_id"))?;

    frame_store.remove(frame_id).await;

    frame_store
        .broadcast(&WsOutgoing::Close {
            frame_id: frame_id.to_string(),
        })
        .await;

    Ok(json!({"ok": true}).to_string())
}

// ---------------------------------------------------------------------------
// Tool definitions for builtins.rs
// ---------------------------------------------------------------------------

/// Return the JSON tool definitions for the webview tools.
pub fn webview_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "ui_render",
            "description": "Render HTML in the agent window. Returns a frame_id for updates/snapshots. \
                Use 'html' for agent-generated content or 'src' for a local dev server URL. \
                \n\nSending events to the agent from your HTML: \
                window.parent.postMessage({event: 'myevent', data: {key: 'value'}}, '*') \
                — the agent receives these via listen(topics=['channel.ui']). \
                \n\nReceiving data from the agent (via ui_push): \
                window.addEventListener('message', (e) => { /* e.data is the pushed JSON */ })",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "html": {
                        "type": "string",
                        "description": "HTML content to render. Write standard HTML/CSS/JS. \
                            To send events back to the agent, use: \
                            window.parent.postMessage({event: 'name', data: {...}}, '*'). \
                            Mutually exclusive with 'src'."
                    },
                    "css": {
                        "type": "string",
                        "description": "CSS injected as a <style> block. Only used with 'html'."
                    },
                    "js": {
                        "type": "string",
                        "description": "JavaScript injected as a <script> block. Only used with 'html'. \
                            Receive ui_push data via: window.addEventListener('message', (e) => { ... })."
                    },
                    "src": {
                        "type": "string",
                        "description": "URL to render directly (e.g. 'http://localhost:5173'). Mutually exclusive with 'html'."
                    },
                    "title": {
                        "type": "string",
                        "description": "Display title for the frame. Default: 'Agent UI'."
                    },
                    "frame_id": {
                        "type": "string",
                        "description": "Reuse an existing frame. If omitted, a new frame_id is generated."
                    },
                    "width": {
                        "type": "number",
                        "description": "Frame width in pixels. Default: 800."
                    },
                    "height": {
                        "type": "number",
                        "description": "Frame height in pixels. Default: 600."
                    }
                }
            }
        }),
        json!({
            "name": "ui_push",
            "description": "Push a JSON update to an open webview frame without re-rendering. \
                The data is delivered as a standard 'message' event on the iframe's window. \
                The frame receives it via: window.addEventListener('message', (e) => { /* e.data */ }).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "frame_id": {
                        "type": "string",
                        "description": "Target frame ID from a prior ui_render call."
                    },
                    "data": {
                        "description": "Arbitrary JSON payload delivered to the frame via postMessage."
                    }
                },
                "required": ["frame_id", "data"]
            }
        }),
        json!({
            "name": "ui_snapshot",
            "description": "Capture the current content of an open webview frame. Returns DOM \
                text or HTML for verification. Only works for broker-served frames (html mode), \
                not external src URLs due to cross-origin restrictions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "frame_id": {
                        "type": "string",
                        "description": "Target frame ID."
                    },
                    "format": {
                        "type": "string",
                        "enum": ["text", "html"],
                        "description": "Output format. 'text' returns innerText, 'html' returns outerHTML. Default: 'text'."
                    }
                },
                "required": ["frame_id"]
            }
        }),
        json!({
            "name": "ui_close",
            "description": "Close an open webview frame and remove it from the display.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "frame_id": {
                        "type": "string",
                        "description": "Frame ID to close."
                    }
                },
                "required": ["frame_id"]
            }
        }),
    ]
}
