//! Shell execution tool.
//!
//! Runs a command in the system shell and returns stdout/stderr.
//! Includes safety guards to block catastrophic commands.

use anyhow::{bail, Result};
use clawshake_core::config::ShellConfig;
use serde_json::{json, Value};
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Hard safety ceilings (not overridable by config)
// ---------------------------------------------------------------------------

/// Maximum timeout a caller can request.
const MAX_TIMEOUT_SECS: u64 = 300;

/// Built-in blocked command patterns (always enforced, case-insensitive
/// substring match). User config can add to this list but never remove.
const BUILTIN_BLOCKED_PATTERNS: &[&str] = &[
    "rm -rf /",
    "rm -rf /*",
    "mkfs",
    "dd if=",
    ":(){ :|:& };:",
    "shutdown",
    "reboot",
    "halt",
    "init 0",
    "init 6",
    "> /dev/sd",
    "> /dev/nvme",
    "format c:",
];

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// Returns the MCP tool schema for the `shell` tool.
pub fn tool_definition() -> Value {
    json!({
        "name": "shell",
        "description": "Execute a shell command and return stdout/stderr. Commands run in a non-interactive shell (PowerShell on Windows, sh on Unix). Dangerous commands (rm -rf /, mkfs, shutdown, etc.) are blocked by a safety guard. Output is truncated at 1 MB.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute."
                },
                "workdir": {
                    "type": "string",
                    "description": "Working directory. Defaults to user home."
                },
                "timeout_secs": {
                    "type": "number",
                    "description": "Timeout in seconds (1–300). Default: 30."
                }
            },
            "required": ["command"]
        }
    })
}

// ---------------------------------------------------------------------------
// Public handler
// ---------------------------------------------------------------------------

