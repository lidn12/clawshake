//! Built-in tools that ship with the broker.
//!
//! Instead of writing manifest JSON files to disk, tools are constructed as
//! `Tool` structs and registered directly in the `ManifestRegistry` at
//! startup.  Network tools are dispatched via IPC to the bridge daemon —
//! no subprocess spawning required.

use clawshake_core::manifest::{InputSchema, InvokeConfig, Tool};
use serde_json::{json, Value};
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

    // -- General-purpose tools (always available) --
    for tool in general_tools() {
        info!(name = %tool.name, "Registered built-in tool");
        registry.register_builtin(tool, "general");
    }

    // -- Spawn tool (always available) --
    {
        let tool = spawn_tool();
        info!(name = %tool.name, "Registered built-in tool");
        registry.register_builtin(tool, "general");
    }

    // -- Cron tools (always available) --
    for tool in cron_tools() {
        info!(name = %tool.name, "Registered built-in tool");
        registry.register_builtin(tool, "cron");
    }

    // -- Webview UI tools (always available) --
    for tool in webview_tools() {
        info!(name = %tool.name, "Registered built-in tool");
        registry.register_builtin(tool, "webview");
    }

    // -- Memory tools (always registered; some are hidden) --
    #[cfg(feature = "memory")]
    for (tool, hidden) in memory_tools() {
        if hidden {
            info!(name = %tool.name, "Registered hidden memory tool");
        } else {
            info!(name = %tool.name, "Registered built-in tool");
        }
        registry.register_builtin_hidden(tool, "memory", hidden);
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
    match clawshake_core::ipc::send_request("ping", json!({})).await {
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
                cursor as 'after' in the next call to only receive new events. \
                Omitting 'after' defaults to HEAD, so only events emitted after this call are returned."
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
                            "description": "Max seconds to wait. 0 = block indefinitely. Default: 0"
                        }),
                    ),
                    (
                        "after".into(),
                        json!({
                            "type": "number",
                            "description": "Cursor (event ID). Only return events with id > after. Omit to start from now (no history replay). Pass 0 to replay all buffered events."
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

/// Build network tool definitions inline.
/// Dispatched via IPC to the bridge daemon by the router (except expose/unexpose
/// which are handled in-process).
fn network_tools() -> Vec<Tool> {
    network_tool_schemas()
        .iter()
        .filter_map(Tool::from_json_schema)
        .collect()
}

/// Inline MCP tool schemas for all `network_*` tools.
fn network_tool_schemas() -> Vec<Value> {
    vec![
        json!({
            "name": "network_peers",
            "description": "List all discovered bridge nodes on the network with their peer IDs, addresses, and last-seen timestamps.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        }),
        json!({
            "name": "network_tools",
            "description": "Progressive tool discovery for a remote peer. Without a query, returns a flat list of the peer's published tool names. With a query, returns matching tools with their full name, description, and inputSchema.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": { "type": "string", "description": "libp2p peer ID string (from network_peers)" },
                    "query": { "type": "string", "description": "Filter tools by name or description substring. Omit for a flat name list." }
                },
                "required": ["peer_id"]
            }
        }),
        json!({
            "name": "network_search",
            "description": "Search for tools across all known peers by tool name or description substring.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Case-insensitive substring to match against tool names and descriptions" }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "network_ping",
            "description": "Check whether a peer currently has an active connection to this node.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": { "type": "string", "description": "libp2p peer ID string" }
                },
                "required": ["peer_id"]
            }
        }),
        json!({
            "name": "network_call",
            "description": "Invoke a tool on a specific remote peer over the P2P network and return its result.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": { "type": "string", "description": "libp2p peer ID string of the target peer" },
                    "tool": { "type": "string", "description": "Tool name to invoke on the remote peer" },
                    "arguments": { "type": "object", "description": "Arguments to pass to the tool. Must match the tool's inputSchema." }
                },
                "required": ["peer_id", "tool"]
            }
        }),
        json!({
            "name": "network_record",
            "description": "Fetch the raw DHT announcement record for a peer directly from the Kademlia DHT (live, not cached).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": { "type": "string", "description": "libp2p peer ID string (from network_peers)" }
                },
                "required": ["peer_id"]
            }
        }),
        json!({
            "name": "network_expose",
            "description": "Expose a local TCP port to the P2P network. Registers a connect_{name} tool that remote peers can call to establish a tunneled connection.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "port": { "type": "integer", "description": "Local TCP port to expose" },
                    "name": { "type": "string", "description": "Short name for the expose (e.g. 'jupyter', 'preview')" },
                    "description": { "type": "string", "description": "Optional human-readable description of the exposed service." },
                    "peers": { "type": "array", "items": { "type": "string" }, "description": "Optional peer ID allowlist. If omitted, any connected peer can connect." }
                },
                "required": ["port", "name"]
            }
        }),
        json!({
            "name": "network_unexpose",
            "description": "Stop exposing a previously shared port. Unregisters the connect_{name} tool and tears down any active tunnels.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the expose to remove (the same name passed to network_expose)." }
                },
                "required": ["name"]
            }
        }),
        json!({
            "name": "network_models",
            "description": "List AI models available on the peer-to-peer network.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": { "type": "string", "description": "Filter to models from a specific peer. Omit to list models from all peers." }
                }
            }
        }),
    ]
}

/// Build the shell tool from the in-crate definition.
/// Dispatched in-process (no IPC) by the router.
fn general_tools() -> Vec<Tool> {
    let val = crate::invoke::shell::tool_definition();
    Tool::from_json_schema(&val).into_iter().collect()
}

/// Convert cron tool definitions into `Tool` structs.
/// These are dispatched in-process to the [`CronScheduler`](crate::invoke::cron::CronScheduler).
fn cron_tools() -> Vec<Tool> {
    crate::invoke::cron::cron_tool_definitions()
        .iter()
        .filter_map(Tool::from_json_schema)
        .collect()
}

/// Build the `spawn` tool from its schema definition.
fn spawn_tool() -> Tool {
    let val = crate::invoke::spawn::spawn_tool_definition();
    Tool::from_json_schema(&val).expect("spawn tool definition must be valid")
}

/// Convert webview tool definitions from `invoke::webview` into `Tool` structs.
fn webview_tools() -> Vec<Tool> {
    crate::invoke::webview::webview_tool_definitions()
        .iter()
        .filter_map(Tool::from_json_schema)
        .collect()
}

/// Build memory tool definitions.  Returns `(Tool, hidden)` pairs.
///
/// `memory_recall` is visible to agents.  Infrastructure and operational
/// tools (`memory_procedural`, `memory_append`, `memory_ingest`,
/// `memory_embed`) are hidden — callable but excluded from `tools/list`.
#[cfg(feature = "memory")]
fn memory_tools() -> Vec<(Tool, bool)> {
    crate::invoke::memory::memory_tool_definitions()
        .iter()
        .filter_map(|val| {
            let hidden = val.get("hidden").and_then(|v| v.as_bool()).unwrap_or(false);
            let tool = Tool::from_json_schema(val)?;
            Some((tool, hidden))
        })
        .collect()
}
