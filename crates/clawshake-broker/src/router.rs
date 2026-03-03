use std::future::Future;
use std::pin::Pin;

use anyhow::Result;
use clawshake_core::manifest::InvokeConfig;
use clawshake_core::permissions::PermissionStore;
use serde_json::Value;

use crate::{
    event_queue::EventQueue,
    invoke,
    invoke::codemode::ShimCache,
    watcher::{ManifestRegistry, McpServerMap},
};

/// Everything the router needs to dispatch any tool call.
pub struct DispatchContext<'a> {
    pub registry: &'a ManifestRegistry,
    pub servers: &'a McpServerMap,
    pub event_queue: &'a EventQueue,
    pub permissions: &'a PermissionStore,
    pub shim_cache: &'a ShimCache,
    pub port: u16,
}

// ---------------------------------------------------------------------------
// POST /invoke dispatch (shared by http_server and ephemeral server)
// ---------------------------------------------------------------------------

/// Shared request type for `POST /invoke`.
#[derive(serde::Deserialize)]
pub struct InvokeRequest {
    pub tool: String,
    #[serde(default)]
    pub arguments: Value,
}

/// Result of a `POST /invoke` dispatch.
pub struct InvokeResult {
    pub text: String,
    pub is_error: bool,
}

/// Parse, permission-check, and dispatch a `POST /invoke` request body.
///
/// This is the single code path used by both `http_server::invoke_handler`
/// and the ephemeral invoke server in `codemode.rs`.
pub async fn dispatch_invoke(body: &str, ctx: &DispatchContext<'_>) -> std::result::Result<InvokeResult, (axum::http::StatusCode, String)> {
    use clawshake_core::identity::AgentId;
    use clawshake_core::permissions::Decision;

    let req: InvokeRequest = serde_json::from_str(body)
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, format!("Bad request: {e}")))?;

    let arguments = if req.arguments.is_object() {
        req.arguments
    } else {
        Value::Object(Default::default())
    };

    // Permission check.
    let decision = ctx.permissions.check(&AgentId::Local, &req.tool).await;
    match decision {
        Decision::Allow => {}
        Decision::Deny => {
            return Ok(InvokeResult {
                text: format!("Permission denied: '{}'", req.tool),
                is_error: true,
            });
        }
        Decision::Ask => {
            if let Err(e) = ctx.permissions.set("local", &req.tool, Decision::Allow).await {
                tracing::warn!("Failed to persist permission: {e}");
            }
        }
    }

    let (text, is_error) = match dispatch(&req.tool, &arguments, ctx).await {
        Ok(t) => (t, false),
        Err(e) => (format!("{e}"), true),
    };

    Ok(InvokeResult { text, is_error })
}

/// Dispatch a `tools/call` for `tool_name` to the correct invoke backend.
///
/// `arguments` is the JSON object from the MCP `tools/call` `arguments` field.
/// Returns the text content to include in the `CallToolResult`.
///
/// Returns a boxed future to break the recursive async type that arises from
/// run_code → ephemeral_invoke_server → dispatch_invoke → dispatch.
pub fn dispatch<'a>(
    tool_name: &'a str,
    arguments: &'a Value,
    ctx: &'a DispatchContext<'a>,
) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
    Box::pin(async move {
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
            "run_code" => {
                let script = arguments
                    .get("script")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let timeout_secs = 30u64;
                invoke::codemode::invoke_run_code(
                    script,
                    ctx.port,
                    ctx.registry,
                    ctx.permissions,
                    ctx.servers,
                    ctx.shim_cache,
                    ctx.event_queue,
                    timeout_secs,
                )
                .await
            }
            "describe_tools" => {
                let query = arguments.get("query").and_then(|v| v.as_str());
                Ok(invoke::codemode::invoke_describe_tools(
                    query,
                    ctx.port,
                    ctx.registry,
                    ctx.shim_cache,
                ))
            }
            // Network tools — dispatch via IPC to the bridge daemon.
            name if name.starts_with("network_") => {
                let params = arguments.clone();
                let resp = clawshake_tools::client::send_request(name, params)
                    .await
                    .map_err(|e| anyhow::anyhow!("bridge IPC error: {e:#}"))?;
                Ok(serde_json::to_string_pretty(&resp)
                    .unwrap_or_else(|_| resp.to_string()))
            }
            _ => anyhow::bail!(
                "in-process tool '{tool_name}' has no registered handler in the router"
            ),
        },
    }
    })
}
