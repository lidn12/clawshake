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
//! clawshake network peers|tools|search|ping|call|record ...
//! clawshake schema dump
//! clawshake rpc <method> <params_json>
//! ```

use anyhow::Result;
use clap::{Parser, Subcommand};
use clawshake_bridge::cli::{run_permissions_action, McpArgs, P2pArgs, PermissionsAction};
use clawshake_broker::{builtins, http_server, watcher};
use clawshake_core::{
    mcp_client::{HttpClient, McpClient},
    permissions::PermissionStore,
};
use clawshake_tools::{
    cli::{run_network_cmd, NetworkCmd},
    client, schema,
};
use tracing::info;

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

/// Clawshake — unified P2P MCP node (broker + bridge).
#[derive(Parser, Debug)]
#[command(name = "clawshake", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    // ---- Broker options (default mode only) --------------------------------
    /// HTTP SSE MCP port for the local broker (default 7475).
    /// VS Code config: { "type": "sse", "url": "http://127.0.0.1:<port>/sse" }
    /// Ignored when --mcp-cmd or --mcp-port is set (Track-1 mode).
    #[arg(long, default_value_t = 7475, value_name = "PORT")]
    port: u16,

    // ---- Shared bridge / P2P options --------------------------------------
    #[command(flatten)]
    p2p: P2pArgs,

    // ---- Track-1 overrides ------------------------------------------------
    #[command(flatten)]
    mcp: McpArgs,
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
            run_permissions_action(action, &store).await?;
            return Ok(());
        }

        // Network / rpc — delegate to the IPC socket of a running node.
        Some(Command::Network { cmd }) => {
            run_network_cmd(cmd).await?;
            return Ok(());
        }
        Some(Command::Rpc { method, params }) => {
            let params_value: serde_json::Value = serde_json::from_str(params)
                .map_err(|e| anyhow::anyhow!("params is not valid JSON: {e}"))?;
            let result = client::send_request(method, params_value).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            return Ok(());
        }

        // No subcommand — fall through to node startup below.
        None => {}
    }

    // -----------------------------------------------------------------------
    // Node startup.
    // -----------------------------------------------------------------------

    // Build the MCP backend.
    //
    // Track-1 (--mcp-cmd / --mcp-port): proxy an existing server directly.
    // Default mode: start the local broker and point the bridge at it.
    let (reannounce_tx, reannounce_rx) = tokio::sync::mpsc::channel::<()>(4);
    let backend: Option<McpClient> = if cli.mcp.is_track1() {
        cli.mcp.build("clawshake-bridge").await?
    } else {
        // Default mode: start the local broker and point the bridge at it.
        let manifests_dir = clawshake_dir.join("manifests");
        let permissions = PermissionStore::open(&db_path).await?;

        builtins::seed(&manifests_dir)?;
        let registry = watcher::ManifestRegistry::new();
        let (sse_tx, sse_rx) = tokio::sync::mpsc::channel::<()>(4);
        let servers = watcher::start(
            manifests_dir,
            registry.clone(),
            Some(reannounce_tx.clone()),
            Some(sse_tx),
        )?;
        info!(tools = registry.tool_count(), "Broker ready");

        let broker_port = cli.port;
        tokio::spawn(http_server::serve(
            broker_port,
            registry,
            permissions,
            servers,
            Some(sse_rx),
        ));
        info!("Broker HTTP server starting on :{broker_port}");

        Some(McpClient::Http(HttpClient::new(format!(
            "http://127.0.0.1:{broker_port}"
        ))))
    };

    clawshake_bridge::cli::start_bridge(cli.p2p, backend, &db_path, reannounce_tx, reannounce_rx)
        .await
}
