//! `clawshake-tools` CLI binary.
//!
//! Exposes all `network.*` capabilities as subcommands.  Each invocation
//! connects to the running `clawshake-bridge` daemon, executes one request,
//! prints the JSON result to stdout, and exits.
//!
//! # Usage
//!
//! ```text
//! clawshake-tools network peers
//! clawshake-tools network tools  --peer-id <PEER_ID>
//! clawshake-tools network search --query <QUERY>
//! clawshake-tools network describe --peer-id <PEER_ID> --tool-name <TOOL>
//! clawshake-tools network ping    --peer-id <PEER_ID>
//! clawshake-tools network call    --peer-id <PEER_ID> --tool <TOOL> [--args <JSON>]
//! ```
//!
//! Output is always a single JSON object printed to stdout, making it trivial
//! for any MCP host (Python, Node, Rust, Claude Desktop) to shell out to these
//! commands and parse the result.

use anyhow::Result;
use clap::{Parser, Subcommand};
use clawshake_tools::client;
use serde_json::json;

#[derive(Parser)]
#[command(name = "clawshake-tools", version, about = "Clawshake network tool CLI")]
struct Cli {
    #[command(subcommand)]
    namespace: Namespace,
}

#[derive(Subcommand)]
enum Namespace {
    /// P2P network discovery and invocation tools.
    Network {
        #[command(subcommand)]
        cmd: NetworkCmd,
    },
}

#[derive(Subcommand)]
enum NetworkCmd {
    /// List all discovered bridge nodes on the network.
    Peers,

    /// List all tools exposed by a specific peer.
    Tools {
        #[arg(long, value_name = "PEER_ID")]
        peer_id: String,
    },

    /// Search for tools across all known peers by name or description substring.
    Search {
        #[arg(long, value_name = "QUERY")]
        query: String,
    },

    /// Get the description and input schema for a specific tool on a peer.
    Describe {
        #[arg(long, value_name = "PEER_ID")]
        peer_id: String,
        #[arg(long, value_name = "TOOL_NAME")]
        tool_name: String,
    },

    /// Check whether a peer is currently connected to this node.
    Ping {
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
        /// Tool arguments as a JSON object string (e.g. '{"track":"Bohemian Rhapsody"}').
        /// Omit for tools with no required arguments.
        #[arg(long, value_name = "JSON")]
        args: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let (method, params) = match cli.namespace {
        Namespace::Network { cmd } => match cmd {
            NetworkCmd::Peers => ("network.peers", json!({})),

            NetworkCmd::Tools { peer_id } => ("network.tools", json!({ "peer_id": peer_id })),

            NetworkCmd::Search { query } => ("network.search", json!({ "query": query })),

            NetworkCmd::Describe { peer_id, tool_name } => (
                "network.describe",
                json!({ "peer_id": peer_id, "tool_name": tool_name }),
            ),

            NetworkCmd::Ping { peer_id } => ("network.ping", json!({ "peer_id": peer_id })),

            NetworkCmd::Call { peer_id, tool, args } => {
                let arguments: serde_json::Value = match args {
                    Some(s) => serde_json::from_str(&s)
                        .map_err(|e| anyhow::anyhow!("--args is not valid JSON: {e}"))?,
                    None => json!({}),
                };
                (
                    "network.call",
                    json!({ "peer_id": peer_id, "tool": tool, "arguments": arguments }),
                )
            }
        },
    };

    let result = client::send_request(method, params).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
