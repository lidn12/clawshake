//! Code mode: `run_code` and `describe_tools` handlers.
//!
//! Generates a JavaScript shim from the `ManifestRegistry` (and optionally
//! `PeerTable`) so agents can write JS scripts that chain tool calls.
//! The shim defines async helper functions — one per tool — that call back
//! into the broker via `POST /invoke`.  The agent's script is wrapped in
//! an async IIFE and piped to `node -` via stdin.

use std::collections::HashMap;
use std::fmt::Write;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use anyhow::{bail, Result};
use axum::{extract::State, routing::post, Router};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{debug, warn};

use crate::watcher::{LoadedTool, ManifestRegistry};

// ---------------------------------------------------------------------------
// Shim cache
// ---------------------------------------------------------------------------

/// Cached shim: full JS source + a compact category summary string.
#[derive(Debug, Clone)]
pub(crate) struct CachedShim {
    /// Full JS source with all tool functions and the `_call` helper.
    pub(crate) full_js: String,
    /// Compact category summary for `describe_tools` with no query.
    /// e.g. "- mail (3 tools): send, search, read\n"
    pub(crate) categories: String,
}

/// Thread-safe shim cache.  Regenerated whenever the manifest registry changes.
#[derive(Clone, Default)]
pub struct ShimCache {
    inner: Arc<RwLock<Option<CachedShim>>>,
}

impl ShimCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Invalidate the cache so the next request regenerates the shim.
    #[allow(dead_code)] // Will be wired to watcher change events.
    pub fn invalidate(&self) {
        let mut cache = self.inner.write().expect("shim cache lock");
        *cache = None;
    }

    /// Get or generate the shim for the given broker port and registry.
    fn get_or_generate(&self, port: u16, registry: &ManifestRegistry) -> CachedShim {
        {
            let cache = self.inner.read().expect("shim cache lock");
            if let Some(ref shim) = *cache {
                return shim.clone();
            }
        }

        let tools = registry.all();
        let shim = generate_shim(port, &tools);

        let mut cache = self.inner.write().expect("shim cache lock");
        *cache = Some(shim.clone());
        shim
    }
}

// ---------------------------------------------------------------------------
// Shim generator
// ---------------------------------------------------------------------------

/// Generate the full JS shim and category summary from the loaded tools.
pub(crate) fn generate_shim(port: u16, tools: &[LoadedTool]) -> CachedShim {
    // Group tools by source.
    let mut by_source: HashMap<&str, Vec<&LoadedTool>> = HashMap::new();
    for lt in tools {
        // Skip code-mode tools themselves to avoid recursion.
        if lt.source == "codemode" {
            continue;
        }
        by_source.entry(&lt.source).or_default().push(lt);
    }

    // Sort sources for deterministic output.
    let mut sources: Vec<&&str> = by_source.keys().collect();
    sources.sort();

    // --- Build full JS shim ---
    let mut js = String::with_capacity(4096);

    // _call helper
    writeln!(js, "const _BROKER = 'http://127.0.0.1:{port}';").unwrap();
    writeln!(js, "async function _call(name, args) {{").unwrap();
    writeln!(js, "  const r = await fetch(`${{_BROKER}}/invoke`, {{").unwrap();
    writeln!(js, "    method: 'POST',").unwrap();
    writeln!(js, "    headers: {{'Content-Type': 'application/json'}},").unwrap();
    writeln!(
        js,
        "    body: JSON.stringify({{tool: name, arguments: args || {{}}}})"
    )
    .unwrap();
    writeln!(js, "  }});").unwrap();
    writeln!(
        js,
        "  if (!r.ok) throw new Error(`invoke failed: ${{r.status}}`);"
    )
    .unwrap();
    writeln!(js, "  const j = await r.json();").unwrap();
    writeln!(js, "  if (j.is_error) throw new Error(j.result);").unwrap();
    writeln!(
        js,
        "  try {{ return JSON.parse(j.result); }} catch {{ return j.result; }}"
    )
    .unwrap();
    writeln!(js, "}}").unwrap();
    writeln!(js).unwrap();

    // --- Build category summary ---
    let mut categories = String::with_capacity(512);

    for source in &sources {
        let group = &by_source[**source];

        // Category summary line — show flat MCP names so agents know exactly what to call.
        let names_preview: Vec<String> = group.iter().map(|lt| lt.tool.name.clone()).collect();
        writeln!(
            categories,
            "- {} ({} tools): {}",
            source,
            group.len(),
            names_preview.join(", ")
        )
        .unwrap();

        // Flat const declaration per tool — JS name == MCP name, no translation.
        for lt in group.iter() {
            writeln!(js, "/** {} */", lt.tool.description.replace("*/", "* /")).unwrap();
            writeln!(
                js,
                "const {} = (args) => _call('{}', args);",
                safe_js_ident(&lt.tool.name),
                lt.tool.name
            )
            .unwrap();
        }
        writeln!(js).unwrap();
    }

    CachedShim {
        full_js: js,
        categories,
    }
}

