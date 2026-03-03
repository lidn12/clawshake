//! Built-in tools that ship with the broker.
//!
//! Instead of writing manifest JSON files to disk, tools are constructed as
//! `Tool` structs and registered directly in the `ManifestRegistry` at
//! startup.  Network tools are sourced from `clawshake_tools::schema` and
//! dispatched via IPC to the bridge daemon — no subprocess spawning required.

use clawshake_core::manifest::{InputSchema, InvokeConfig, Tool};
use serde_json::json;
use tracing::info;

use crate::watcher::ManifestRegistry;

/// Register all built-in tools directly in the registry.
///
/// - Event tools (`emit`, `listen`) are always registered.
/// - Code-mode tools (`run_code`, `describe_tools`) are registered when
///   `code_mode` is true (Node.js detected on PATH).
/// - Network tools are registered when `bridge_available` is true (bridge
///   daemon is running and responsive).
pub fn register(registry: &ManifestRegistry, code_mode: bool, bridge_available: bool) {
    // -- Event tools (always available) --
    for tool in event_tools() {
        info!(name = %tool.name, "Registered built-in tool");
        registry.register_builtin(tool, "events");
    }

    // -- Code-mode tools --
    if code_mode {
        for tool in codemode_tools() {
            info!(name = %tool.name, "Registered built-in tool");
            registry.register_builtin(tool, "codemode");
        }
    }

    // -- Network tools (from clawshake-tools schema) --
    if bridge_available {
        for tool in network_tools() {
            info!(name = %tool.name, "Registered built-in tool");
            registry.register_builtin(tool, "network");
        }
    } else {
        info!("Bridge not available — network tools not registered");
    }
}

/// Try to contact the bridge daemon over IPC to see if it is running.
pub async fn detect_bridge() -> bool {
    match clawshake_tools::client::send_request("ping", json!({})).await {
        Ok(_) => {
            info!("Bridge daemon detected via IPC");
            true
        }
        Err(e) => {
            info!("Bridge not detected: {e:#}");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Tool constructors
// ---------------------------------------------------------------------------

fn event_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "listen".into(),
            description: "Block until events matching the given topics arrive. Returns an array \
                of events and a cursor for resumption. Use this to wait for filesystem changes, \
                peer messages, webhook deliveries, or any other event. Call with no topics to \
                receive all events. The cursor enables efficient polling — pass the returned \
                cursor as 'after' in the next call to only receive new events."
                .into(),
            input_schema: InputSchema {
                r#type: "object".into(),
                properties: [
                    (
                        "topics".into(),
                        json!({
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Topic prefixes to filter on (e.g. ['fs', 'peer']). Empty or omitted = all topics."
                        }),
                    ),
                    (
                        "timeout_secs".into(),
                        json!({
                            "type": "number",
                            "description": "Max seconds to wait. 0 = block indefinitely. Default: 30"
                        }),
                    ),
                    (
                        "after".into(),
                        json!({
                            "type": "number",
                            "description": "Cursor (event ID). Only return events with id > after. Default: 0 (all buffered events)."
                        }),
                    ),
                ]
                .into(),
                required: vec![],
            },
            requires: None,
            invoke: InvokeConfig::InProcess,
        },
        Tool {
            name: "emit".into(),
            description: "Push an event into the local event queue. Other listeners on this node \
                can receive it via listen(). Use dot-separated topic names (e.g. 'task.complete', \
                'msg.agent'). To send events to a remote peer, use \
                network_call(peer_id, 'emit', {topic, data}) instead."
                .into(),
            input_schema: InputSchema {
                r#type: "object".into(),
                properties: [
                    (
                        "topic".into(),
                        json!({
                            "type": "string",
                            "description": "Event topic, e.g. 'task.complete', 'msg.agent'"
                        }),
                    ),
                    (
                        "data".into(),
                        json!({
                            "description": "Arbitrary JSON payload"
                        }),
                    ),
                ]
                .into(),
                required: vec!["topic".into(), "data".into()],
            },
            requires: None,
            invoke: InvokeConfig::InProcess,
        },
    ]
}

fn codemode_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "run_code".into(),
            description: "Execute a JavaScript script that can call any available tool as an \
                async function. Use describe_tools first to discover the JS API. Intermediate \
                results stay in the JS runtime and only console.log() output is returned. This \
                is dramatically more token-efficient for multi-step workflows."
                .into(),
            input_schema: InputSchema {
                r#type: "object".into(),
                properties: [(
                    "script".into(),
                    json!({
                        "type": "string",
                        "description": "JavaScript code to execute. All tool functions are pre-loaded as async functions. Wrap multi-step logic in a single script. Output is collected from console.log() calls. A top-level return value is also automatically printed, so both styles work: `console.log(result)` and `return result`."
                    }),
                )]
                .into(),
                required: vec!["script".into()],
            },
            requires: None,
            invoke: InvokeConfig::InProcess,
        },
        Tool {
            name: "describe_tools".into(),
            description: "Search available tools and return their JavaScript API definitions \
                for use with run_code. Call with no query to list all tool categories. Call with \
                a query to get specific tool function signatures."
                .into(),
            input_schema: InputSchema {
                r#type: "object".into(),
                properties: [(
                    "query".into(),
                    json!({
                        "type": "string",
                        "description": "Filter tools by name, source, or description substring. Omit for category summary."
                    }),
                )]
                .into(),
                required: vec![],
            },
            requires: None,
            invoke: InvokeConfig::InProcess,
        },
    ]
}

/// Convert `clawshake_tools::schema::tool_definitions()` into `Tool` structs
/// with `InvokeConfig::InProcess`.  The router dispatches these via IPC to
/// the bridge daemon.
fn network_tools() -> Vec<Tool> {
    clawshake_tools::schema::tool_definitions()
        .into_iter()
        .filter_map(|val| {
            let name = val.get("name")?.as_str()?.to_string();
            let description = val.get("description")?.as_str()?.to_string();
            let schema_val = val.get("inputSchema").cloned().unwrap_or(json!({
                "type": "object",
                "properties": {}
            }));
            let input_schema: InputSchema = serde_json::from_value(schema_val).unwrap_or_default();
            Some(Tool {
                name,
                description,
                input_schema,
                requires: None,
                invoke: InvokeConfig::InProcess,
            })
        })
        .collect()
}
