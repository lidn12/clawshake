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
//! clawshake-tools network call    --peer-id <PEER_ID> --tool <TOOL> [--args <JSON>|-]
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
//! result  = parse(exec("clawshake-tools rpc network_search {\"query\":\"weather\"}"))
//! ```

use anyhow::Result;
use clap::{Parser, Subcommand};
use clawshake_tools::{cli, client, schema};

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
        cmd: cli::NetworkCmd,
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
    ///   clawshake-tools rpc network_peers '{}'
    ///   clawshake-tools rpc network_search '{"query":"weather"}'
    ///   clawshake-tools rpc network_call '{"peer_id":"12D3...","tool":"weather.now","arguments":{}}'
    Rpc {
        /// Method name (e.g. "network_peers", "network_call").
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.namespace {
        Namespace::Schema {
            cmd: SchemaCmd::Dump,
        } => {
            let defs = schema::tool_definitions();
            println!("{}", serde_json::to_string_pretty(&defs)?);
        }
        Namespace::Rpc { method, params } => {
            let params_value: serde_json::Value = serde_json::from_str(&params)
                .map_err(|e| anyhow::anyhow!("params is not valid JSON: {e}"))?;
            let result = client::send_request(&method, params_value).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Namespace::Network { cmd } => {
            cli::run_network_cmd(&cmd).await?;
        }
    }

    Ok(())
}
