//! Unified MCP client abstraction.
//!
//! Both the bridge (speaking to backend servers) and the broker (forwarding to
//! manifested MCP servers) need the same JSON-RPC-over-stdio / HTTP client
//! logic.  This module provides a single implementation that both crates use.
//!
//! # Usage
//!
//! ```rust,ignore
//! // Stdio
//! let client = McpClient::spawn_stdio("npx", &["-y".into(), "@modelcontextprotocol/server-filesystem".into()], "my-app").await?;
//! // HTTP
//! let client = McpClient::connect_http("http://127.0.0.1:3000");
//!
//! let tools  = client.tools_list().await?;
//! let result = client.tools_call("read_file", &json!({"path": "/tmp/foo"})).await?;
//! ```

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
    sync::{oneshot, Mutex},
};
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Public top-level enum
// ---------------------------------------------------------------------------

/// A connected MCP client.  Cheaply cloneable — the stdio variant is
/// Arc-backed so all clones share the same subprocess.
#[derive(Clone)]
pub enum McpClient {
    Stdio(Arc<StdioClient>),
    Http(HttpClient),
}

impl McpClient {
    /// Spawn `command args` as an MCP stdio server, run the initialisation
    /// handshake (sending `client_name` as the MCP client info), and return a
    /// ready-to-use client.
    pub async fn spawn_stdio(command: &str, args: &[String], client_name: &str) -> Result<Self> {
        let client = StdioClient::spawn(command, args, client_name).await?;
        Ok(McpClient::Stdio(client))
    }

    /// Connect to an HTTP-based MCP server at `url`.
    pub fn connect_http(url: impl Into<String>) -> Self {
        McpClient::Http(HttpClient::new(url))
    }

