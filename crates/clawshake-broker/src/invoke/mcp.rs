//! MCP invoke backend — thin wrapper around [`clawshake_core::mcp_client::McpClient`].

use anyhow::Result;
use clawshake_core::mcp_client::McpClient;
use serde_json::Value;

/// A connected MCP server that can answer `tools/list` and `tools/call`.
/// Thin wrapper around [`McpClient`] that preserves the broker's public API.
#[derive(Clone)]
pub struct McpServer(McpClient);

impl McpServer {
    /// Spawn `command args` as an MCP stdio server.
    pub async fn spawn_stdio(command: &str, args: &[String]) -> Result<Self> {
        let client = McpClient::spawn_stdio(command, args, "clawshake-broker").await?;
        Ok(McpServer(client))
    }

    /// Connect to an HTTP MCP server at `url`.
    pub fn connect_http(url: &str) -> Self {
        McpServer(McpClient::connect_http(url))
    }

    /// Query `tools/list` and return the raw tool objects.
    pub async fn tools_list(&self) -> Result<Vec<Value>> {
        self.0.tools_list().await
    }

    /// Call a tool and return the result as a JSON string.
    pub async fn tools_call(&self, tool_name: &str, arguments: &Value) -> Result<String> {
        self.0.tools_call(tool_name, arguments).await
    }

    /// Shut down the underlying server connection / process.
    pub async fn shutdown(&self) {
        self.0.shutdown().await
    }
}