/// Generate a filtered JS shim containing only tools matching the query.
fn generate_filtered_shim(tools: &[LoadedTool], query: &str) -> String {
    let query_lower = query.to_lowercase();
    let keywords: Vec<&str> = query_lower.split_whitespace().collect();

    let filtered: Vec<&LoadedTool> = tools
        .iter()
        .filter(|lt| {
            // Skip code-mode tools themselves.
            if lt.source == "codemode" {
                return false;
            }
            // Match if any keyword appears in source, name, or description.
            keywords.iter().any(|kw| {
                lt.source.to_lowercase().contains(kw)
                    || lt.tool.name.to_lowercase().contains(kw)
                    || lt.tool.description.to_lowercase().contains(kw)
            })
        })
        .collect();

    if filtered.is_empty() {
        return format!(
            "No tools matching '{query}'. Call describe_tools with no query to see all categories."
        );
    }

    // Group by source.
    let mut by_source: HashMap<&str, Vec<&&LoadedTool>> = HashMap::new();
    for lt in &filtered {
        by_source.entry(&lt.source).or_default().push(lt);
    }
    let mut sources: Vec<&&str> = by_source.keys().collect();
    sources.sort();

    let mut js = String::with_capacity(2048);

    // Include _call helper in filtered output too so agents can copy-paste
    writeln!(
        js,
        "// Use these functions inside run_code's script parameter:"
    )
    .unwrap();
    writeln!(js).unwrap();

    for source in &sources {
        let group = &by_source[**source];
        for lt in group {
            // Build param hints from inputSchema
            let param_hint = param_hint_from_schema(&lt.tool.input_schema);
            writeln!(
                js,
                "/** {} @param {{{}}} args */",
                lt.tool.description.replace("*/", "* /"),
                param_hint
            )
            .unwrap();

            writeln!(
                js,
                "const {} = (args) => _call('{}', args);",
                safe_js_ident(&lt.tool.name),
                lt.tool.name
            )
            .unwrap();
        }
        writeln!(js).unwrap();
    }

    js
}

/// Build a compact parameter hint string from `InputSchema`.
fn param_hint_from_schema(schema: &clawshake_core::manifest::InputSchema) -> String {
    if schema.properties.is_empty() {
        return String::new();
    }
    let mut parts = Vec::new();
    for (key, val) in &schema.properties {
        let typ = val.get("type").and_then(|t| t.as_str()).unwrap_or("any");
        let required = schema.required.contains(key);
        if required {
            parts.push(format!("{key}: {typ}"));
        } else {
            parts.push(format!("{key}?: {typ}"));
        }
    }
    parts.join(", ")
}

/// Make a string safe as a JS identifier (replace non-alphanumeric with _).
fn safe_js_ident(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    // Ensure it doesn't start with a digit.
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    if out.is_empty() {
        out.push_str("_unnamed");
    }
    out
}

// ---------------------------------------------------------------------------
// run_code
// ---------------------------------------------------------------------------

