use anyhow::Result;
use serde_json::Value;

/// Invoke a tool via a deep-link URL (fire-and-forget via `open::that`).
///
/// `{{param}}` placeholders in `template` are substituted from `arguments`.
/// Returns `{"status":"invoked"}` — no return value is available.
pub async fn invoke(template: &str, arguments: &Value) -> Result<String> {
    let url = substitute(template, arguments);
    // `open::that` is synchronous; run it on the blocking thread pool.
    tokio::task::spawn_blocking(move || {
        open::that(&url)?;
        Ok::<(), anyhow::Error>(())
    })
    .await??;
    Ok(r#"{"status":"invoked"}"#.to_string())
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
