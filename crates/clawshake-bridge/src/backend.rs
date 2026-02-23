//! MCP backend abstraction.
//!
//! The bridge acts as an MCP *client* to a backend server, then re-exposes
//! that server's tools outward over the P2P network. Two transports are
//! supported:
//!
//! - **HTTP** — POST JSON-RPC to `http://localhost:{port}`
//! - **Stdio** — spawn the MCP server as a child process and communicate
//!   over its stdin/stdout using newline-delimited JSON-RPC.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
    sync::{oneshot, Mutex},
};
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Public enum
// ---------------------------------------------------------------------------

/// An initialised MCP backend ready to accept `tools/list` and `tools/call`
/// requests.  Cheaply cloneable (Arc-backed for the stdio variant).
#[derive(Clone)]
pub enum McpBackend {
    Http(HttpBackend),
    Stdio(Arc<StdioBackend>),
}

impl McpBackend {
    /// Query `tools/list` and return the raw tool objects from the result.
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

    /// Forward a raw MCP JSON-RPC request and return the response.
    pub async fn call(&self, request: Value) -> Result<Value> {
        match self {
            McpBackend::Http(b) => b.call(request).await,
            McpBackend::Stdio(b) => b.call(request).await,
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP backend
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct HttpBackend {
    url: String,
    client: reqwest::Client,
}

impl HttpBackend {
    pub fn new(port: u16) -> Self {
        Self {
            url: format!("http://127.0.0.1:{port}"),
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
            .context("HTTP request to MCP backend failed")?
            .json::<Value>()
            .await
            .context("Failed to parse MCP backend HTTP response")?;
        Ok(resp)
    }
}

// ---------------------------------------------------------------------------
// Stdio backend
// ---------------------------------------------------------------------------

pub struct StdioBackend {
    stdin: Mutex<ChildStdin>,
    next_id: AtomicU64,
    /// Pending JSON-RPC request IDs → response senders.
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
}

impl StdioBackend {
    /// Spawn `command` as an MCP stdio server, run the initialisation
    /// handshake, and return a ready-to-use backend.
    pub async fn spawn(command: &str) -> Result<Arc<Self>> {
        let mut parts = command.split_whitespace();
        let program = parts.next().context("--mcp-cmd must not be empty")?;
        let args: Vec<&str> = parts.collect();

        let mut child = tokio::process::Command::new(program)
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit()) // surface server logs to bridge stderr
            .spawn()
            .with_context(|| format!("Failed to spawn MCP server: {command}"))?;

        let stdin: ChildStdin = child.stdin.take().context("child has no stdin")?;
        let stdout: ChildStdout = child.stdout.take().context("child has no stdout")?;

        let backend = Arc::new(StdioBackend {
            stdin: Mutex::new(stdin),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        });

        // Background reader: picks responses off stdout and routes them to the
        // correct waiting caller by matching on the JSON-RPC `id` field.
        let backend_reader = backend.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                debug!("<- MCP stdio: {trimmed}");
                match serde_json::from_str::<Value>(trimmed) {
                    Ok(msg) => {
                        if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
                            let tx = backend_reader.pending.lock().await.remove(&id);
                            if let Some(tx) = tx {
                                let _ = tx.send(msg);
                            }
                        }
                        // Notifications (no `id`) are silently dropped for now.
                    }
                    Err(e) => warn!("Failed to parse MCP backend line: {e}: {line}"),
                }
            }
            warn!("MCP stdio backend stdout closed — process may have exited");
        });

        // Keep the child process alive until it exits naturally.
        tokio::spawn(async move {
            match child.wait().await {
                Ok(status) => warn!("MCP backend process exited: {status}"),
                Err(e) => warn!("MCP backend wait error: {e}"),
            }
        });

        // Run the MCP initialisation handshake before accepting tool calls.
        backend.initialize().await?;

        Ok(backend)
    }

    /// MCP initialise + initialized handshake.
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
                    "name": "clawshake-bridge",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        });
        self.call_with_id(id, init_req)
            .await
            .context("MCP initialize handshake failed")?;

        // `notifications/initialized` is a one-way notification with no response.
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

    /// Write a JSON value to stdin without registering a response waiter
    /// (for notifications).
    async fn send_notification(&self, msg: Value) -> Result<()> {
        let line = serde_json::to_string(&msg).context("serialising notification")?;
        debug!("-> MCP stdio: {line}");
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Write a JSON-RPC request to stdin and wait for the matching response.
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

        rx.await
            .context("MCP backend closed before responding")
    }

    /// Forward a JSON-RPC request, assigning a fresh id (overwriting any
    /// existing one so there are no conflicts with long-lived IDs from
    /// remote callers).
    pub async fn call(&self, mut request: Value) -> Result<Value> {
        let id = self.fresh_id();
        request["id"] = json!(id);
        self.call_with_id(id, request).await
    }
}
