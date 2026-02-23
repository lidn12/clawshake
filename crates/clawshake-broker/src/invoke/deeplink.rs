use anyhow::Result;

/// Invoke a tool via a deep-link URL (fire-and-forget).
///
/// ```json
/// "invoke": { "type": "deeplink", "template": "spotify:search:{{query}}" }
/// ```
///
/// Returns `{"status": "invoked"}` — no return value is available.
pub async fn invoke(_url: &str) -> Result<String> {
    // TODO(track-2): open::that(url) and return {"status": "invoked"}
    anyhow::bail!("deeplink invoke not yet implemented")
}
