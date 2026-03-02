//! Shared `network_*` CLI types and dispatch reused by `clawshake-tools` and
//! `clawshake`.
//!
//! Centralises `NetworkCmd` and its IPC dispatch so both binaries expose the
//! same subcommands without duplicating the mapping logic.

use anyhow::Result;
use clap::Subcommand;
use serde_json::json;

use crate::client;

// ---------------------------------------------------------------------------
// NetworkCmd
// ---------------------------------------------------------------------------

#[derive(Subcommand, Debug)]
pub enum NetworkCmd {
    /// List all discovered bridge nodes on the network.
    Peers,

    /// Progressive tool discovery for a remote peer.
    ///
    /// Without --query: returns a compact category summary.
    /// With --query: returns matching tools with full schemas.
    Tools {
        #[arg(long, value_name = "PEER_ID")]
        peer_id: String,
        /// Filter tools by name or description substring. Omit for a summary.
        #[arg(long, value_name = "QUERY")]
        query: Option<String>,
    },

    /// Search for tools across all known peers by name or description
    /// substring.
    Search {
        #[arg(long, value_name = "QUERY")]
        query: String,
    },

    /// Check whether a peer is currently reachable from this node.
    Ping {
        #[arg(long, value_name = "PEER_ID")]
        peer_id: String,
    },

    /// Fetch the raw DHT announcement record for a peer.
    Record {
        #[arg(long, value_name = "PEER_ID")]
        peer_id: String,
    },

    /// Invoke a tool on a remote peer and print the result.
    Call {
        #[arg(long, value_name = "PEER_ID")]
        peer_id: String,
        /// Fully-qualified tool name (e.g. "spotify.play").
        #[arg(long, value_name = "TOOL")]
        tool: String,
        /// Tool arguments as a JSON object string.
        /// Pass "-" to read JSON from stdin (avoids shell quoting issues).
        /// Omit for tools with no required arguments.
        #[arg(long, value_name = "JSON|-")]
        args: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Dispatch a `NetworkCmd` through the bridge IPC socket and print the result.
pub async fn run_network_cmd(cmd: &NetworkCmd) -> Result<()> {
    let (method, params): (&str, serde_json::Value) = match cmd {
        NetworkCmd::Peers => ("network_peers", json!({})),
        NetworkCmd::Tools { peer_id, query } => {
            let mut p = json!({ "peer_id": peer_id });
            if let Some(q) = query {
                p["query"] = json!(q);
            }
            ("network_tools", p)
        }
        NetworkCmd::Search { query } => ("network_search", json!({ "query": query })),
        NetworkCmd::Ping { peer_id } => ("network_ping", json!({ "peer_id": peer_id })),
        NetworkCmd::Record { peer_id } => ("network_record", json!({ "peer_id": peer_id })),
        NetworkCmd::Call {
            peer_id,
            tool,
            args,
        } => {
            let arguments: serde_json::Value = match args.as_deref() {
                Some("-") => {
                    let mut buf = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                        .map_err(|e| anyhow::anyhow!("failed to read stdin: {e}"))?;
                    serde_json::from_str(&buf)
                        .map_err(|e| anyhow::anyhow!("stdin is not valid JSON: {e}"))?
                }
                Some(s) => serde_json::from_str(s)
                    .map_err(|e| anyhow::anyhow!("--args is not valid JSON: {e}"))?,
                None => json!({}),
            };
            (
                "network_call",
                json!({ "peer_id": peer_id, "tool": tool, "arguments": arguments }),
            )
        }
    };

    let result = client::send_request(method, params).await?;

    // If the result contains an "error" key (set by network::err()), propagate
    // it as a proper error so the broker's invoke/cli backend sees a non-zero
    // exit code and marks the MCP response with isError: true.
    if let Some(msg) = result.get("error").and_then(|v| v.as_str()) {
        anyhow::bail!("{msg}");
    }

    // For network_call, the unwrapped result is {"result": "..."} — print
    // just the inner text so the agent sees plain content, not a JSON wrapper.
    if let Some(text) = result.get("result").and_then(|v| v.as_str()) {
        println!("{text}");
    } else {
        println!("{}", serde_json::to_string_pretty(&result)?);
    }
    Ok(())
}
