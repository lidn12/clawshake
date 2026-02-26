use anyhow::Result;
use clawshake_core::manifest::InvokeConfig;
use serde_json::Value;

use crate::{invoke, watcher::{ManifestRegistry, McpServerMap}};

/// Dispatch a `tools/call` for `tool_name` to the correct invoke backend.
///
/// `arguments` is the JSON object from the MCP `tools/call` `arguments` field.
/// Returns the text content to include in the `CallToolResult`.
pub async fn dispatch(
    tool_name: &str,
    arguments: &Value,
    registry: &ManifestRegistry,
    servers: &McpServerMap,
) -> Result<String> {
    let loaded = registry
        .get(tool_name)
        .ok_or_else(|| anyhow::anyhow!("Unknown tool: {tool_name}"))?;

    match &loaded.tool.invoke {
        InvokeConfig::Cli {
            command,
            args,
            shell,
        } => invoke::cli::invoke(command, args, *shell, arguments).await,
        InvokeConfig::Http {
            url,
            method,
            headers,
        } => invoke::http::invoke(url, method.as_deref(), headers, arguments).await,
        InvokeConfig::Deeplink { template } => invoke::deeplink::invoke(template, arguments).await,
        InvokeConfig::AppleScript { script } => {
            invoke::script::invoke_applescript(script, arguments).await
        }
        InvokeConfig::PowerShell { script } => {
            invoke::script::invoke_powershell(script, arguments).await
        }
        InvokeConfig::Mcp { server_key } => {
            let server = servers
                .get(server_key)
                .ok_or_else(|| anyhow::anyhow!("MCP server '{server_key}' not running"))?;
            server.tools_call(tool_name, arguments).await
        }
    }
}
