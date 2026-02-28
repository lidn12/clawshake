//! `clawshake` — unified binary combining the broker and bridge in one command.
//!
//! # Default mode (no `--mcp-cmd` / `--mcp-port`)
//!
//! Starts the built-in broker HTTP server on `--port` (default 7475) and
//! connects the P2P bridge to it, so the local MCP registry is announced to
//! the network.
//!
//! ```text
//! clawshake                          # broker on :7475 + bridge on :7474
//! clawshake --port 8080 --p2p-port 8081
//! ```
//!
//! # Track-1 mode (`--mcp-cmd` or `--mcp-port`)
//!
//! Skips the local broker and proxies an existing MCP server directly, exactly
//! like running `clawshake-bridge` standalone.
//!
//! ```text
//! clawshake --mcp-cmd "node server.js"
//! clawshake --mcp-port 3000
//! ```
//!
//! # Subcommands
//!
//! ```text
//! clawshake permissions allow|deny|remove|list ...
//! clawshake network peers|tools|search|describe|ping|call|record ...
//! clawshake schema dump
//! clawshake rpc <method> <params_json>
//! ```

use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use clawshake_bridge::p2p;
use clawshake_core::mcp_client::{HttpClient, McpClient, StdioClient};
use clawshake_broker::{builtins, http_server, watcher};
use clawshake_core::{
    network_channel::{new_connected_peers, new_outbound_call_channel},
    peer_table::PeerTable,
    permissions::{Decision, PermissionStore},
};
use clawshake_tools::{client, schema};
use serde_json::json;
use tracing::info;

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

/// Clawshake — unified P2P MCP node (broker + bridge).
#[derive(Parser, Debug)]
#[command(name = "clawshake", version, about)]
struct Cli {
    /// Subcommand (permissions, network, schema, rpc). Omit to start the node.
    #[command(subcommand)]
    command: Option<Command>,

    // ---- Broker options (default mode only) --------------------------------
    /// HTTP SSE MCP port for the local broker (default 7475).
    /// VS Code config: { "type": "sse", "url": "http://127.0.0.1:<port>/sse" }
    /// Ignored when --mcp-cmd or --mcp-port is set (Track-1 mode).
    #[arg(long, default_value_t = 7475, value_name = "PORT")]
    port: u16,

    // ---- Bridge / P2P options ---------------------------------------------
    /// TCP port for inbound P2P connections (0 = random; default 7474 in
    /// relay-server mode).
    #[arg(long, default_value_t = 0)]
    p2p_port: u16,

    /// Bootstrap peer multiaddr(s) to dial on startup.
    /// Can be specified multiple times.
    #[arg(long = "boot", value_name = "MULTIADDR")]
    boot_peers: Vec<String>,

    /// Skip the hardcoded default bootstrap peers.
    #[arg(long, default_value_t = false)]
    no_default_boot: bool,

    /// Enable relay-server mode (stable port, forwards NAT'd peers).
    #[arg(long, default_value_t = false)]
    relay_server: bool,

    /// Path to the Ed25519 keypair file (default ~/.clawshake/identity.key).
    #[arg(long, value_name = "PATH")]
    identity: Option<std::path::PathBuf>,

    // ---- Track-1 overrides ------------------------------------------------
    /// [Track-1] Proxy an MCP server listening on this HTTP port.
    /// Skips the local broker entirely.
    #[arg(long, value_name = "PORT", conflicts_with = "mcp_cmd")]
    mcp_port: Option<u16>,

    /// [Track-1] Proxy an MCP server launched with this stdio command.
    /// Skips the local broker entirely.
    #[arg(long, value_name = "COMMAND", conflicts_with = "mcp_port")]
    mcp_cmd: Option<String>,
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

#[derive(Subcommand, Debug)]
enum Command {
    /// Manage the local permission store.
    ///
    /// Examples:
    ///   clawshake permissions allow p2p:* *
    ///   clawshake permissions deny  p2p:* mail.*
    ///   clawshake permissions remove p2p:* *
    ///   clawshake permissions list
    Permissions {
        #[command(subcommand)]
        action: PermissionsAction,
    },

    /// P2P network discovery and invocation tools.
    ///
    /// Requires a running clawshake node (or clawshake-bridge daemon).
    Network {
        #[command(subcommand)]
        cmd: NetworkCmd,
    },

    /// Tool schema utilities (no node connection required).
    Schema {
        #[command(subcommand)]
        cmd: SchemaCmd,
    },

    /// Generic RPC — send any method + params to the running node.
    ///
    /// Examples:
    ///   clawshake rpc network_peers '{}'
    ///   clawshake rpc network_search '{"query":"weather"}'
    Rpc { method: String, params: String },
}

// ---- permissions -----------------------------------------------------------

#[derive(Subcommand, Debug)]
enum PermissionsAction {
    Allow { agent_id: String, tool_name: String },
    Deny { agent_id: String, tool_name: String },
    Remove { agent_id: String, tool_name: String },
    List,
}

// ---- network ---------------------------------------------------------------

#[derive(Subcommand, Debug)]
enum NetworkCmd {
    /// List all discovered bridge nodes on the network.
    Peers,

    /// Get the full tool listing (names, descriptions, inputSchemas) for a specific peer.
    Tools {
        #[arg(long, value_name = "PEER_ID")]
        peer_id: String,
    },

    /// Search for tools across all known peers by name or description substring.
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
        #[arg(long, value_name = "TOOL")]
        tool: String,
        /// Arguments as a JSON object string. Pass "-" to read from stdin.
        #[arg(long, value_name = "JSON|-")]
        args: Option<String>,
    },
}

// ---- schema ----------------------------------------------------------------

