use anyhow::Result;
use std::collections::HashMap;

/// Invoke a tool via an HTTP request to a local or remote endpoint.
///
/// ```json
/// "invoke": { "type": "http", "url": "http://localhost:1234/endpoint",
///             "method": "POST", "headers": { "Authorization": "Bearer ..." } }
/// ```
pub async fn invoke(
    _url: &str,
    _method: Option<&str>,
    _headers: &HashMap<String, String>,
    _body: serde_json::Value,
) -> Result<String> {
    // TODO(track-2): reqwest HTTP call, return response body as string
    anyhow::bail!("http invoke not yet implemented")
}
