use anyhow::Result;
use serde_json::Value;

/// Invoke a tool via a deep-link URL (fire-and-forget via `open::that`).
///
/// `{{param}}` placeholders in `template` are substituted from `arguments`.
/// Returns `{"status":"invoked"}` — no return value is available.
pub async fn invoke(template: &str, arguments: &Value) -> Result<String> {
    let url = super::substitute(template, arguments);
    // `open::that` is synchronous; run it on the blocking thread pool.
    tokio::task::spawn_blocking(move || {
        open::that(&url)?;
        Ok::<(), anyhow::Error>(())
    })
    .await??;
    Ok(r#"{"status":"invoked"}"#.to_string())
}
