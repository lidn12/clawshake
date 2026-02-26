//! MCP invoke backend — forward `tools/call` to external MCP servers.
//!
//! The broker can connect to existing MCP servers (stdio or HTTP) and expose
//! their tools as part of its own tool set.  This module manages the running
//! server processes and proxies JSON-RPC calls to them.

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
// Public types
// ---------------------------------------------------------------------------

/// A running MCP server that can answer `tools/list` and `tools/call`.
#[derive(Clone)]
pub enum McpServer {
    Stdio(Arc<StdioServer>),
    Http(HttpServer),
}

impl McpServer {
    /// Connect to a stdio-based MCP server by spawning the given command.
    pub async fn spawn_stdio(command: &str, args: &[String]) -> Result<Self> {
        let server = StdioServer::spawn(command, args).await?;
        Ok(McpServer::Stdio(server))
    }

    /// Connect to an HTTP-based MCP server.
    pub fn connect_http(url: &str) -> Self {
        McpServer::Http(HttpServer::new(url))
    }

    /// Query `tools/list` and return raw tool JSON objects.
    pub async fn tools_list(&self) -> Result<Vec<Value>> {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {}
        });
        let resp = self.call_raw(req).await?;
        let tools = resp
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(tools)
    }

    /// Forward a `tools/call` request and return the text content of the result.
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
        let resp = self.call_raw(req).await?;

        // Extract text from the MCP result content array.
        if let Some(err) = resp.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown MCP error");
            anyhow::bail!("MCP server error: {msg}");
        }

        let content = resp
            .get("result")
            .and_then(|r| r.get("content"))
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();

        // Check isError flag.
        let is_error = resp
            .get("result")
            .and_then(|r| r.get("isError"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

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

    /// Send a raw JSON-RPC request and return the response.
    async fn call_raw(&self, request: Value) -> Result<Value> {
        match self {
            McpServer::Stdio(s) => s.call(request).await,
            McpServer::Http(h) => h.call(request).await,
        }
    }

    /// Shut down the server: close stdin, cancel background tasks, kill child.
    pub async fn shutdown(&self) {
        match self {
            McpServer::Stdio(s) => s.shutdown().await,
            McpServer::Http(_) => {} // Nothing to shut down.
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP server connection
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct HttpServer {
    url: String,
    client: reqwest::Client,
}

impl HttpServer {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            client: reqwest::Client::new(),
        }
    }

    async fn call(&self, request: Value) -> Result<Value> {
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
// Stdio server connection
// ---------------------------------------------------------------------------

pub struct StdioServer {
    stdin: Mutex<ChildStdin>,
    child: Mutex<Option<Child>>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
}

impl StdioServer {
    /// Spawn `command` with `args` as an MCP stdio server, run the MCP
    /// initialisation handshake, and return a ready-to-use handle.
    pub async fn spawn(command: &str, args: &[String]) -> Result<Arc<Self>> {
        // On Windows, .cmd/.bat wrappers (npx.cmd, node.cmd, etc.) require
        // a shell.  On Unix, use sh -c for PATH and alias resolution.
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

        let server = Arc::new(StdioServer {
            stdin: Mutex::new(stdin),
            child: Mutex::new(Some(child)),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        });

        // Background reader: routes JSON-RPC responses to waiting callers.
        // The task exits naturally when stdout closes (child killed or exited).
        let reader_handle = server.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        debug!("<- MCP server: {trimmed}");
                        match serde_json::from_str::<Value>(trimmed) {
                            Ok(msg) => {
                                if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
                                    let tx = reader_handle.pending.lock().await.remove(&id);
                                    if let Some(tx) = tx {
                                        let _ = tx.send(msg);
                                    }
                                }
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
        server.initialize().await?;

        Ok(server)
    }

    async fn initialize(&self) -> Result<()> {
        let id = self.fresh_id();
        let init_req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "clawshake-broker",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        });
        self.call_with_id(id, init_req)
            .await
            .context("MCP initialize handshake failed")?;

        // Send `notifications/initialized` (one-way, no response).
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
        debug!("-> MCP server: {line}");
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
        debug!("-> MCP server: {line}");
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await?;
        }

        rx.await.context("MCP server closed before responding")
    }

    pub async fn call(&self, mut request: Value) -> Result<Value> {
        let id = self.fresh_id();
        request["id"] = json!(id);
        self.call_with_id(id, request).await
    }

    /// Cancel background tasks, close stdin, and kill the child process.
    pub async fn shutdown(&self) {
        // Drop all pending callers so they don't hang.
        {
            let mut pending = self.pending.lock().await;
            pending.clear();
        }

        // Kill the child process.  This closes stdout, which makes the
        // background reader task exit and drop its Arc<StdioServer>.
        if let Some(mut child) = self.child.lock().await.take() {
            // Try graceful wait (100ms), then force kill.
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
