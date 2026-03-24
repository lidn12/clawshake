//! Generic `spawn` tool — async wrapper for any tool call.
//!
//! `spawn` accepts a tool name and its arguments, kicks off the call in a
//! background task, and returns a `task_id` immediately.  When the inner tool
//! completes, the result is emitted as an event:
//!
//! - **Success** → topic `task.done`, data `{ task_id, tool, result }`
//! - **Failure** → topic `task.failed`, data `{ task_id, tool, error }`
//!
//! This lets agents run any tool asynchronously, collect results via `listen`,
//! and compose patterns like parallel execution, background jobs, and sub-agent
//! delegation without purpose-built tools for each variant.

use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{debug, info};

use crate::router;

// ---------------------------------------------------------------------------
// Task ID generation
// ---------------------------------------------------------------------------

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

fn next_task_id() -> u64 {
    NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// Handle a `spawn` tool call.
///
/// # Arguments (from JSON)
///
/// - `tool` (required): Name of the tool to invoke.
/// - `arguments` (optional): Arguments to pass to the tool. Default `{}`.
///
/// Returns immediately with `{ "task_id": <id> }`.
pub async fn handle(arguments: &Value, ctx: &router::DispatchContext<'_>) -> Result<String> {
    let tool = arguments
        .get("tool")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required field: tool"))?;

    // Prevent spawning spawn (infinite recursion footgun).
    if tool == "spawn" {
        bail!("cannot spawn 'spawn' — use the inner tool name directly");
    }

    let inner_args = arguments
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    // Verify the tool exists before spawning so the agent gets an immediate
    // error for typos rather than a delayed task.failed event.
    if ctx.registry.get(tool).is_none() {
        bail!(
            "tool '{tool}' is not registered — check tools/list. \
             spawn only dispatches to registered tools."
        );
    }

    let task_id = next_task_id();
    let tool_name = tool.to_string();

    debug!(task_id, tool = %tool_name, "spawn: launching background task");

    // We need an owned context so the spawned task can outlive this borrow.
    let owned_ctx = ctx.to_owned();
    let eq = ctx.event_queue.clone();

    tokio::spawn(async move {
        let dispatch_ctx = owned_ctx.as_dispatch();
        let result = router::dispatch(&tool_name, &inner_args, &dispatch_ctx).await;

        match result {
            Ok(text) => {
                debug!(task_id, tool = %tool_name, "spawn: task completed");
                eq.push(
                    "task.done",
                    format!("spawn:{task_id}"),
                    json!({
                        "task_id": task_id,
                        "tool": tool_name,
                        "result": text,
                    }),
                )
                .await;
            }
            Err(e) => {
                debug!(task_id, tool = %tool_name, error = %e, "spawn: task failed");
                eq.push(
                    "task.failed",
                    format!("spawn:{task_id}"),
                    json!({
                        "task_id": task_id,
                        "tool": tool_name,
                        "error": format!("{e:#}"),
                    }),
                )
                .await;
            }
        }
    });

    info!(task_id, tool = %tool, "spawn: task started");
    Ok(serde_json::to_string_pretty(&json!({
        "task_id": task_id,
        "tool": tool,
        "status": "spawned",
    }))?)
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// Returns the MCP tool schema for the `spawn` tool.
pub fn spawn_tool_definition() -> serde_json::Value {
    json!({
        "name": "spawn",
        "description": "Run any tool asynchronously in the background. Returns a task_id immediately. The result arrives as an event: listen for topic 'task.done' or 'task.failed' with matching task_id. Use this for parallel execution (spawn N tools, listen for N results), long-running operations, or fire-and-forget jobs.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "tool": {
                    "type": "string",
                    "description": "Name of the tool to invoke (e.g. 'shell', 'run_code', 'web_fetch')."
                },
                "arguments": {
                    "type": "object",
                    "description": "Arguments to pass to the tool. Must match the tool's inputSchema. Omit or pass {} for tools with no required arguments."
                }
            },
            "required": ["tool"]
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_queue::EventQueue;

    #[tokio::test]
    async fn spawn_nonexistent_tool_fails_immediately() {
        // We need a minimal DispatchContext. Build one with an empty registry.
        let registry = crate::watcher::ManifestRegistry::new();
        let servers = crate::watcher::McpServerMap::new();
        let eq = EventQueue::new();
        let file = tempfile::NamedTempFile::new().unwrap();
        let perms = clawshake_core::permissions::PermissionStore::open(file.path())
            .await
            .unwrap();
        let shim_cache = crate::invoke::codemode::ShimCache::new();
        let cron = crate::invoke::cron::CronScheduler::new();
        let frame_store = crate::webview::FrameStore::new();

        let ctx = router::DispatchContext {
            registry: &registry,
            servers: &servers,
            event_queue: &eq,
            permissions: &perms,
            shim_cache: &shim_cache,
            cron: &cron,
            port: 0,
            code_mode: false,
            memory: None,
            frame_store: &frame_store,
        };

        let result = handle(&json!({"tool": "nonexistent_tool"}), &ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not registered"));
    }

    #[tokio::test]
    async fn spawn_spawn_is_blocked() {
        let registry = crate::watcher::ManifestRegistry::new();
        let servers = crate::watcher::McpServerMap::new();
        let eq = EventQueue::new();
        let file = tempfile::NamedTempFile::new().unwrap();
        let perms = clawshake_core::permissions::PermissionStore::open(file.path())
            .await
            .unwrap();
        let shim_cache = crate::invoke::codemode::ShimCache::new();
        let cron = crate::invoke::cron::CronScheduler::new();
        let frame_store = crate::webview::FrameStore::new();

        let ctx = router::DispatchContext {
            registry: &registry,
            servers: &servers,
            event_queue: &eq,
            permissions: &perms,
            shim_cache: &shim_cache,
            cron: &cron,
            port: 0,
            code_mode: false,
            memory: None,
            frame_store: &frame_store,
        };

        let result = handle(&json!({"tool": "spawn"}), &ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot spawn"));
    }

    #[tokio::test]
    async fn missing_tool_field() {
        let registry = crate::watcher::ManifestRegistry::new();
        let servers = crate::watcher::McpServerMap::new();
        let eq = EventQueue::new();
        let file = tempfile::NamedTempFile::new().unwrap();
        let perms = clawshake_core::permissions::PermissionStore::open(file.path())
            .await
            .unwrap();
        let shim_cache = crate::invoke::codemode::ShimCache::new();
        let cron = crate::invoke::cron::CronScheduler::new();
        let frame_store = crate::webview::FrameStore::new();

        let ctx = router::DispatchContext {
            registry: &registry,
            servers: &servers,
            event_queue: &eq,
            permissions: &perms,
            shim_cache: &shim_cache,
            cron: &cron,
            port: 0,
            code_mode: false,
            memory: None,
            frame_store: &frame_store,
        };

        let result = handle(&json!({}), &ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing"));
    }
}