#[derive(Subcommand, Debug)]
enum SchemaCmd {
    /// Print MCP tool schemas for all network.* tools as a JSON array.
    Dump,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "clawshake=info,clawshake_bridge=info,clawshake_broker=info,libp2p=warn"
                    .parse()
                    .unwrap()
            }),
        )
        .init();

    let cli = Cli::parse();

    // Shared ~/.clawshake paths.
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    let clawshake_dir = home.join(".clawshake");
    let db_path = clawshake_dir.join("permissions.db");

    // -----------------------------------------------------------------------
    // Handle offline / non-node subcommands first.
    // -----------------------------------------------------------------------

    match &cli.command {
        // schema dump — purely local, no node required.
        Some(Command::Schema {
            cmd: SchemaCmd::Dump,
        }) => {
            let defs = schema::tool_definitions();
            println!("{}", serde_json::to_string_pretty(&defs)?);
            return Ok(());
        }

        // permissions — read/write the local DB, no node required.
        Some(Command::Permissions { action }) => {
            let store = PermissionStore::open(&db_path).await?;
            match action {
                PermissionsAction::Allow {
                    agent_id,
                    tool_name,
                } => {
                    store.set(agent_id, tool_name, Decision::Allow).await?;
                    println!("✓ allow  {agent_id}  {tool_name}");
                }
                PermissionsAction::Deny {
                    agent_id,
                    tool_name,
                } => {
                    store.set(agent_id, tool_name, Decision::Deny).await?;
                    println!("✓ deny   {agent_id}  {tool_name}");
                }
                PermissionsAction::Remove {
                    agent_id,
                    tool_name,
                } => {
                    store.remove(agent_id, tool_name).await?;
                    println!("✓ removed  {agent_id}  {tool_name}");
                }
                PermissionsAction::List => {
                    let records = store.list().await?;
                    if records.is_empty() {
                        println!("(no rules)");
                    } else {
                        println!("{:<12}  {:<40}  {}", "decision", "agent_id", "tool_name");
                        println!("{}", "-".repeat(72));
                        for r in records {
                            println!("{:<12}  {:<40}  {}", r.decision, r.agent_id, r.tool_name);
                        }
                    }
                }
            }
            return Ok(());
        }

        // Network / rpc — delegate to the IPC socket of a running node.
        Some(Command::Network { .. } | Command::Rpc { .. }) => {
            run_ipc_command(cli.command.as_ref().unwrap()).await?;
            return Ok(());
        }

        // No subcommand — fall through to node startup below.
        None => {}
    }

    // -----------------------------------------------------------------------
    // Node startup.
    // -----------------------------------------------------------------------

    let p2p_port = if cli.relay_server && cli.p2p_port == 0 {
        p2p::RELAY_DEFAULT_PORT
    } else {
        cli.p2p_port
    };

    // Build the MCP backend.
    let backend: Option<McpClient> = if let Some(cmd) = &cli.mcp_cmd {
        // Track-1 mode: proxy a stdio MCP server.
        info!("MCP backend: stdio — {cmd}");
        let b = StdioClient::spawn(cmd, &[], "clawshake-bridge").await?;
        Some(McpClient::Stdio(b))
    } else if let Some(port) = cli.mcp_port {
        // Track-1 mode: proxy an existing HTTP MCP server.
        info!("MCP backend: HTTP — http://127.0.0.1:{port}");
        Some(McpClient::Http(HttpClient::new(format!("http://127.0.0.1:{port}"))))
    } else {
        // Default mode: start the local broker and point the bridge at it.
        let manifests_dir = clawshake_dir.join("manifests");
        let permissions = PermissionStore::open(&db_path).await?;

        builtins::seed(&manifests_dir)?;
        let registry = watcher::ManifestRegistry::new();
        let servers = watcher::start(manifests_dir, registry.clone())?;
        info!(tools = registry.tool_count(), "Broker ready");

        let broker_port = cli.port;
        tokio::spawn(http_server::serve(broker_port, registry, permissions, servers));
        info!("Broker HTTP server starting on :{broker_port}");

        Some(McpClient::Http(HttpClient::new(format!("http://127.0.0.1:{broker_port}"))))
    };

    // Open the permission store for the bridge (p2p inbound gate).
    let store = PermissionStore::open(&db_path).await?;
    store.seed_p2p_deny_default().await?;
    let store = Arc::new(store);

    let table = Arc::new(PeerTable::new());
    let connected = new_connected_peers();
    let (call_tx, call_rx) = new_outbound_call_channel();

    tokio::spawn(clawshake_tools::ipc::run(
        Arc::clone(&table),
        connected.clone(),
        call_tx,
    ));

    p2p::run(
        p2p_port,
        cli.boot_peers,
        cli.identity,
        backend,
        store,
        table,
        connected,
        cli.no_default_boot,
        cli.relay_server,
        call_rx,
    )
    .await
}

// ---------------------------------------------------------------------------
// IPC helper — used by `network` and `rpc` subcommands.
// ---------------------------------------------------------------------------

async fn run_ipc_command(cmd: &Command) -> Result<()> {
    let (method, params): (&str, serde_json::Value) = match cmd {
        Command::Rpc { method, params } => {
            let params_value: serde_json::Value = serde_json::from_str(params)
                .map_err(|e| anyhow::anyhow!("params is not valid JSON: {e}"))?;
            let result = client::send_request(method, params_value).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            return Ok(());
        }

        Command::Network { cmd } => match cmd {
            NetworkCmd::Peers => ("network_peers", json!({})),
            NetworkCmd::Tools { peer_id } => ("network_tools", json!({ "peer_id": peer_id })),
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
        },

        _ => unreachable!("run_ipc_command called with non-IPC command"),
    };

    let result = client::send_request(method, params).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
