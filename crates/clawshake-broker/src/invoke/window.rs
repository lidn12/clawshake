//! Invoke handlers for native window control tools.
//!
//! These tools control the host window (Tauri or browser) via WebSocket
//! messages to the connected host page.  When running inside `clawshake-window`
//! (Tauri), the host page JS calls `window.__TAURI__` APIs.  In a browser,
//! commands gracefully degrade (e.g. resize has no effect, notifications use
//! the Web Notifications API).
//!
//! - `window_open`      — open a new native window (Tauri) or browser tab
//! - `window_close`     — close a window
//! - `window_resize`    — resize a window
//! - `window_set_title` — change window title
//! - `window_focus`     — bring a window to front
//! - `window_notify`    — show a system notification
//! - `window_list`      — list open windows

use anyhow::Result;
use serde_json::{json, Value};

use crate::webview::{FrameStore, WsOutgoing};

/// Handle `window_open` — open a new native window or browser tab.
pub async fn handle_open(arguments: &Value, frame_store: &FrameStore) -> Result<String> {
    let label = arguments
        .get("label")
        .and_then(|v| v.as_str())
        .unwrap_or("new")
        .to_string();
    let title = arguments
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("clawshake")
        .to_string();
    let url = arguments
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // If no URL is provided, use /ui?window=<label> so the host page
    // only renders frames targeted at this window (plus untagged ones).
    let url = if url.is_empty() {
        format!("/ui?window={label}")
    } else {
        url
    };
    let width = arguments
        .get("width")
        .and_then(|v| v.as_u64())
        .unwrap_or(1200) as u32;
    let height = arguments
        .get("height")
        .and_then(|v| v.as_u64())
        .unwrap_or(800) as u32;

    frame_store
        .broadcast(&WsOutgoing::WindowOpen {
            label: label.clone(),
            title,
            url,
            width,
            height,
        })
        .await;

    Ok(json!({"ok": true, "label": label}).to_string())
}

/// Handle `window_close` — close a window by label.
pub async fn handle_close(arguments: &Value, frame_store: &FrameStore) -> Result<String> {
    let label = arguments
        .get("label")
        .and_then(|v| v.as_str())
        .unwrap_or("main")
        .to_string();

    frame_store
        .broadcast(&WsOutgoing::WindowClose {
            label: label.clone(),
        })
        .await;

    Ok(json!({"ok": true}).to_string())
}

/// Handle `window_resize` — resize a window by label.
pub async fn handle_resize(arguments: &Value, frame_store: &FrameStore) -> Result<String> {
    let label = arguments
        .get("label")
        .and_then(|v| v.as_str())
        .unwrap_or("main")
        .to_string();
    let width = arguments
        .get("width")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow::anyhow!("missing required field: width"))? as u32;
    let height = arguments
        .get("height")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow::anyhow!("missing required field: height"))? as u32;

    frame_store
        .broadcast(&WsOutgoing::WindowResize {
            label,
            width,
            height,
        })
        .await;

    Ok(json!({"ok": true}).to_string())
}

/// Handle `window_set_title` — change a window's title bar text.
pub async fn handle_set_title(arguments: &Value, frame_store: &FrameStore) -> Result<String> {
    let label = arguments
        .get("label")
        .and_then(|v| v.as_str())
        .unwrap_or("main")
        .to_string();
    let title = arguments
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required field: title"))?
        .to_string();

    frame_store
        .broadcast(&WsOutgoing::WindowSetTitle { label, title })
        .await;

    Ok(json!({"ok": true}).to_string())
}

/// Handle `window_focus` — bring a window to front.
pub async fn handle_focus(arguments: &Value, frame_store: &FrameStore) -> Result<String> {
    let label = arguments
        .get("label")
        .and_then(|v| v.as_str())
        .unwrap_or("main")
        .to_string();

    frame_store
        .broadcast(&WsOutgoing::WindowFocus { label })
        .await;

    Ok(json!({"ok": true}).to_string())
}

