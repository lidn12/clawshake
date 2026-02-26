use anyhow::Result;
use serde_json::Value;

/// Invoke a PowerShell script on Windows (or PowerShell Core cross-platform).
///
/// `{{param}}` placeholders in `script` are substituted from `arguments`.
/// Stdout is returned as the result.
pub async fn invoke_powershell(script: &str, arguments: &Value) -> Result<String> {
    let script_sub = substitute(script, arguments);
    let output = tokio::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script_sub])
        .output()
        .await?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim_end().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("powershell exited {}: {}", output.status, stderr.trim())
    }
}

/// Invoke an AppleScript on macOS via `osascript`.
///
/// `{{param}}` placeholders in `script` are substituted from `arguments`.
/// Stdout is returned as the result.
///
/// On non-macOS platforms this always returns an error.
pub async fn invoke_applescript(script: &str, arguments: &Value) -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        let script_sub = substitute(script, arguments);
        let output = tokio::process::Command::new("osascript")
            .args(["-e", &script_sub])
            .output()
            .await?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim_end().to_string())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("osascript exited {}: {}", output.status, stderr.trim())
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (script, arguments);
        anyhow::bail!("AppleScript is only supported on macOS")
    }
}

fn substitute(template: &str, arguments: &Value) -> String {
    let mut result = template.to_string();
    if let Some(obj) = arguments.as_object() {
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
