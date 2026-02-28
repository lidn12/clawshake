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
    let command_sub = super::substitute(command, arguments);

    // Substitute then filter: drop empty args and the flag preceding them.
    let raw_args: Vec<String> = args
        .iter()
        .map(|a| super::substitute(a, arguments))
        .collect();
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
            // Unix: sh -c requires a single string.  Each token is
            // single-quoted so the shell cannot interpret metacharacters.
            let mut parts = vec![super::escape_shell(&command_sub)];
            parts.extend(args_sub.iter().map(|a| super::escape_shell(a)));
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
        anyhow::bail!(
            "The underlying command (`{}`) exited with {}. stderr: {}. \
             This may indicate a configuration issue on the node or missing dependencies.",
            command_sub,
            output.status,
            stderr.trim()
        )
    }
}
