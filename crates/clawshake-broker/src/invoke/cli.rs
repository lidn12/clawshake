use anyhow::Result;
use serde_json::Value;

/// Invoke a tool via a subprocess command.  Captures stdout as the return value.
///
/// `arguments` is the MCP `tools/call` arguments object.  Any `{{param}}`
/// placeholder in `command` or `args` is substituted with the matching value.
/// Unresolved placeholders (no matching key in arguments) are replaced with an
/// empty string.  Args that become empty after substitution — and any preceding
/// flag (`--foo`) whose value arg is empty — are dropped automatically, making
/// optional parameters work without special manifest logic.
///
/// By default the process is spawned **directly** (no shell) so arguments are
/// passed verbatim.  Set `shell = true` to route through `cmd /C` (Windows) or
/// `sh -c` (Unix) when shell features like `.cmd`/`.bat` or pipes are needed.
pub async fn invoke(
    command: &str,
    args: &[String],
    shell: bool,
    arguments: &Value,
) -> Result<String> {
    let command_sub = substitute(command, arguments);

    // Substitute then filter: drop empty args and the flag preceding them.
    let raw_args: Vec<String> = args.iter().map(|a| substitute(a, arguments)).collect();
    let mut args_sub: Vec<String> = Vec::new();
    let mut i = 0;
    while i < raw_args.len() {
        let arg = &raw_args[i];
        // Flag followed by an empty value → skip both.
        if arg.starts_with('-') && i + 1 < raw_args.len() && raw_args[i + 1].is_empty() {
            i += 2;
            continue;
        }
        if !arg.is_empty() {
            args_sub.push(arg.clone());
        }
        i += 1;
    }

    // On Windows, .cmd/.bat files cannot be executed without a shell.
    // Auto-promote to shell mode transparently rather than failing.
    #[cfg(windows)]
    let shell = shell || {
        let lower = command_sub.to_lowercase();
        lower.ends_with(".cmd") || lower.ends_with(".bat")
    };

    let output = if shell {
        #[cfg(windows)]
        {
            // Pass command and each argument as separate tokens to cmd /C.
            // This avoids all join-and-quote complexity while still letting
            // cmd.exe resolve .cmd/.bat and PATH lookups.
            tokio::process::Command::new("cmd")
                .arg("/C")
                .arg(&command_sub)
                .args(&args_sub)
                .output()
                .await?
        }
        #[cfg(not(windows))]
        {
            // Unix: sh -c requires a single string.
            let mut parts = vec![shell_quote(&command_sub)];
            parts.extend(args_sub.iter().map(|a| shell_quote(a)));
            tokio::process::Command::new("sh")
                .args(["-c", &parts.join(" ")])
                .output()
                .await?
        }
    } else {
        // Direct spawn: each argument is a discrete OS string.
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
/// Any placeholders that remain unresolved are stripped, along with any
/// immediately preceding separator character (`:` or `=`).
/// e.g. `"path:{{line}}"` with no `line` arg → `"path"`
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
    // Strip remaining unresolved placeholders (with optional preceding separator).
    loop {
        let Some(start) = result.find("{{") else {
            break;
        };
        let prefix_start = if start > 0 && matches!(result.as_bytes()[start - 1], b':' | b'=') {
            start - 1
        } else {
            start
        };
        let Some(rel_end) = result[start..].find("}}") else {
            break;
        };
        result.drain(prefix_start..start + rel_end + 2);
    }
    result
}

/// Wrap a token in double-quotes for shell safety (used only in Unix shell mode).
#[cfg(not(windows))]
fn shell_quote(s: &str) -> String {
    if s.contains(' ') || s.contains('"') || s.is_empty() {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}
