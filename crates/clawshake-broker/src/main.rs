use anyhow::Result;
use clap::{Parser, Subcommand};
use clawshake_core::permissions::PermissionStore;
use std::path::PathBuf;
use tracing::info;

mod builtins;
mod cli;
mod http_server;
mod invoke;
mod mcp_server;
mod router;
mod watcher;

use invoke::codemode::ShimCache;

/// Clawshake Broker — manifest watcher and MCP server for local capabilities.
#[derive(Parser, Debug)]
#[command(name = "clawshake-broker", version, about)]
#[command(subcommand_required = true, arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the broker MCP server.
    ///
    /// Without --port, runs as an MCP stdio server (JSON-RPC over stdin/stdout).
    /// With --port, runs as an HTTP SSE MCP server.
    ///
    /// Examples:
    ///   clawshake-broker run                 # stdio mode
    ///   clawshake-broker run --port 7475     # HTTP SSE mode
    Run {
        /// Run as an HTTP SSE MCP server on this port (e.g. --port 7475).
        /// Omit to use stdio mode instead.
        #[arg(long)]
        port: Option<u16>,

        /// Enable code mode: hide individual tools from tools/list and expose
        /// only run_code + describe_tools.  Tools remain callable by name
        /// through run_code scripts.  Requires Node.js on PATH.
        #[arg(long, default_value_t = false)]
        code_mode: bool,
    },

    /// List all tools registered with the broker.
    ///
    /// Shows tool name, published status, and description.
    /// Reads manifests and permissions — no running server needed.
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "clawshake_broker=info,warn"
                    .parse()
                    .expect("valid tracing filter")
            }),
        )
        // MCP stdio: log to stderr so we don't pollute the JSON-RPC channel.
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    // Resolve ~/.clawshake paths.
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    let clawshake_dir = home.join(".clawshake");
    let manifests_dir = clawshake_dir.join("manifests");
    let db_path = clawshake_dir.join("permissions.db");

    match cli.command {
        Command::Tools { action } => match action {
            ToolsAction::List { json } => {
                cli::list_tools(&manifests_dir, &db_path, json).await?;
            }
            ToolsAction::Validate { file } => {
                cli::validate_manifest(&file)?;
            }
            ToolsAction::Add { file } => {
                cli::add_manifest(&file, &manifests_dir)?;
            }
            ToolsAction::Remove { name } => {
                cli::remove_manifest(&name, &manifests_dir)?;
            }
        },

        Command::Run { port, code_mode } => {
            // Detect Node.js on PATH.
            let has_node = which::which("node").is_ok();
            let code_mode_active = if code_mode && !has_node {
                tracing::warn!(
                    "Node.js not found on PATH — code mode unavailable. \
                     Install Node.js 18+ to enable."
                );
                false
            } else if has_node {
                if code_mode {
                    info!("Code mode enabled (Node.js detected)");
                } else {
                    info!(
                        "Node.js detected — run_code and describe_tools available. \
                         Use --code-mode to hide individual tools from tools/list."
                    );
                }
                // Register code mode tools regardless of toggle — the toggle
                // only controls tools/list filtering.
                true
            } else {
                false
            };

            // Seed built-in manifests (network.json, and optionally codemode.json).
            builtins::seed(&manifests_dir, code_mode_active)?;

            // Open permission store.
            let permissions = PermissionStore::open(&db_path).await?;

            // Create shim cache.
            let shim_cache = ShimCache::new();

            // Load manifests and start file watcher.
            let registry = watcher::ManifestRegistry::new();
            let (sse_tx, sse_rx) = tokio::sync::mpsc::channel::<()>(4);
            let servers = watcher::start(manifests_dir, registry.clone(), None, Some(sse_tx))?;
            info!(tools = registry.tool_count(), "Broker ready");

            if let Some(port) = port {
                return http_server::serve(
                    port,
                    registry,
                    permissions,
                    servers,
                    Some(sse_rx),
                    shim_cache,
                    code_mode,
                )
                .await;
            }

            // Default: MCP stdio loop (no SSE sessions — drop the receiver).
            drop(sse_rx);
            mcp_server::serve_stdio(registry, permissions, servers, shim_cache, code_mode_active)
                .await?;
        }
    }

    Ok(())
}
