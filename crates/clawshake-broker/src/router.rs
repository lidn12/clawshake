use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;

/// Dispatch a tool call to the right invoke handler based on the manifest's
/// `InvokeConfig` type.
///
/// Implemented in Track 2.
pub async fn dispatch(
    _tool_name: &str,
    _arguments: &HashMap<String, Value>,
) -> Result<String> {
    // TODO(track-2): match on InvokeConfig type and delegate to the
    // appropriate invoke module (cli, http, deeplink, script)
    anyhow::bail!("broker invoke router not yet implemented")
}
