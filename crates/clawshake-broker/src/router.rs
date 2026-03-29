use std::future::Future;
use std::pin::Pin;

use anyhow::Result;
use clawshake_core::identity::AgentId;
use clawshake_core::manifest::InvokeConfig;
use clawshake_core::permissions::{Decision, PermissionStore};
use serde_json::Value;

use crate::{
    event_queue::EventQueue,
    expose::ExposeTable,
    invoke,
    invoke::codemode::ShimCache,
    invoke::cron::CronScheduler,
    watcher::{ManifestRegistry, McpServerMap},
    webview::FrameStore,
};

#[cfg(feature = "memory")]
use crate::invoke::memory::MemoryContext;

/// Stub type used when the `memory` feature is disabled.
#[cfg(not(feature = "memory"))]
#[derive(Clone)]
pub struct MemoryContext;

/// Everything the router needs to dispatch any tool call.
pub struct DispatchContext<'a> {
    pub registry: &'a ManifestRegistry,
    pub servers: &'a McpServerMap,
    pub event_queue: &'a EventQueue,
    pub permissions: &'a PermissionStore,
    pub shim_cache: &'a ShimCache,
    pub cron: &'a CronScheduler,
    pub port: u16,
    /// When true, `tools/list` hides individual tools and only shows
    /// `run_code` + `describe_tools`.
    pub code_mode: bool,
    /// Long-term memory context. `None` when memory is disabled.
    pub memory: Option<&'a MemoryContext>,
    /// Webview frame store for `ui_*` tools.
    pub frame_store: &'a FrameStore,
    /// Expose table for `network_expose` / `network_unexpose` / `connect_*`.
    pub expose_table: &'a ExposeTable,
    /// Trigger a DHT re-announce when the tool list changes (e.g. after
    /// `network_expose`). `None` in standalone broker mode (no bridge).
    pub reannounce_tx: Option<&'a tokio::sync::mpsc::Sender<()>>,
}

/// Owned version of [`DispatchContext`] used by server entry points
/// (`serve_stdio`, `http_server::serve`) that need to own their state.
#[derive(Clone)]
pub struct BrokerContext {
    pub registry: ManifestRegistry,
    pub permissions: PermissionStore,
    pub servers: McpServerMap,
    pub event_queue: EventQueue,
    pub shim_cache: ShimCache,
    pub cron: CronScheduler,
    pub port: u16,
    pub code_mode: bool,
    /// Long-term memory context. `None` when memory is disabled.
    pub memory: Option<MemoryContext>,
    /// Webview frame store for `ui_*` tools.
    pub frame_store: FrameStore,
    /// Expose table for `network_expose` / `network_unexpose` / `connect_*`.
    pub expose_table: ExposeTable,
    /// Trigger a DHT re-announce when the tool list changes (e.g. after
    /// `network_expose`). `None` in standalone broker mode (no bridge).
    pub reannounce_tx: Option<tokio::sync::mpsc::Sender<()>>,
}

impl BrokerContext {
    /// Borrow as a [`DispatchContext`].
    pub fn as_dispatch(&self) -> DispatchContext<'_> {
        DispatchContext {
            registry: &self.registry,
            servers: &self.servers,
            event_queue: &self.event_queue,
            permissions: &self.permissions,
            shim_cache: &self.shim_cache,
            cron: &self.cron,
            port: self.port,
            code_mode: self.code_mode,
            memory: self.memory.as_ref(),
            frame_store: &self.frame_store,
            expose_table: &self.expose_table,
            reannounce_tx: self.reannounce_tx.as_ref(),
        }
    }
}

impl<'a> DispatchContext<'a> {
    /// Clone all borrowed fields into an owned [`BrokerContext`].
    pub fn to_owned(&self) -> BrokerContext {
        BrokerContext {
            registry: self.registry.clone(),
            permissions: self.permissions.clone(),
            servers: self.servers.clone(),
            event_queue: self.event_queue.clone(),
            shim_cache: self.shim_cache.clone(),
            cron: self.cron.clone(),
            port: self.port,
            code_mode: self.code_mode,
            memory: self.memory.cloned(),
            frame_store: self.frame_store.clone(),
            expose_table: self.expose_table.clone(),
            reannounce_tx: self.reannounce_tx.cloned(),
        }
    }
}

