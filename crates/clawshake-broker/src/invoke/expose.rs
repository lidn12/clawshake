//! Handlers for `network_expose`, `network_unexpose`, and dynamic `connect_*` tools.

use anyhow::Result;
use clawshake_core::manifest::{InputSchema, InvokeConfig, Tool};
use serde_json::{json, Value};

use crate::expose::{ExposeEntry, ExposeTable};
use crate::watcher::ManifestRegistry;

/// Handle `network_expose(port, name, description?, peers?)`.
///
/// Creates an entry in the expose table, dynamically registers a
/// `connect_{name}` tool in the manifest registry, and notifies the bridge
/// daemon via IPC so it can accept inbound tunnel streams.
pub async fn handle_expose(
    arguments: &Value,
    expose_table: &ExposeTable,
    registry: &ManifestRegistry,
    reannounce_tx: Option<&tokio::sync::mpsc::Sender<()>>,
) -> Result<String> {
    let port = arguments
        .get("port")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: port"))?;
    let port =
        u16::try_from(port).map_err(|_| anyhow::anyhow!("port must be a valid u16 (0–65535)"))?;

    let name = arguments
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: name"))?
        .to_string();

    // Validate name: must be a valid tool-name suffix (alphanumeric + underscore).
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        anyhow::bail!(
            "name must be non-empty and contain only alphanumeric characters or underscores"
        );
    }

    let description = arguments
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let peers: Option<Vec<String>> = arguments.get("peers").and_then(|v| {
        v.as_array().map(|arr| {
            arr.iter()
                .filter_map(|p| p.as_str().map(|s| s.to_string()))
                .collect()
        })
    });

    let expose_id = format!("expose_{}", uuid::Uuid::new_v4());

    // Check for duplicate name.
    if expose_table.get(&name).is_some() {
        anyhow::bail!("an expose with name '{name}' already exists — call network_unexpose first");
    }

    let entry = ExposeEntry {
        expose_id: expose_id.clone(),
        name: name.clone(),
        port,
        description: description.clone(),
        peers: peers.clone(),
    };
    expose_table.insert(entry);

    // Dynamically register `connect_{name}` tool in the manifest registry.
    let tool_name = format!("connect_{name}");
    let tool_description = match &description {
        Some(desc) => format!(
            "Connect to {name} on this peer. {desc}. Returns a local URL for the tunneled service."
        ),
        None => {
            format!("Connect to {name} on this peer. Returns a local URL for the tunneled service.")
        }
    };

    let tool = Tool {
        name: tool_name.clone(),
        description: tool_description,
        input_schema: InputSchema {
            r#type: "object".into(),
            properties: [(
                "local_port".into(),
                json!({
                    "type": "integer",
                    "description": "Optional. Local port to bind the tunnel on. If omitted, an ephemeral port is assigned."
                }),
            )]
            .into(),
            required: vec![],
        },
        requires: None,
        invoke: InvokeConfig::InProcess,
    };

    // Use a unique source per expose so we can unregister just this tool.
    let source = format!("expose:{name}");
    registry.register_builtin(tool, &source);
    tracing::info!(name, port, tool = %tool_name, "Registered expose + connect tool");

    // Notify the bridge daemon so it can accept inbound tunnel streams for
    // this name.  Best-effort: if the bridge is not running, the expose still
    // works for local operations and will be synced when the bridge comes up.
    let ipc_params = json!({
        "name": name,
        "port": port,
        "peers": peers,
    });
    if let Err(e) = clawshake_core::ipc::send_request("tunnel_register", ipc_params).await {
        tracing::warn!("Failed to notify bridge about expose '{name}': {e:#}");
    }

    // Trigger a DHT re-announce so peers discover the new connect_* tool.
    if let Some(tx) = reannounce_tx {
        let _ = tx.try_send(());
    }

    let result = json!({
        "expose_id": expose_id,
        "name": name,
        "port": port,
        "tool": format!("connect_{name}"),
    });
    Ok(serde_json::to_string_pretty(&result)?)
}

/// Handle `network_unexpose(name)`.
///
/// Removes the expose entry, unregisters the `connect_{name}` tool, and
/// notifies the bridge to tear down any active tunnels.
pub async fn handle_unexpose(
    arguments: &Value,
    expose_table: &ExposeTable,
    registry: &ManifestRegistry,
    reannounce_tx: Option<&tokio::sync::mpsc::Sender<()>>,
) -> Result<String> {
    let name = arguments
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: name"))?;

    if !expose_table.remove(name) {
        anyhow::bail!("no active expose with name '{name}'");
    }

    // Unregister the dynamically created tool.
    let source = format!("expose:{name}");
    registry.unload_source(&source);
    tracing::info!(name, "Unregistered expose + connect tool");

    // Notify the bridge daemon.
    let ipc_params = json!({ "name": name });
    if let Err(e) = clawshake_core::ipc::send_request("tunnel_unregister", ipc_params).await {
        tracing::warn!("Failed to notify bridge about unexpose '{name}': {e:#}");
    }

    // Trigger a DHT re-announce so peers drop the stale connect_* tool.
    if let Some(tx) = reannounce_tx {
        let _ = tx.try_send(());
    }

    let result = json!({ "ok": true, "name": name });
    Ok(serde_json::to_string_pretty(&result)?)
}
///
/// Looks up the expose table and returns the authorization response that the
/// caller's bridge will intercept to set up the tunnel.
pub fn handle_connect(
    tool_name: &str,
    arguments: &Value,
    expose_table: &ExposeTable,
) -> Result<String> {
    // Strip the `connect_` prefix to get the expose name.
    let name = tool_name
        .strip_prefix("connect_")
        .ok_or_else(|| anyhow::anyhow!("unexpected tool name: {tool_name}"))?;

    let entry = expose_table
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("no active expose with name '{name}'"))?;

    // Echo back local_port if the caller provided one (passthrough convention).
    let local_port = arguments.get("local_port").and_then(|v| v.as_u64());

    let mut response = json!({
        "authorized": true,
        "name": name,
        "port": entry.port,
    });
    if let Some(lp) = local_port {
        response["local_port"] = json!(lp);
    }

    Ok(serde_json::to_string_pretty(&response)?)
}
