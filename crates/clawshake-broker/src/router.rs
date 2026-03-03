use anyhow::Result;
use clawshake_core::manifest::InvokeConfig;
use serde_json::Value;

use crate::{
    event_queue::EventQueue,
    invoke,
    watcher::{ManifestRegistry, McpServerMap},
};

/// Everything the router needs to dispatch any tool call.
pub struct DispatchContext<'a> {
    pub registry: &'a ManifestRegistry,
    pub servers: &'a McpServerMap,
    pub event_queue: &'a EventQueue,
}

/// Dispatch a `tools/call` for `tool_name` to the correct invoke backend.
///
/// `arguments` is the JSON object from the MCP `tools/call` `arguments` field.
/// Returns the text content to include in the `CallToolResult`.
pub async fn dispatch(
    tool_name: &str,
    arguments: &Value,
    ctx: &DispatchContext<'_>,
) -> Result<String> {
    let loaded = ctx.registry.get(tool_name).ok_or_else(|| {
        anyhow::anyhow!(
            "no tool with this name is registered on the node. \
             Re-check available tools via tools/list."
        )
    })?;

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
            let server = ctx.servers.get(server_key).ok_or_else(|| {
                anyhow::anyhow!(
                    "its MCP server ('{server_key}') is not running. \
                     The node operator may need to restart the broker."
                )
            })?;
            server.tools_call(tool_name, arguments).await
        }
        InvokeConfig::InProcess => match tool_name {
            "emit" => invoke::events::invoke_emit(arguments, ctx.event_queue)
                .await
                .map_err(|e| anyhow::anyhow!(e)),
            "listen" => invoke::events::invoke_listen(arguments, ctx.event_queue)
                .await
                .map_err(|e| anyhow::anyhow!(e)),
            // run_code and describe_tools are MCP-lifecycle tools: they need
            // the broker port and shim cache which aren't in DispatchContext.
            // They are intercepted by name in mcp_server::handle() before
            // reaching the router.  Reaching here means a non-MCP caller
            // (e.g. POST /invoke) tried to call them directly.
            "run_code" | "describe_tools" => anyhow::bail!(
                "'{tool_name}' can only be called through the MCP server, \
                 not via POST /invoke"
            ),
            _ => anyhow::bail!(
                "in-process tool '{tool_name}' has no registered handler in the router"
            ),
        },
    }
}