/// Execute agent-supplied JavaScript by piping it to `node -` via stdin.
///
/// The generated JS shim is prepended so the agent's code can call tool
/// functions directly.  stdout is captured and returned.  stderr + exit
/// code signal errors.
///
/// When `port` is 0 (stdio mode), an ephemeral HTTP server is spun up on
/// a random port for the duration of the call so the Node.js subprocess
/// can call tools back via `POST /invoke`.
pub async fn invoke_run_code(
    script: &str,
    ctx: &crate::router::DispatchContext<'_>,
    timeout_secs: u64,
) -> Result<String> {
    // If we're in stdio mode (no HTTP server), spin up an ephemeral one.
    let (effective_port, _ephemeral_guard) = if ctx.port == 0 {
        let (p, guard) = start_ephemeral_invoke_server(ctx).await?;
        (p, Some(guard))
    } else {
        (ctx.port, None)
    };

    let cached = ctx.shim_cache.get_or_generate(effective_port, ctx.registry);

    // Combine shim + agent script in an async IIFE.
    // If the script returns a non-undefined value it is automatically printed,
    // so both `return result` and `console.log(result)` work as output.
    let combined = format!(
        "{}\n;(async () => {{\n{}\n}})().then(r => {{ if (r !== undefined) console.log(typeof r === 'string' ? r : JSON.stringify(r, null, 2)); }}).catch(e => {{ console.error(e.message || e); process.exit(1); }});\n",
        cached.full_js, script
    );

    debug!(script_len = combined.len(), "run_code: spawning node");

    // Resolve the node executable path explicitly so that the broker can find
    // it even when run as a child process of VS Code (which may have a
    // stripped PATH that doesn't include the user's Node.js install dir).
    let node_exe = which::which("node")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "node".to_string());

    let mut child = Command::new(&node_exe)
        .arg("-") // read from stdin
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            anyhow::anyhow!("Failed to spawn node ({node_exe}): {e}. Is Node.js installed?")
        })?;

    // Write script to stdin then close it.
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(combined.as_bytes()).await?;
        // stdin is dropped here, closing the pipe.
    }

    // Apply timeout.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        child.wait_with_output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();

            if output.status.success() {
                debug!(stdout_len = stdout.len(), "run_code: success");
                Ok(stdout)
            } else {
                let msg = if stderr.is_empty() {
                    format!("Script exited with code {}", output.status)
                } else {
                    stderr
                };
                bail!("{msg}")
            }
        }
        Ok(Err(e)) => bail!("Failed to wait for node process: {e}"),
        Err(_) => {
            // Timeout — child has been moved into wait_with_output() which
            // timed out.  Tokio drops the Child (and kills it) when the
            // future is dropped, so no explicit kill is needed here.
            warn!("run_code: timeout after {timeout_secs}s");
            bail!("Script timed out after {timeout_secs} seconds")
        }
    }
}

// ---------------------------------------------------------------------------
// describe_tools
// ---------------------------------------------------------------------------

/// Handle `describe_tools` calls.
///
/// - No query → return category summary.
/// - With query → return filtered JS shim showing matching tool functions.
pub fn invoke_describe_tools(
    query: Option<&str>,
    ctx: &crate::router::DispatchContext<'_>,
) -> String {
    match query {
        None | Some("") => {
            let cached = ctx.shim_cache.get_or_generate(ctx.port, ctx.registry);
            format!(
                "Available tool categories:\n{}\nCall describe_tools with a tool name or keyword to get its JS function signature.",
                cached.categories
            )
        }
        Some(q) => {
            let tools = ctx.registry.all();
            generate_filtered_shim(&tools, q)
        }
    }
}

// ---------------------------------------------------------------------------
// Ephemeral invoke server (for stdio mode)
// ---------------------------------------------------------------------------

/// Guard that shuts down the ephemeral server when dropped.
pub struct EphemeralServerGuard {
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for EphemeralServerGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Spin up a temporary HTTP server with only `POST /invoke` on a random port.
///
/// Returns `(port, guard)`.  The server runs until the guard is dropped.
async fn start_ephemeral_invoke_server(
    ctx: &crate::router::DispatchContext<'_>,
) -> Result<(u16, EphemeralServerGuard)> {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0u16))).await?;
    let port = listener.local_addr()?.port();

    let mut state = ctx.to_owned();
    state.port = port;

    let app = Router::new()
        .route("/invoke", post(ephemeral_invoke_handler))
        .with_state(state);

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    debug!(port, "Ephemeral invoke server started");

    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .ok();
        debug!("Ephemeral invoke server stopped");
    });

    Ok((
        port,
        EphemeralServerGuard {
            shutdown_tx: Some(shutdown_tx),
        },
    ))
}

