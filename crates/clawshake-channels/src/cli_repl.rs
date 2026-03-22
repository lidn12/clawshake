//! CLI REPL channel — `clawshake chat`.
//!
//! Reads lines from stdin, emits them as `channel.cli` events, and waits
//! for `channel.cli.response` events from the agent.
//!
//! # Local mode (default)
//!
//! Connects to the broker's HTTP endpoint as a stateless MCP client
//! (`POST /` with JSON-RPC `tools/call`).
//!
//! # Remote mode (`--peer <id>`)
//!
//! Routes through `network_call` via IPC to the local bridge daemon, which
//! relays to the remote peer's broker over P2P.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::debug;

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Run the CLI REPL in local mode, talking directly to the broker HTTP endpoint.
pub async fn run_local(port: u16) -> Result<()> {
    let base_url = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();

    eprintln!("Connected to broker on port {port}");
    eprintln!("Type a message and press Enter. Type \"exit\" to quit.\n");

    // Obtain the initial cursor so we only see responses emitted after now.
    let cursor = local_get_cursor(&client, &base_url).await?;
    repl_loop(
        cursor,
        |emit_args| {
            let client = client.clone();
            let base_url = base_url.clone();
            Box::pin(async move { local_tools_call(&client, &base_url, "emit", emit_args).await })
        },
        |listen_args| {
            let client = client.clone();
            let base_url = base_url.clone();
            Box::pin(
                async move { local_tools_call(&client, &base_url, "listen", listen_args).await },
            )
        },
    )
    .await
}

/// Run the CLI REPL in remote mode, routing through `network_call` to a peer.
pub async fn run_remote(peer_id: &str) -> Result<()> {
    let peer_id = peer_id.to_string();

    eprintln!("Connected to remote peer {peer_id}");
    eprintln!("Type a message and press Enter. Type \"exit\" to quit.\n");

    // Obtain the initial cursor from the remote peer.
    let cursor = remote_get_cursor(&peer_id).await?;
    let peer_emit = peer_id.clone();
    let peer_listen = peer_id.clone();
    repl_loop(
        cursor,
        move |emit_args| {
            let peer = peer_emit.clone();
            Box::pin(async move { remote_tools_call(&peer, "emit", emit_args).await })
        },
        move |listen_args| {
            let peer = peer_listen.clone();
            Box::pin(async move { remote_tools_call(&peer, "listen", listen_args).await })
        },
    )
    .await
}

// ---------------------------------------------------------------------------
// Core REPL loop (transport-agnostic)
// ---------------------------------------------------------------------------