/// Handle a `shell` tool call.
///
/// # Arguments (from JSON)
///
/// - `command` (required): The command string to execute.
/// - `workdir` (optional): Working directory. Defaults to user home.
/// - `timeout_secs` (optional): Timeout in seconds. Default and max come
///   from `ShellConfig`, hard-capped at `MAX_TIMEOUT_SECS`.
pub async fn handle(arguments: &Value, cfg: &ShellConfig) -> Result<String> {
    let command = arguments
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required field: command"))?;

    let timeout_secs = arguments
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(cfg.default_timeout_secs)
        .min(MAX_TIMEOUT_SECS);

    let workdir = arguments
        .get("workdir")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from);

    // Safety check — built-in patterns + user-configured extras.
    check_blocked(command, &cfg.blocked_patterns)?;

    debug!(command, timeout_secs, ?workdir, "shell: executing");

    // Build the command.
    //
    // On Windows we use PowerShell instead of cmd.exe for two reasons:
    //
    // 1. **Quote fidelity**: cmd.exe /C re-parses its argument with its own
    //    quoting rules and strips the outer double-quotes that tools like
    //    `node -e "..."` rely on.  PowerShell -Command receives the string
    //    verbatim and evaluates it, so quoting is preserved correctly.
    //
    // 2. **Encoding**: cmd.exe uses the system code page (e.g. CP936 on
    //    Chinese Windows), producing garbled output.  We prepend a snippet
    //    that sets the console encoding to UTF-8 before running the caller's
    //    command, giving clean output regardless of locale.
    let mut cmd = if cfg!(windows) {
        let ps_command = format!(
            "$OutputEncoding = [Console]::OutputEncoding = \
             [Console]::InputEncoding = [System.Text.Encoding]::UTF8; {command}"
        );
        let mut c = tokio::process::Command::new("powershell");
        c.args(["-NoProfile", "-NonInteractive", "-Command", &ps_command]);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.args(["-c", command]);
        c
    };

    // Working directory.
    if let Some(ref dir) = workdir {
        cmd.current_dir(dir);
    } else if let Some(home) = dirs::home_dir() {
        cmd.current_dir(home);
    }

    // Capture output with timeout.
    let output = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("command timed out after {timeout_secs}s"))?
        .map_err(|e| anyhow::anyhow!("failed to execute command: {e}"))?;

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = truncate_output(&output.stdout, cfg.max_output_bytes);
    let stderr = truncate_output(&output.stderr, cfg.max_output_bytes);

    debug!(
        exit_code,
        stdout_len = stdout.len(),
        stderr_len = stderr.len(),
        "shell: completed"
    );

    // Build result.
    let mut result = String::new();
    if !stdout.is_empty() {
        result.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !result.is_empty() {
            result.push_str("\n--- stderr ---\n");
        }
        result.push_str(&stderr);
    }
    if result.is_empty() {
        result.push_str("(no output)");
    }
    if exit_code != 0 {
        result.push_str(&format!("\n\n[exit code: {exit_code}]"));
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Safety
// ---------------------------------------------------------------------------

/// Check if a command matches any blocked pattern.
///
/// Checks the built-in deny list first, then any user-configured extras.
fn check_blocked(command: &str, extra_patterns: &[String]) -> Result<()> {
    let lower = command.to_lowercase();
    for pattern in BUILTIN_BLOCKED_PATTERNS {
        if lower.contains(pattern) {
            warn!(command, pattern, "shell: blocked dangerous command");
            bail!(
                "Command blocked by safety guard: matches pattern \"{pattern}\". \
                 If you need to run this command, do so from a terminal directly."
            );
        }
    }
    for pattern in extra_patterns {
        let p = pattern.to_lowercase();
        if lower.contains(&p) {
            warn!(command, %pattern, "shell: blocked by user-configured pattern");
            bail!(
                "Command blocked by safety guard: matches user-configured pattern \"{pattern}\". \
                 Edit [tools.shell].blocked_patterns in config.toml to change."
            );
        }
    }
    Ok(())
}

/// Convert raw output bytes to a UTF-8 string, truncating if necessary.
fn truncate_output(bytes: &[u8], max_bytes: usize) -> String {
    let s = String::from_utf8_lossy(bytes);
    if s.len() > max_bytes {
        let truncated: String = s.chars().take(max_bytes).collect();
        format!("{truncated}\n\n... (output truncated at {max_bytes} bytes)")
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clawshake_core::config::ShellConfig;

    #[test]
    fn blocked_commands() {
        assert!(check_blocked("rm -rf /", &[]).is_err());
        assert!(check_blocked("sudo rm -RF /", &[]).is_err()); // case insensitive
        assert!(check_blocked("mkfs.ext4 /dev/sda1", &[]).is_err());
        assert!(check_blocked("dd if=/dev/zero of=/dev/sda", &[]).is_err());
        assert!(check_blocked("shutdown -h now", &[]).is_err());
    }

    #[test]
    fn allowed_commands() {
        assert!(check_blocked("ls -la", &[]).is_ok());
        assert!(check_blocked("rm -rf ./temp", &[]).is_ok()); // relative path is fine
        assert!(check_blocked("echo hello", &[]).is_ok());
        assert!(check_blocked("cargo build", &[]).is_ok());
        assert!(check_blocked("pip install requests", &[]).is_ok());
    }

    #[test]
    fn user_blocked_patterns() {
        let extras = vec!["deploy --force".to_string(), "DROP TABLE".to_string()];
        assert!(check_blocked("deploy --force staging", &extras).is_err());
        assert!(check_blocked("DROP TABLE users", &extras).is_err());
        assert!(check_blocked("deploy staging", &extras).is_ok());
    }

    #[test]
    fn truncation() {
        let max = ShellConfig::default().max_output_bytes;
        let long = "x".repeat(max + 100);
        let result = truncate_output(long.as_bytes(), max);
        assert!(result.len() < long.len());
        assert!(result.contains("truncated"));
    }

    #[tokio::test]
    async fn basic_execution() {
        let cfg = ShellConfig::default();
        let cmd = if cfg!(windows) {
            "echo hello"
        } else {
            "echo hello"
        };
        let result = handle(&json!({"command": cmd}), &cfg).await.unwrap();
        assert!(result.contains("hello"));
    }

    #[tokio::test]
    async fn missing_command() {
        let cfg = ShellConfig::default();
        let result = handle(&json!({}), &cfg).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn nonzero_exit_code() {
        let cfg = ShellConfig::default();
        // `exit 1` works in both PowerShell and sh.
        let result = handle(&json!({"command": "exit 1"}), &cfg).await.unwrap();
        assert!(result.contains("exit code: 1"));
    }
}
