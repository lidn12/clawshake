use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;

/// Invoke a tool via an HTTP request to a local or remote endpoint.
///
/// `arguments` is sent as the JSON request body for POST/PUT; it is appended
/// as query parameters for GET/DELETE.  Template placeholders (`{{param}}`)
/// in `url` are substituted before the request is sent.
pub async fn invoke(
    url: &str,
    method: Option<&str>,
    headers: &HashMap<String, String>,
    arguments: &Value,
) -> Result<String> {
    let method = method.unwrap_or("POST").to_uppercase();
    let url = super::substitute(url, arguments);

    let client = reqwest::Client::new();
    let mut builder = match method.as_str() {
        "GET" => client.get(&url),
        "DELETE" => client.delete(&url),
        "PUT" => client.put(&url),
        _ => client.post(&url),
    };

    // Attach custom headers.
    for (k, v) in headers {
        builder = builder.header(k.as_str(), v.as_str());
    }

    // Body for POST/PUT; query params for GET/DELETE.
    builder = if method == "GET" || method == "DELETE" {
        if let Some(obj) = arguments.as_object() {
            let pairs: Vec<(String, String)> = obj
                .iter()
                .map(|(k, v)| {
                    let s = match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    (k.clone(), s)
                })
                .collect();
            builder.query(&pairs)
        } else {
            builder
        }
    } else {
        builder
            .header("content-type", "application/json")
            .json(arguments)
    };

    let resp = builder.send().await?;
    let status = resp.status();
    let body = resp.text().await?;

    if status.is_success() {
        Ok(body)
    } else {
        anyhow::bail!("HTTP {status}: {body}")
    }
}