    /// Query `tools/list` and return the raw tool JSON objects from the result.
    pub async fn tools_list(&self) -> Result<Vec<Value>> {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {}
        });
        let resp = self.call(req).await?;
        let tools = resp
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(tools)
    }

    /// Send a `tools/call` request and return the concatenated text content.
    ///
    /// Returns an error if the JSON-RPC layer failed or if the MCP result
    /// carries `isError: true`.
    pub async fn tools_call(&self, tool_name: &str, arguments: &Value) -> Result<String> {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": arguments,
            }
        });
        let resp = self.call(req).await?;

        if let Some(err) = resp.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown MCP error");
            anyhow::bail!("MCP error: {msg}");
        }

        let result = resp.get("result");

        let is_error = result
            .and_then(|r| r.get("isError"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let content = result
            .and_then(|r| r.get("content"))
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();

        let text: String = content
            .iter()
            .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n");

        if is_error {
            anyhow::bail!("{text}");
        }
        Ok(text)
    }

    /// Forward a raw JSON-RPC request and return the response.
    pub async fn call(&self, request: Value) -> Result<Value> {
        match self {
            McpClient::Stdio(c) => c.call(request).await,
            McpClient::Http(c) => c.call(request).await,
        }
    }

    /// Shut down the underlying connection.
    ///
    /// For stdio clients this kills the child process; for HTTP clients this
    /// is a no-op.
    pub async fn shutdown(&self) {
        match self {
            McpClient::Stdio(c) => c.shutdown().await,
            McpClient::Http(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP client
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct HttpClient {
    url: String,
    client: reqwest::Client,
}

impl HttpClient {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            client: reqwest::Client::new(),
        }
    }

    pub async fn call(&self, request: Value) -> Result<Value> {
        let resp = self
            .client
            .post(&self.url)
            .json(&request)
            .send()
            .await
            .context("HTTP request to MCP server failed")?
            .json::<Value>()
            .await
            .context("Failed to parse MCP server HTTP response")?;
        Ok(resp)
    }
}

// ---------------------------------------------------------------------------
// Stdio client
// ---------------------------------------------------------------------------

pub struct StdioClient {
    stdin: Mutex<ChildStdin>,
    child: Mutex<Option<Child>>,
    next_id: AtomicU64,
    /// In-flight JSON-RPC requests: id → response oneshot sender.
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
}

impl StdioClient {
    /// Spawn `command args` as an MCP stdio server, run the MCP initialisation
    /// handshake (identifying this client as `client_name`), and return a
    /// ready-to-use handle.
    pub async fn spawn(command: &str, args: &[String], client_name: &str) -> Result<Arc<Self>> {
        // On Windows, .cmd/.bat wrappers (npx.cmd, node.cmd, etc.) cannot be
        // executed directly  by CreateProcess — route through `cmd /C`.
        // On Unix, use `sh -c` for PATH / alias resolution.
        #[cfg(windows)]
        let mut child = {
            let mut cmd = tokio::process::Command::new("cmd");
            cmd.arg("/C").arg(command);
            for a in args {
                cmd.arg(a);
            }
            cmd.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::inherit())
                .spawn()
                .with_context(|| format!("Failed to spawn MCP server: {command}"))?
        };

        #[cfg(not(windows))]
        let mut child = {
            let full = if args.is_empty() {
                command.to_string()
            } else {
                format!("{} {}", command, args.join(" "))
            };
            tokio::process::Command::new("sh")
                .args(["-c", &full])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::inherit())
                .spawn()
                .with_context(|| format!("Failed to spawn MCP server: {command}"))?
        };

        let stdin: ChildStdin = child.stdin.take().context("child has no stdin")?;
        let stdout: ChildStdout = child.stdout.take().context("child has no stdout")?;

        let client = Arc::new(StdioClient {
            stdin: Mutex::new(stdin),
            child: Mutex::new(Some(child)),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        });

        // Background reader: routes JSON-RPC responses to waiting callers by
        // matching `id`.  Exits naturally when stdout closes.
        let reader = client.clone();
        tokio::spawn(async move {
            let buf = BufReader::new(stdout);
            let mut lines = buf.lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        debug!("<- MCP stdio: {trimmed}");
                        match serde_json::from_str::<Value>(trimmed) {
                            Ok(msg) => {
                                if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
                                    let tx = reader.pending.lock().await.remove(&id);
                                    if let Some(tx) = tx {
                                        let _ = tx.send(msg);
                                    }
                                }
                                // Notifications (no `id`) are silently dropped.
                            }
                            Err(e) => warn!("Failed to parse MCP server line: {e}: {line}"),
                        }
                    }
                    Ok(None) | Err(_) => {
                        debug!("MCP server stdout closed");
                        break;
                    }
                }
            }
        });

        // MCP initialisation handshake.
        client.initialize(client_name).await?;

        Ok(client)
    }

    async fn initialize(&self, client_name: &str) -> Result<()> {
        let id = self.fresh_id();
        let init_req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": client_name,
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        });
        self.call_with_id(id, init_req)
            .await
            .context("MCP initialize handshake failed")?;

        // `notifications/initialized` is one-way (no response expected).
        self.send_notification(json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        }))
        .await?;

        Ok(())
    }

    fn fresh_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    async fn send_notification(&self, msg: Value) -> Result<()> {
        let line = serde_json::to_string(&msg).context("serialising notification")?;
        debug!("-> MCP stdio: {line}");
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn call_with_id(&self, id: u64, request: Value) -> Result<Value> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let line = serde_json::to_string(&request).context("serialising request")?;
        debug!("-> MCP stdio: {line}");
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await?;
        }

        rx.await.context("MCP server closed before responding")
    }

    /// Forward a JSON-RPC request, assigning a fresh `id` (overwriting any
    /// existing one to avoid conflicts with IDs from remote callers).
    pub async fn call(&self, mut request: Value) -> Result<Value> {
        let id = self.fresh_id();
        request["id"] = json!(id);
        self.call_with_id(id, request).await
    }

    /// Drain pending callers and kill the child process.
    pub async fn shutdown(&self) {
        // Let all waiting callers unblock immediately.
        self.pending.lock().await.clear();

        if let Some(mut child) = self.child.lock().await.take() {
            match tokio::time::timeout(std::time::Duration::from_millis(100), child.wait()).await {
                Ok(Ok(status)) => info!("MCP server exited gracefully: {status}"),
                _ => {
                    if let Err(e) = child.kill().await {
                        warn!("Failed to kill MCP server child: {e}");
                    } else {
                        info!("MCP server child process killed");
                    }
                }
            }
        }
    }
}
