use anyhow::Result;
use serde_json::Value;

/// Invoke a tool via a subprocess command.  Captures stdout as the return value.
///
/// `arguments` is the MCP `tools/call` arguments object.  Any `{{param}}`
/// placeholder in `command` or `args` is substituted with the matching value.
///
/// The command is routed through the platform shell so `.cmd`/`.bat`/`uvx`/`npx`
/// wrappers resolve correctly — same pattern as `clawshake-bridge --mcp-cmd`.
pub async fn invoke(command: &str, args: &[String], arguments: &Value) -> Result<String> {
    let command_sub = substitute(command, arguments);
    let args_sub: Vec<String> = args.iter().map(|a| substitute(a, arguments)).collect();

    // Build the full shell command string.
    let mut parts = vec![shell_quote(&command_sub)];
    parts.extend(args_sub.iter().map(|a| shell_quote(a)));
    let full_cmd = parts.join(" ");

    let output = {
        #[cfg(windows)]
        {
            tokio::process::Command::new("cmd")
                .args(["/C", &full_cmd])
                .output()
                .await?
        }
        #[cfg(not(windows))]
        {
            tokio::process::Command::new("sh")
                .args(["-c", &full_cmd])
                .output()
                .await?
        }
    };

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("command exited {}: {}", output.status, stderr.trim())
    }
}

/// Replace every `{{key}}` in `template` with the matching value from `args`.
fn substitute(template: &str, args: &Value) -> String {
    let mut result = template.to_string();
    if let Some(obj) = args.as_object() {
        for (key, val) in obj {
            let placeholder = format!("{{{{{key}}}}}");
            let value = match val {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            result = result.replace(&placeholder, &value);
        }
    }
    result
}

/// Wrap a token in double-quotes for shell safety when it contains
/// spaces or shell metacharacters.  Simple heuristic — not a full
/// POSIX quoting library, but sufficient for manifest args.
fn shell_quote(s: &str) -> String {
    if s.contains(' ') || s.contains('"') || s.is_empty() {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}
