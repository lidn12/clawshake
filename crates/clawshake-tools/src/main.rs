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
//!
//! # Generic interface (language-agnostic integration)
//!
//! clawshake-tools schema dump
//! clawshake-tools rpc <METHOD> <PARAMS_JSON>
//! ```
//!
//! Output is always a single JSON value printed to stdout, making it trivial
//! for any MCP host (Python, Node, Ruby, Go, …) to shell out to these
//! commands and parse the result.
//!
//! ## Integrating in any language
//!
//! ```text
//! schemas = parse(exec("clawshake-tools schema dump"))   # → register tools
//! result  = parse(exec("clawshake-tools rpc network.search {\"query\":\"weather\"}"))
//! ```

use anyhow::Result;
use clap::{Parser, Subcommand};
use clawshake_tools::{client, schema};
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "clawshake-tools",
    version,
    about = "Clawshake network tool CLI"
)]
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

    /// Tool schema utilities (no bridge connection required).
    Schema {
        #[command(subcommand)]
        cmd: SchemaCmd,
    },

    /// Generic RPC — send any method+params to the bridge and print the result.
    ///
    /// This is the preferred integration point for non-Rust MCP hosts.
    /// Use `schema dump` to discover available methods and their input schemas.
    ///
    /// Example:
    ///   clawshake-tools rpc network.peers '{}'
    ///   clawshake-tools rpc network.search '{"query":"weather"}'
    ///   clawshake-tools rpc network.call '{"peer_id":"12D3...","tool":"weather.now","arguments":{}}'
    Rpc {
        /// Method name (e.g. "network.peers", "network.call").
        method: String,
        /// Parameters as a JSON object string.  Use '{}' for tools with no parameters.
        params: String,
    },
}

#[derive(Subcommand)]
enum SchemaCmd {
    /// Print the MCP tool schemas for all network.* tools as a JSON array.
    ///
    /// Pipe this into your MCP host's list_tools handler so schemas stay in
    /// sync automatically — no manual copying required.
    ///
    /// Example (Python):
    ///   schemas = json.loads(subprocess.check_output(["clawshake-tools", "schema", "dump"]))
    Dump,
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

    // `schema dump` is purely local — no bridge connection needed.
    if let Namespace::Schema {
        cmd: SchemaCmd::Dump,
    } = &cli.namespace
    {
        let defs = schema::tool_definitions();
        println!("{}", serde_json::to_string_pretty(&defs)?);
        return Ok(());
    }

    // All other namespaces go through the bridge IPC socket.
    let (method, params): (&str, serde_json::Value) = match cli.namespace {
        Namespace::Schema { .. } => unreachable!(),

        // Generic RPC — caller supplies method + raw JSON params.
        Namespace::Rpc { method, params } => {
            let params_value: serde_json::Value = serde_json::from_str(&params)
                .map_err(|e| anyhow::anyhow!("params is not valid JSON: {e}"))?;
            // method is owned so we need to do the send separately.
            let result = client::send_request(&method, params_value).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            return Ok(());
        }

        Namespace::Network { cmd } => match cmd {
            NetworkCmd::Peers => ("network.peers", json!({})),

            NetworkCmd::Tools { peer_id } => ("network.tools", json!({ "peer_id": peer_id })),

            NetworkCmd::Search { query } => ("network.search", json!({ "query": query })),

            NetworkCmd::Describe { peer_id, tool_name } => (
                "network.describe",
                json!({ "peer_id": peer_id, "tool_name": tool_name }),
            ),

            NetworkCmd::Ping { peer_id } => ("network.ping", json!({ "peer_id": peer_id })),

            NetworkCmd::Call {
                peer_id,
                tool,
                args,
            } => {
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
