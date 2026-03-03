//! `clawshake` — unified binary combining the broker and bridge in one command.
//!
//! # Usage
//!
//! ```text
//! clawshake run                      # broker on :7475 + bridge (random P2P port)
//! clawshake run --port 8080 --p2p-port 8081
//! clawshake run --mcp-cmd "node server.js"   # Track-1: proxy existing MCP
//! clawshake run --mcp-port 3000              # Track-1: proxy HTTP MCP
//! ```
//!
//! # Subcommands
//!
//! ```text
//! clawshake run [flags]               # start unified node
//! clawshake status [--json]           # peer ID, running state, stats
//! clawshake permissions allow|deny|remove|list ...
//! clawshake network peers|tools|search|ping|call|record ...
//! clawshake tools list [--json]       # list locally registered tools
//! clawshake tools add <file>          # install a manifest
//! clawshake tools remove <name>       # remove a manifest
//! clawshake tools validate <file>     # validate a manifest
//! ```

use anyhow::Result;
use clap::{Parser, Subcommand};
use clawshake_bridge::cli::{run_permissions_action, McpArgs, P2pArgs, PermissionsAction};
use clawshake_core::{
    mcp_client::{HttpClient, McpClient},
    permissions::PermissionStore,
};
use clawshake_tools::cli::{run_network_cmd, NetworkCmd};
use tracing::info;

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

/// Clawshake — unified P2P MCP node (broker + bridge).
#[derive(Parser, Debug)]
#[command(name = "clawshake", version, about)]
#[command(subcommand_required = true, arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the unified node (broker + P2P bridge).
    ///
    /// Default mode starts the built-in broker on --port (default 7475) and
    /// connects the P2P bridge to it.
    ///
    /// Track-1 mode (--mcp-cmd or --mcp-port) skips the local broker and
    /// proxies an existing MCP server directly.
    Run {
        /// HTTP SSE MCP port for the local broker (default 7475).
        /// Ignored in Track-1 mode.
        #[arg(long, default_value_t = 7475, value_name = "PORT")]
        port: u16,

        /// Expose only run_code + describe_tools instead of individual tools.
        #[arg(long, default_value_t = false)]
        code_mode: bool,

        #[command(flatten)]
        p2p: P2pArgs,

        #[command(flatten)]
        mcp: McpArgs,
    },

    /// Manage the local permission store.
    ///
    /// Examples:
    ///   clawshake permissions allow p2p:* *
    ///   clawshake permissions deny  p2p:* network_call
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

    /// Show node identity and status.
    ///
    /// Displays the local peer ID (always available from the key on disk)
    /// and, if a node is running, live stats such as connected peer count
    /// and registered tool count.
    Status {
        /// Output as JSON instead of a human-readable table.
        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// Manage locally registered tools.
    ///
    /// List, add, remove, or validate tool manifests.  `tools list` queries
    /// the running broker for live tools (including MCP-server-sourced tools)
    /// and falls back to a manifest scan when the broker is not running.
    Tools {
        #[command(subcommand)]
        action: clawshake_broker::cli::ToolsAction,
    },
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
                    .expect("valid tracing filter")
            }),
        )
        .init();

    let cli = Cli::parse();

    // Shared ~/.clawshake paths.
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    let clawshake_dir = home.join(".clawshake");
    let db_path = clawshake_dir.join("permissions.db");

    match cli.command {
        // ---- Offline subcommands ------------------------------------------
        Command::Permissions { action } => {
            let store = PermissionStore::open(&db_path).await?;
            run_permissions_action(&action, &store, &clawshake_dir).await?;
        }

        Command::Network { cmd } => {
            run_network_cmd(&cmd).await?;
        }

        Command::Tools { action } => {
            let manifests_dir = clawshake_dir.join("manifests");
            clawshake_broker::cli::run_tools_action(&action, &manifests_dir, &db_path).await?;
        }

        Command::Status { json } => {
            let manifests_dir = clawshake_dir.join("manifests");
            let (total, published) =
                clawshake_broker::cli::tool_counts(&manifests_dir, &db_path).await;
            clawshake_bridge::cli::show_status(json, Some((total, published))).await?;
        }

        // ---- Node startup -------------------------------------------------
        Command::Run {
            port,
            code_mode,
            p2p,
            mcp,
        } => {
            let (reannounce_tx, reannounce_rx) = tokio::sync::mpsc::channel::<()>(4);
            let backend: Option<McpClient> = if mcp.is_track1() {
                mcp.build("clawshake-bridge").await?
            } else {
                let manifests_dir = clawshake_dir.join("manifests");

                let (_, code_mode_active) = clawshake_broker::cli::detect_code_mode(code_mode);
                let permissions = PermissionStore::open(&db_path).await?;
                let shim_cache = clawshake_broker::invoke::codemode::ShimCache::new();
                let event_queue = clawshake_broker::event_queue::EventQueue::new();
                let registry = clawshake_broker::watcher::ManifestRegistry::new();

                clawshake_broker::builtins::register(&registry, code_mode_active, true);

                let (sse_tx, sse_rx) = tokio::sync::mpsc::channel::<()>(4);
                let servers = clawshake_broker::watcher::start(
                    manifests_dir,
                    registry.clone(),
                    Some(reannounce_tx.clone()),
                    Some(sse_tx),
                    Some(event_queue.clone()),
                )?;
                info!(tools = registry.tool_count(), "Broker ready on :{port}");

                let broker = clawshake_broker::router::BrokerContext {
                    registry,
                    permissions,
                    servers,
                    event_queue,
                    shim_cache,
                    port,
                    code_mode: code_mode_active,
                };

                tokio::spawn(async move {
                    if let Err(e) = clawshake_broker::http_server::serve(
                        broker,
                        Some(sse_rx),
                    )
                    .await
                    {
                        tracing::error!("Broker HTTP server error: {e:#}");
                        std::process::exit(1);
                    }
                });

                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                Some(McpClient::Http(HttpClient::new(format!(
                    "http://127.0.0.1:{port}"
                ))))
            };

            clawshake_bridge::cli::start_bridge(
                p2p,
                backend,
                &db_path,
                reannounce_tx,
                reannounce_rx,
            )
            .await?;
        }
    }

    Ok(())
}
