use anyhow::Result;
use serde_json::Value;

/// Invoke a tool via a subprocess command.  Captures stdout as the return value.
///
/// `arguments` is the MCP `tools/call` arguments object.  Any `{{param}}`
/// placeholder in `command` or `args` is substituted with the matching value.
///
/// By default the process is spawned **directly** (no shell) so that arguments
/// containing spaces, quotes, JSON, or any other special characters are passed
/// verbatim.  Set `shell = true` to route through `cmd /C` (Windows) or
/// `sh -c` (Unix) when shell features are needed (`.cmd`/`.bat`, pipes, etc.).
pub async fn invoke(
    command: &str,
    args: &[String],
    shell: bool,
    arguments: &Value,
) -> Result<String> {
    let command_sub = substitute(command, arguments);
    let args_sub: Vec<String> = args.iter().map(|a| substitute(a, arguments)).collect();

    // On Windows, .cmd/.bat files cannot be executed without a shell.
    // Auto-promote to shell mode transparently rather than failing.
    #[cfg(windows)]
    let shell = shell || {
        let lower = command_sub.to_lowercase();
        lower.ends_with(".cmd") || lower.ends_with(".bat")
    };

    let output = if shell {
        // Shell mode: join into one string and let the shell parse it.
        let mut parts = vec![shell_quote(&command_sub)];
        parts.extend(args_sub.iter().map(|a| shell_quote(a)));
        let full_cmd = parts.join(" ");

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
    } else {
        // Direct spawn: each argument is passed as a discrete OS string.
        // No shell quoting, no metacharacter interpretation.
        tokio::process::Command::new(&command_sub)
            .args(&args_sub)
            .output()
            .await?
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

/// Wrap a token in double-quotes for shell safety (used only in shell mode).
fn shell_quote(s: &str) -> String {
    if s.contains(' ') || s.contains('"') || s.is_empty() {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}
