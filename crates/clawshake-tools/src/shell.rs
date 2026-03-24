//! Shell execution tool.
//!
//! Runs a command in the system shell and returns stdout/stderr.
//! Includes safety guards to block catastrophic commands.

use anyhow::{bail, Result};
use serde_json::Value;
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Configuration defaults
// ---------------------------------------------------------------------------

/// Default timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Maximum timeout a caller can request.
const MAX_TIMEOUT_SECS: u64 = 300;

/// Maximum output size in bytes before truncation.
const MAX_OUTPUT_BYTES: usize = 1_048_576; // 1 MB

/// Blocked command patterns (case-insensitive substring match).
const BLOCKED_PATTERNS: &[&str] = &[
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
// Public handler
// ---------------------------------------------------------------------------

/// Handle a `shell` tool call.
///
/// # Arguments (from JSON)
///
/// - `command` (required): The command string to execute.
/// - `workdir` (optional): Working directory. Defaults to user home.
/// - `timeout_secs` (optional): Timeout in seconds. Default 30, max 300.
pub async fn handle(arguments: &Value) -> Result<String> {
    let command = arguments
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required field: command"))?;

    let timeout_secs = arguments
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .min(MAX_TIMEOUT_SECS);

    let workdir = arguments
        .get("workdir")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from);

    // Safety check.
    check_blocked(command)?;

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
    let stdout = truncate_output(&output.stdout);
    let stderr = truncate_output(&output.stderr);

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
fn check_blocked(command: &str) -> Result<()> {
    let lower = command.to_lowercase();
    for pattern in BLOCKED_PATTERNS {
        if lower.contains(pattern) {
            warn!(command, pattern, "shell: blocked dangerous command");
            bail!(
                "Command blocked by safety guard: matches pattern \"{pattern}\". \
                 If you need to run this command, do so from a terminal directly."
            );
        }
    }
    Ok(())
}

/// Convert raw output bytes to a UTF-8 string, truncating if necessary.
fn truncate_output(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    if s.len() > MAX_OUTPUT_BYTES {
        let truncated: String = s.chars().take(MAX_OUTPUT_BYTES).collect();
        format!("{truncated}\n\n... (output truncated at {MAX_OUTPUT_BYTES} bytes)")
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
    use serde_json::json;

    #[test]
    fn blocked_commands() {
        assert!(check_blocked("rm -rf /").is_err());
        assert!(check_blocked("sudo rm -RF /").is_err()); // case insensitive
        assert!(check_blocked("mkfs.ext4 /dev/sda1").is_err());
        assert!(check_blocked("dd if=/dev/zero of=/dev/sda").is_err());
        assert!(check_blocked("shutdown -h now").is_err());
    }

    #[test]
    fn allowed_commands() {
        assert!(check_blocked("ls -la").is_ok());
        assert!(check_blocked("rm -rf ./temp").is_ok()); // relative path is fine
        assert!(check_blocked("echo hello").is_ok());
        assert!(check_blocked("cargo build").is_ok());
        assert!(check_blocked("pip install requests").is_ok());
    }

    #[test]
    fn truncation() {
        let long = "x".repeat(MAX_OUTPUT_BYTES + 100);
        let result = truncate_output(long.as_bytes());
        assert!(result.len() < long.len());
        assert!(result.contains("truncated"));
    }

    #[tokio::test]
    async fn basic_execution() {
        let cmd = if cfg!(windows) {
            "echo hello"
        } else {
            "echo hello"
        };
        let result = handle(&json!({"command": cmd})).await.unwrap();
        assert!(result.contains("hello"));
    }

    #[tokio::test]
    async fn missing_command() {
        let result = handle(&json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn nonzero_exit_code() {
        // `exit 1` works in both PowerShell and sh.
        let result = handle(&json!({"command": "exit 1"})).await.unwrap();
        assert!(result.contains("exit code: 1"));
    }
}