/// Parse and dispatch a `POST /invoke` body, returning an axum response.
pub async fn dispatch_invoke(body: &str, ctx: &DispatchContext<'_>) -> axum::response::Response {
    use axum::{http::StatusCode, response::IntoResponse, Json};

    #[derive(serde::Deserialize)]
    struct Req {
        tool: String,
        #[serde(default)]
        arguments: Value,
    }

    let req: Req = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"result": format!("Bad request: {e}"), "is_error": true})),
            )
                .into_response();
        }
    };
    let args = if req.arguments.is_object() {
        req.arguments
    } else {
        Value::Object(Default::default())
    };
    let (text, is_error) = match dispatch(&req.tool, &args, ctx).await {
        Ok(t) => (t, false),
        Err(e) => (format!("{e}"), true),
    };
    Json(serde_json::json!({"result": text, "is_error": is_error})).into_response()
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
        // Permission check (single location for all callers).
        let decision = ctx.permissions.check(&AgentId::Local, tool_name).await;
        match decision {
            Decision::Allow => {}
            Decision::Deny => anyhow::bail!("Permission denied: '{tool_name}'"),
            Decision::Ask => {
                if let Err(e) = ctx
                    .permissions
                    .set("local", tool_name, Decision::Allow)
                    .await
                {
                    tracing::warn!("Failed to persist permission: {e}");
                }
            }
        }

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
            InvokeConfig::Deeplink { template } => {
                invoke::deeplink::invoke(template, arguments).await
            }
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
                "emit" => invoke::events::invoke_emit(arguments, ctx.event_queue).await,
                "listen" => invoke::events::invoke_listen(arguments, ctx.event_queue).await,
                "run_code" => {
                    let script = arguments
                        .get("script")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    invoke::codemode::invoke_run_code(script, ctx, 30).await
                }
                "describe_tools" => {
                    let query = arguments.get("query").and_then(|v| v.as_str());
                    Ok(invoke::codemode::invoke_describe_tools(query, ctx))
                }
                // Expose tools — handled in-process (broker-side).
                "network_expose" => {
                    invoke::expose::handle_expose(
                        arguments,
                        ctx.expose_table,
                        ctx.registry,
                        ctx.reannounce_tx,
                    )
                    .await
                }
                "network_unexpose" => {
                    invoke::expose::handle_unexpose(
                        arguments,
                        ctx.expose_table,
                        ctx.registry,
                        ctx.reannounce_tx,
                    )
                    .await
                }
                // Dynamic connect_* tools — handled in-process.
                name if name.starts_with("connect_") => {
                    invoke::expose::handle_connect(name, arguments, ctx.expose_table)
                }
                // Network tools — dispatch via IPC to the bridge daemon.
                name if name.starts_with("network_") => {
                    let params = arguments.clone();
                    let resp = clawshake_core::ipc::send_request(name, params)
                        .await
                        .map_err(|e| anyhow::anyhow!("bridge IPC error: {e:#}"))?;
                    Ok(serde_json::to_string_pretty(&resp).unwrap_or_else(|_| resp.to_string()))
                }
                // General-purpose tools — dispatch in-process.
                "shell" => invoke::shell::handle(arguments).await,
                // Spawn — async wrapper for any tool call.
                "spawn" => invoke::spawn::handle(arguments, ctx).await,
                // Cron tools — dispatch to the scheduler.
                "cron_add" => ctx.cron.handle_add(arguments, ctx.event_queue).await,
                "cron_list" => ctx.cron.handle_list(arguments).await,
                "cron_remove" => ctx.cron.handle_remove(arguments).await,
                // Memory tools — dispatch to the memory subsystem.
                #[cfg(feature = "memory")]
                name if name.starts_with("memory_") => {
                    let mem = ctx.memory.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("memory subsystem is disabled (no [memory] config)")
                    })?;
                    match name {
                        "memory_recall" => invoke::memory::invoke_recall(arguments, mem).await,
                        "memory_procedural" => {
                            invoke::memory::invoke_procedural(arguments, mem).await
                        }
                        "memory_append" => invoke::memory::invoke_append(arguments, mem).await,
                        "memory_ingest" => invoke::memory::invoke_ingest(arguments, mem).await,
                        "memory_embed" => invoke::memory::invoke_embed(arguments, mem).await,
                        _ => anyhow::bail!("unknown memory tool '{name}'"),
                    }
                }
                #[cfg(not(feature = "memory"))]
                name if name.starts_with("memory_") => {
                    anyhow::bail!("memory subsystem not compiled in (build with --features memory)")
                }
                // Webview UI tools — dispatch to the frame store.
                "ui_render" => {
                    invoke::webview::handle_render(arguments, ctx.frame_store, ctx.port).await
                }
                "ui_push" => invoke::webview::handle_push(arguments, ctx.frame_store).await,
                "ui_snapshot" => invoke::webview::handle_snapshot(arguments, ctx.frame_store).await,
                "ui_list" => invoke::webview::handle_list(arguments, ctx.frame_store).await,
                "ui_close" => invoke::webview::handle_close(arguments, ctx.frame_store).await,
                // Window control tools.
                "window_open" => invoke::window::handle_open(arguments, ctx.frame_store).await,
                "window_close" => invoke::window::handle_close(arguments, ctx.frame_store).await,
                "window_resize" => invoke::window::handle_resize(arguments, ctx.frame_store).await,
                "window_set_title" => {
                    invoke::window::handle_set_title(arguments, ctx.frame_store).await
                }
                "window_focus" => invoke::window::handle_focus(arguments, ctx.frame_store).await,
                "window_notify" => invoke::window::handle_notify(arguments, ctx.frame_store).await,
                "window_list" => invoke::window::handle_list(arguments, ctx.frame_store).await,
                _ => anyhow::bail!(
                    "in-process tool '{tool_name}' has no registered handler in the router"
                ),
            },
        }
    })
}