/// Generic REPL loop parameterised by two async closures:
/// - `do_emit(arguments)` — calls the `emit` tool
/// - `do_listen(arguments)` — calls the `listen` tool
///
/// Both return the JSON string result from the tool call.
async fn repl_loop<FE, FL>(mut cursor: u64, do_emit: FE, do_listen: FL) -> Result<()>
where
    FE: Fn(Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send>>
        + Send
        + Sync,
    FL: Fn(Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send>>
        + Send
        + Sync,
{
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();

    loop {
        // Prompt
        stdout.write_all(b"> ").await?;
        stdout.flush().await?;

        let line = match lines.next_line().await? {
            Some(l) => l,
            None => break, // EOF
        };
        let text = line.trim().to_string();
        if text.is_empty() {
            continue;
        }
        if text == "exit" || text == "quit" {
            break;
        }

        // Emit user input.
        let emit_args = json!({
            "topic": crate::TOPIC_CLI,
            "data": {
                "text": text,
            }
        });
        let emit_result = do_emit(emit_args).await;
        if let Err(e) = &emit_result {
            eprintln!("Error sending message: {e:#}");
            continue;
        }
        debug!(result = %emit_result.unwrap(), "emit result");

        // Wait for agent response.
        let listen_args = json!({
            "topics": [crate::TOPIC_CLI_RESPONSE],
            "after": cursor,
            "timeout_secs": 120,
        });

        match do_listen(listen_args).await {
            Ok(result_text) => {
                match serde_json::from_str::<Value>(&result_text) {
                    Ok(parsed) => {
                        // Update cursor.
                        if let Some(c) = parsed.get("cursor").and_then(|v| v.as_u64()) {
                            cursor = c;
                        }

                        // Print responses.
                        let mut found = false;
                        if let Some(events) = parsed.get("events").and_then(|v| v.as_array()) {
                            for event in events {
                                let data = event.get("data").unwrap_or(&Value::Null);
                                if let Some(response_text) =
                                    data.get("text").and_then(|v| v.as_str())
                                {
                                    stdout.write_all(response_text.as_bytes()).await?;
                                    stdout.write_all(b"\n\n").await?;
                                    stdout.flush().await?;
                                    found = true;
                                }
                            }
                        }
                        if !found {
                            eprintln!("(no response within timeout)");
                        }
                    }
                    Err(e) => {
                        eprintln!("Failed to parse listen result: {e}");
                        debug!(raw = %result_text, "unparseable listen result");
                    }
                }
            }
            Err(e) => {
                eprintln!("Error waiting for response: {e:#}");
            }
        }
    }

    eprintln!("Goodbye.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Local transport: direct HTTP to broker
// ---------------------------------------------------------------------------

/// Call a tool via the broker's stateless `POST /` JSON-RPC endpoint.
async fn local_tools_call(
    client: &reqwest::Client,
    base_url: &str,
    tool_name: &str,
    arguments: Value,
) -> Result<String> {
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": tool_name,
            "arguments": arguments,
        }
    });

    let resp = client
        .post(base_url)
        .json(&request)
        .send()
        .await
        .context("Failed to reach broker — is `clawshake run` running?")?;

    let body: Value = resp
        .json()
        .await
        .context("Invalid JSON response from broker")?;

    // Extract text content from the MCP tools/call result.
    extract_tool_result(&body)
}

/// Get the current event cursor from the broker.
async fn local_get_cursor(client: &reqwest::Client, base_url: &str) -> Result<u64> {
    // A very short listen with no matching events returns the current cursor.
    let result = local_tools_call(
        client,
        base_url,
        "listen",
        json!({
            "topics": [crate::TOPIC_CLI_RESPONSE],
            "timeout_secs": 0.001,
        }),
    )
    .await?;

    let parsed: Value = serde_json::from_str(&result).unwrap_or(json!({}));
    Ok(parsed.get("cursor").and_then(|v| v.as_u64()).unwrap_or(0))
}

// ---------------------------------------------------------------------------
// Remote transport: network_call via bridge IPC
// ---------------------------------------------------------------------------

/// Call a tool on a remote peer via `network_call` through the bridge daemon.
async fn remote_tools_call(peer_id: &str, tool_name: &str, arguments: Value) -> Result<String> {
    let params = json!({
        "peer_id": peer_id,
        "tool": tool_name,
        "arguments": arguments,
    });

    let result = clawshake_tools::client::send_request("network_call", params)
        .await
        .map_err(|e| anyhow::anyhow!("bridge IPC error: {e:#}"))?;

    // network_call returns { "result": "..." } on success or { "error": "..." } on failure.
    if let Some(err) = result.get("error").and_then(|v| v.as_str()) {
        anyhow::bail!("Remote call failed: {err}");
    }

    Ok(result
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string())
}

/// Get the current event cursor from a remote peer.
async fn remote_get_cursor(peer_id: &str) -> Result<u64> {
    let result = remote_tools_call(
        peer_id,
        "listen",
        json!({
            "topics": [crate::TOPIC_CLI_RESPONSE],
            "timeout_secs": 0.001,
        }),
    )
    .await?;

    let parsed: Value = serde_json::from_str(&result).unwrap_or(json!({}));
    Ok(parsed.get("cursor").and_then(|v| v.as_u64()).unwrap_or(0))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract text content from a JSON-RPC `tools/call` response.
///
/// Handles the MCP response envelope:
/// ```json
/// { "result": { "content": [{ "type": "text", "text": "..." }], "isError": false } }
/// ```
fn extract_tool_result(body: &Value) -> Result<String> {
    // Check for JSON-RPC error.
    if let Some(err) = body.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        anyhow::bail!("Broker error: {msg}");
    }

    let result = body
        .get("result")
        .ok_or_else(|| anyhow::anyhow!("Missing result in broker response"))?;

    // Check isError flag.
    let is_error = result
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Collect text from content array.
    let text = result
        .get("content")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    if is_error {
        anyhow::bail!("Tool error: {text}");
    }

    Ok(text)
}