/// Handle `window_notify` — show a system notification.
pub async fn handle_notify(arguments: &Value, frame_store: &FrameStore) -> Result<String> {
    let title = arguments
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required field: title"))?
        .to_string();
    let body = arguments
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    frame_store
        .broadcast(&WsOutgoing::WindowNotify { title, body })
        .await;

    Ok(json!({"ok": true}).to_string())
}

/// Handle `window_list` — return the list of open windows.
///
/// Sends a `WindowListRequest` to the window server over WS and waits for
/// its `WindowListResponse`.  Falls back to an empty list on timeout.
pub async fn handle_list(_arguments: &Value, frame_store: &FrameStore) -> Result<String> {
    if !frame_store.has_ws_client().await {
        return Ok(json!({"windows": []}).to_string());
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let rx = frame_store.register_list_request(request_id.clone()).await;

    frame_store
        .broadcast(&WsOutgoing::WindowListRequest {
            request_id: request_id.clone(),
        })
        .await;

    match tokio::time::timeout(std::time::Duration::from_secs(3), rx).await {
        Ok(Ok(windows)) => Ok(json!({"windows": windows}).to_string()),
        _ => Ok(json!({"windows": [], "note": "timeout"}).to_string()),
    }
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

/// Return the JSON tool definitions for the window tools.
pub fn window_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "window_open",
            "description": "Open a new native window (when running in clawshake-window) or \
                browser tab (in browser mode). Use this to create additional windows for \
                dashboards, monitors, or setup wizards.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "label": {
                        "type": "string",
                        "description": "Unique label for the window. Used to reference it in other window_* calls. Default: 'new'."
                    },
                    "title": {
                        "type": "string",
                        "description": "Window title bar text. Default: 'clawshake'."
                    },
                    "url": {
                        "type": "string",
                        "description": "URL to load in the window. Defaults to /ui?window=<label>, which \
                            shows only frames targeted at this window (plus untagged frames)."
                    },
                    "width": {
                        "type": "number",
                        "description": "Window width in pixels. Default: 1200."
                    },
                    "height": {
                        "type": "number",
                        "description": "Window height in pixels. Default: 800."
                    }
                }
            }
        }),
        json!({
            "name": "window_close",
            "description": "Close a native window by its label. In browser mode, this is a no-op. \
                Closing the 'main' window will shut down the application in native mode.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "label": {
                        "type": "string",
                        "description": "Window label to close. Default: 'main'."
                    }
                }
            }
        }),
        json!({
            "name": "window_resize",
            "description": "Resize a native window. In browser mode, this is a no-op (browser windows \
                cannot be resized programmatically).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "label": {
                        "type": "string",
                        "description": "Window label to resize. Default: 'main'."
                    },
                    "width": {
                        "type": "number",
                        "description": "New width in pixels."
                    },
                    "height": {
                        "type": "number",
                        "description": "New height in pixels."
                    }
                },
                "required": ["width", "height"]
            }
        }),
        json!({
            "name": "window_set_title",
            "description": "Change the title bar text of a native window. In browser mode, \
                changes document.title instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "label": {
                        "type": "string",
                        "description": "Window label. Default: 'main'."
                    },
                    "title": {
                        "type": "string",
                        "description": "New title text."
                    }
                },
                "required": ["title"]
            }
        }),
        json!({
            "name": "window_focus",
            "description": "Bring a native window to the front. In browser mode, attempts \
                window.focus().",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "label": {
                        "type": "string",
                        "description": "Window label to focus. Default: 'main'."
                    }
                }
            }
        }),
        json!({
            "name": "window_notify",
            "description": "Show a system notification. Works in both native (Tauri) and \
                browser mode (Web Notifications API, requires user permission). Use for \
                alerting the user about background events.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Notification title."
                    },
                    "body": {
                        "type": "string",
                        "description": "Notification body text."
                    }
                },
                "required": ["title"]
            }
        }),
        json!({
            "name": "window_list",
            "description": "List currently open windows. In browser mode, only the main tab is known. \
                In native mode (clawshake-window), returns all open Tauri windows.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
    ]
}
