use anyhow::Result;
use clawshake_core::{
    permissions::PermissionStore,
    protocol::{
        JsonRpcRequest, JsonRpcResponse, McpContent, McpToolDef, ToolsCallParams, ToolsCallResult,
        ToolsListResult,
    },
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::debug;

use crate::{
    event_queue::EventQueue,
    invoke::codemode::ShimCache,
    router,
    watcher::{ManifestRegistry, McpServerMap},
};

// JSON-RPC error codes
const PARSE_ERROR: i64 = -32700;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// MCP stdio server.
///
/// Reads newline-delimited JSON-RPC 2.0 from stdin, writes responses to stdout.
/// All callers over stdio are treated as `AgentId::Local`.
pub async fn serve_stdio(
    registry: ManifestRegistry,
    permissions: PermissionStore,
    servers: McpServerMap,
    shim_cache: ShimCache,
    code_mode: bool,
    event_queue: EventQueue,
) -> Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();
    let mut out = stdout;

    // stdio mode doesn't have an HTTP port for /invoke callbacks,
    // so code mode tools won't work over stdio. We still pass port=0
    // but run_code will fail gracefully if called.
    let port = 0u16;

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        debug!(line = %line, "← stdin");

        let response = match serde_json::from_str::<JsonRpcRequest>(&line) {
            Err(e) => Some(JsonRpcResponse::err(
                None,
                PARSE_ERROR,
                format!("Parse error: {e}"),
            )),
            Ok(req) => {
                handle(
                    &req,
                    &registry,
                    &permissions,
                    &servers,
                    &shim_cache,
                    port,
                    code_mode,
                    &event_queue,
                )
                .await
            }
        };

        if let Some(resp) = response {
            let mut bytes = serde_json::to_vec(&resp)?;
            bytes.push(b'\n');
            debug!(json = %String::from_utf8_lossy(&bytes).trim_end(), "→ stdout");
            out.write_all(&bytes).await?;
            out.flush().await?;
        }
    }
    Ok(())
}

/// Handle one decoded request.  Returns `None` for notifications (no response).
pub(crate) async fn handle(
    req: &JsonRpcRequest,
    registry: &ManifestRegistry,
    permissions: &PermissionStore,
    servers: &McpServerMap,
    shim_cache: &ShimCache,
    port: u16,
    code_mode: bool,
    event_queue: &EventQueue,
) -> Option<JsonRpcResponse> {
    let id = req.id.clone();
    match req.method.as_str() {
        // ---------------------------------------------------------------
        "initialize" => {
            let resp = json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "clawshake-broker",
                    "version": env!("CARGO_PKG_VERSION")
                }
            });
            Some(JsonRpcResponse::ok(id, resp))
        }

        // ---------------------------------------------------------------
        "ping" => {
            // MCP liveness ping — respond with empty result.
            Some(JsonRpcResponse::ok(id, json!({})))
        }

        // ---------------------------------------------------------------
        "notifications/initialized" => {
            // Client notification — no response.
            None
        }

        // ---------------------------------------------------------------
        "notifications/cancelled" => {
            // Client cancelled an in-flight request — no response.
            // (We don't currently track cancellable requests, but we must
            // not return METHOD_NOT_FOUND for this standard notification.)
            None
        }

        // ---------------------------------------------------------------
        "tools/list" => {
            let all_tools = registry.all();
            let defs: Vec<McpToolDef> = all_tools
                .into_iter()
                .filter(|lt| {
                    // In code mode (--code-mode), hide individual tools from
                    // the listing — only show run_code + describe_tools.
                    // The hidden tools are still callable by name via run_code.
                    if code_mode && lt.source != "codemode" {
                        return false;
                    }
                    true
                })
                .map(|lt| {
                    let schema = serde_json::to_value(&lt.tool.input_schema)
                        .unwrap_or_else(|_| json!({"type":"object","properties":{}}));
                    McpToolDef {
                        name: lt.tool.name.clone(),
                        description: lt.tool.description.clone(),
                        input_schema: schema,
                    }
                })
                .collect();
            let result = ToolsListResult { tools: defs };
            Some(JsonRpcResponse::ok(
                id,
                serde_json::to_value(result).expect("MCP result serializes to JSON"),
            ))
        }

        // ---------------------------------------------------------------
        "tools/call" => {
            let params = match req
                .params
                .as_ref()
                .and_then(|p| serde_json::from_value::<ToolsCallParams>(p.clone()).ok())
            {
                Some(p) => p,
                None => {
                    return Some(JsonRpcResponse::err(
                        id,
                        INVALID_PARAMS,
                        "tools/call requires {name, arguments}",
                    ));
                }
            };

            // Dispatch (permission check happens inside router::dispatch).
            let arguments = serde_json::to_value(&params.arguments)
                .unwrap_or(Value::Object(Default::default()));
            let ctx = router::DispatchContext {
                registry,
                servers,
                event_queue,
                permissions,
                shim_cache,
                port,
            };
            let (content, is_error) = match router::dispatch(&params.name, &arguments, &ctx).await {
                Ok(text) => (vec![McpContent::text(text)], false),
                Err(e) => (
                    vec![McpContent::text(format!("Tool '{}': {}", params.name, e))],
                    true,
                ),
            };
            let result = ToolsCallResult { content, is_error };
            Some(JsonRpcResponse::ok(
                id,
                serde_json::to_value(result).expect("MCP result serializes to JSON"),
            ))
        }

        // ---------------------------------------------------------------
        other => Some(JsonRpcResponse::err(
            id,
            METHOD_NOT_FOUND,
            format!("Method not found: {other}"),
        )),
    }
}
