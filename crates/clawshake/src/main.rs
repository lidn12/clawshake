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
use clawshake_broker::{builtins, http_server, watcher};
use clawshake_core::{
    mcp_client::{HttpClient, McpClient},
    permissions::PermissionStore,
};
use clawshake_tools::cli::{run_network_cmd, NetworkCmd};
use std::path::PathBuf;
use tracing::{info, warn};

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
        action: ToolsAction,
    },
}

#[derive(Subcommand, Debug)]
enum ToolsAction {
    /// List all registered tools.
    List {
        /// Output as JSON instead of a human-readable table.
        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// Validate a manifest file without installing it.
    Validate {
        /// Path to the manifest JSON file.
        file: PathBuf,
    },

    /// Install a manifest file into the manifests directory.
    Add {
        /// Path to the manifest JSON file to install.
        file: PathBuf,
    },

    /// Remove an installed manifest by name.
    Remove {
        /// Manifest name (e.g. "calendar", not "calendar.json").
        name: String,
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
            match action {
                ToolsAction::List { json } => {
                    builtins::seed(&manifests_dir)?;
                    clawshake_broker::cli::list_tools(&manifests_dir, &db_path, json).await?;
                }
                ToolsAction::Validate { file } => {
                    clawshake_broker::cli::validate_manifest(&file)?;
                }
                ToolsAction::Add { file } => {
                    clawshake_broker::cli::add_manifest(&file, &manifests_dir)?;
                }
                ToolsAction::Remove { name } => {
                    clawshake_broker::cli::remove_manifest(&name, &manifests_dir)?;
                }
            }
        }

        Command::Status { json } => {
            let manifests_dir = clawshake_dir.join("manifests");
            builtins::seed(&manifests_dir)?;
            let (total, published) =
                clawshake_broker::cli::tool_counts(&manifests_dir, &db_path).await;
            clawshake_bridge::cli::show_status(json, Some((total, published))).await?;
        }

        // ---- Node startup -------------------------------------------------
        Command::Run { port, p2p, mcp } => {
            // Check that clawshake-tools is available on PATH.
            check_tools_binary();

            let (reannounce_tx, reannounce_rx) = tokio::sync::mpsc::channel::<()>(4);
            let backend: Option<McpClient> = if mcp.is_track1() {
                mcp.build("clawshake-bridge").await?
            } else {
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

                let broker_port = port;
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

// ---------------------------------------------------------------------------
// Startup checks
// ---------------------------------------------------------------------------

/// Warn if `clawshake-tools` is not found on PATH.
fn check_tools_binary() {
    let name = if cfg!(windows) {
        "clawshake-tools.exe"
    } else {
        "clawshake-tools"
    };
    if which::which(name).is_err() {
        warn!(
            "clawshake-tools not found on PATH; network_* tools will not work. \
             Install it alongside clawshake or add it to your PATH."
        );
    }
}
