use anyhow::Result;

/// Invoke a tool via a subprocess command. Captures stdout as the return value.
///
/// ```json
/// "invoke": { "type": "cli", "command": "python", "args": ["script.py", "{{arg}}"] }
/// ```
pub async fn invoke(_command: &str, _args: &[String]) -> Result<String> {
    // TODO(track-2): std::process::Command, capture stdout
    anyhow::bail!("cli invoke not yet implemented")
}