async fn ephemeral_invoke_handler(
    State(state): State<crate::router::BrokerContext>,
    body: String,
) -> axum::response::Response {
    debug!(body = %body, "← POST /invoke (ephemeral)");
    let ctx = state.as_dispatch();
    crate::router::dispatch_invoke(&body, &ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawshake_core::manifest::{InvokeConfig, Tool};

    use crate::watcher::{LoadedTool, ManifestRegistry, McpServerMap};

    fn make_loaded_tool(name: &str, source: &str, description: &str) -> LoadedTool {
        LoadedTool {
            tool: Tool {
                name: name.to_string(),
                description: description.to_string(),
                input_schema: Default::default(),
                requires: None,
                invoke: InvokeConfig::InProcess,
            },
            source: source.to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // generate_shim
    // -----------------------------------------------------------------------

    #[test]
    fn shim_contains_call_helper() {
        let tools = vec![make_loaded_tool("some_tool", "misc", "does stuff")];
        let shim = generate_shim(7474, &tools);
        assert!(
            shim.full_js.contains("async function _call(name, args)"),
            "shim must define _call: {}",
            shim.full_js
        );
        assert!(shim.full_js.contains("fetch"), "shim must use fetch");
    }

    #[test]
    fn shim_flat_function_names() {
        let tools = vec![
            make_loaded_tool("network_peers", "network", "list peers"),
            make_loaded_tool("network_ping", "network", "ping a peer"),
        ];
        let shim = generate_shim(7474, &tools);
        assert!(
            shim.full_js
                .contains("const network_peers = (args) => _call('network_peers', args);"),
            "flat const declaration expected:\n{}",
            shim.full_js
        );
        assert!(
            shim.full_js
                .contains("const network_ping = (args) => _call('network_ping', args);"),
            "flat const declaration expected:\n{}",
            shim.full_js
        );
        assert!(
            !shim.full_js.contains("const network = {"),
            "must NOT use namespace objects:\n{}",
            shim.full_js
        );
    }

    #[test]
    fn shim_category_summary_flat_names() {
        let tools = vec![
            make_loaded_tool("network_peers", "network", "list peers"),
            make_loaded_tool("network_ping", "network", "ping a peer"),
        ];
        let shim = generate_shim(7474, &tools);
        assert!(
            shim.categories.contains("network (2 tools)"),
            "categories must list tool count:\n{}",
            shim.categories
        );
        // The category line should reference the flat MCP names.
        assert!(
            shim.categories.contains("network_peers"),
            "categories must show flat tool names:\n{}",
            shim.categories
        );
        // Must NOT use dot-notation like "network.peers".
        assert!(
            !shim.categories.contains("network."),
            "categories must not use dot-notation:\n{}",
            shim.categories
        );
    }

    #[test]
    fn shim_skips_codemode_tools() {
        let tools = vec![
            make_loaded_tool("run_code", "codemode", "run JS"),
            make_loaded_tool("real_tool", "my_source", "a real tool"),
        ];
        let shim = generate_shim(7474, &tools);
        assert!(
            !shim.full_js.contains("run_code"),
            "codemode tools must be skipped:\n{}",
            shim.full_js
        );
        assert!(
            shim.full_js.contains("real_tool"),
            "other tools must be included:\n{}",
            shim.full_js
        );
    }

    #[test]
    fn shim_safe_js_ident() {
        // Hyphens in tool names must be replaced with underscores in the JS ident,
        // but the _call invocation must use the original MCP name.
        let tools = vec![make_loaded_tool("my-tool", "src", "hyphenated")];
        let shim = generate_shim(7474, &tools);
        assert!(
            shim.full_js.contains("const my_tool ="),
            "hyphen must become underscore in JS ident:\n{}",
            shim.full_js
        );
        assert!(
            shim.full_js.contains("_call('my-tool'"),
            "original MCP name must be preserved in _call:\n{}",
            shim.full_js
        );
    }

    // -----------------------------------------------------------------------
    // invoke_describe_tools — uses DispatchContext (async, needs PermissionStore)
    // -----------------------------------------------------------------------

    async fn make_ctx_with_tools(
        tools: Vec<LoadedTool>,
    ) -> (
        ManifestRegistry,
        McpServerMap,
        crate::event_queue::EventQueue,
        ShimCache,
    ) {
        let registry = ManifestRegistry::new();
        for lt in tools {
            registry.register_builtin(lt.tool, &lt.source);
        }
        let servers = McpServerMap::new();
        let event_queue = crate::event_queue::EventQueue::new();
        let shim_cache = ShimCache::new();
        (registry, servers, event_queue, shim_cache)
    }

    #[tokio::test]
    async fn describe_tools_no_query_returns_categories() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let perms = clawshake_core::permissions::PermissionStore::open(file.path())
            .await
            .unwrap();

        let tools = vec![
            make_loaded_tool("mail_send", "mail", "Send mail"),
            make_loaded_tool("notes_create", "notes", "Create note"),
        ];
        let (registry, servers, event_queue, shim_cache) = make_ctx_with_tools(tools).await;

        let ctx = crate::router::DispatchContext {
            registry: &registry,
            servers: &servers,
            event_queue: &event_queue,
            permissions: &perms,
            shim_cache: &shim_cache,
            port: 7474,
            code_mode: false,
        };

        let output = crate::invoke::codemode::invoke_describe_tools(None, &ctx);
        assert!(
            output.contains("mail"),
            "should list mail category:\n{output}"
        );
        assert!(
            output.contains("notes"),
            "should list notes category:\n{output}"
        );
    }

    #[tokio::test]
    async fn describe_tools_with_query_returns_filtered_js() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let perms = clawshake_core::permissions::PermissionStore::open(file.path())
            .await
            .unwrap();

        // Build network_peers with a concrete InputSchema so param_hint_from_schema
        // has something to emit.  The test then asserts the specific field name
        // appears — a bug that returned the wrong property name or skipped it would
        // fail despite @param still being present.
        let mut props = std::collections::HashMap::new();
        props.insert("peer_id".to_string(), serde_json::json!({"type": "string"}));
        let schema = clawshake_core::manifest::InputSchema {
            r#type: "object".to_string(),
            properties: props,
            required: vec!["peer_id".to_string()],
        };
        let network_tool = LoadedTool {
            tool: clawshake_core::manifest::Tool {
                name: "network_peers".to_string(),
                description: "List peers".to_string(),
                input_schema: schema,
                requires: None,
                invoke: clawshake_core::manifest::InvokeConfig::InProcess,
            },
            source: "network".to_string(),
        };

        let tools = vec![
            network_tool,
            make_loaded_tool("mail_send", "mail", "Send mail"),
        ];
        let (registry, servers, event_queue, shim_cache) = make_ctx_with_tools(tools).await;

        let ctx = crate::router::DispatchContext {
            registry: &registry,
            servers: &servers,
            event_queue: &event_queue,
            permissions: &perms,
            shim_cache: &shim_cache,
            port: 7474,
            code_mode: false,
        };

        let output = crate::invoke::codemode::invoke_describe_tools(Some("network"), &ctx);
        assert!(
            output.contains("network_peers"),
            "network tool expected:\n{output}"
        );
        assert!(
            !output.contains("mail_send"),
            "mail tool must be filtered out:\n{output}"
        );
        // Assert the specific property name emitted by param_hint_from_schema,
        // not just the bare @param keyword.
        assert!(
            output.contains("peer_id"),
            "@param hint must include the 'peer_id' property name:\n{output}"
        );
    }

    #[tokio::test]
    async fn describe_tools_no_match_returns_message() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let perms = clawshake_core::permissions::PermissionStore::open(file.path())
            .await
            .unwrap();

        let tools = vec![make_loaded_tool("mail_send", "mail", "Send mail")];
        let (registry, servers, event_queue, shim_cache) = make_ctx_with_tools(tools).await;

        let ctx = crate::router::DispatchContext {
            registry: &registry,
            servers: &servers,
            event_queue: &event_queue,
            permissions: &perms,
            shim_cache: &shim_cache,
            port: 7474,
            code_mode: false,
        };

        let output = crate::invoke::codemode::invoke_describe_tools(Some("nonexistent"), &ctx);
        assert!(
            output.contains("No tools matching"),
            "expected 'No tools matching' message:\n{output}"
        );
    }
}
