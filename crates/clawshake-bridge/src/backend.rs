//! MCP backend abstraction for the bridge.
//!
//! Thin wrapper around [`clawshake_core::mcp_client`].
//! This module preserves the `McpBackend` / `HttpBackend` / `StdioBackend`
//! API used by `main.rs`, `p2p.rs`, `proxy.rs`, and `announce.rs`.

use std::sync::Arc;

use anyhow::Result;
pub use clawshake_core::mcp_client::{HttpClient, McpClient, StdioClient};

// ---------------------------------------------------------------------------
// Public API  (unchanged from callers' perspective)
// ---------------------------------------------------------------------------

/// An initialised MCP backend.  Re-export of [`McpClient`].
pub type McpBackend = McpClient;

/// Adapter: callers construct via `HttpBackend::new(port: u16)`.
pub struct HttpBackend;

impl HttpBackend {
    /// Return an HTTP backend that connects to `http://127.0.0.1:<port>`.
    pub fn new(port: u16) -> HttpClient {
        HttpClient::new(format!("http://127.0.0.1:{port}"))
    }
}

/// Adapter: callers construct via `StdioBackend::spawn(command)` (no args / client name).
pub struct StdioBackend;

impl StdioBackend {
    /// Spawn `command` as an MCP stdio server and return a ready-to-use client.
    pub async fn spawn(command: &str) -> Result<Arc<StdioClient>> {
        StdioClient::spawn(command, &[], "clawshake-bridge").await
    }
}
