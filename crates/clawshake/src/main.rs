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
//! clawshake permissions allow|deny|remove|list ...
//! clawshake network peers|tools|search|ping|call|record ...
//! clawshake tools [--json]            # list locally registered tools
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

    /// List all tools registered with the local broker.
    ///
    /// Shows tool name, published status (whether the tool is advertised on
    /// the P2P network), and description.  Requires reading the manifests
    /// directory and permissions database — no running node needed.
    Tools {
        /// Output as JSON instead of a human-readable table.
        #[arg(long, default_value_t = false)]
        json: bool,
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
            run_permissions_action(&action, &store).await?;
        }

        Command::Network { cmd } => {
            run_network_cmd(&cmd).await?;
        }

        Command::Tools { json } => {
            let manifests_dir = clawshake_dir.join("manifests");
            list_tools(&manifests_dir, &db_path, json).await?;
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
// tools subcommand
// ---------------------------------------------------------------------------

/// Load manifests and permissions, then print a table of all registered tools
/// with their published status.
async fn list_tools(
    manifests_dir: &std::path::Path,
    db_path: &std::path::Path,
    json: bool,
) -> Result<()> {
    // Seed built-in manifests so network.* always appears.
    builtins::seed(manifests_dir)?;

    // Load all manifests exactly the way the broker does.
    let registry = watcher::ManifestRegistry::new();
    watcher::load_manifests_from_dir(manifests_dir, &registry)?;

    let permissions = PermissionStore::open(db_path).await?;

    let tools = registry.all();

    if json {
        let entries: Vec<serde_json::Value> = {
            let mut out = Vec::new();
            for t in &tools {
                let published = permissions.is_network_exposed(&t.tool.name).await;
                out.push(serde_json::json!({
                    "name": t.tool.name,
                    "description": t.tool.description,
                    "source": t.source,
                    "published": published,
                }));
            }
            out
        };
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        if tools.is_empty() {
            println!("No tools registered.");
            println!(
                "Add manifests to {}",
                manifests_dir.display()
            );
            return Ok(());
        }

        println!(
            "{:<30}  {:<5}  {}",
            "Name", "Pub", "Description"
        );
        println!("{}", "-".repeat(78));
        for t in &tools {
            let published = permissions.is_network_exposed(&t.tool.name).await;
            let marker = if published { "  ✓" } else { "  ✗" };
            let desc = truncate(&t.tool.description, 40);
            println!("{:<30}  {:<5}  {}", t.tool.name, marker, desc);
        }
    }

    Ok(())
}

/// Truncate a string to `max` chars, appending "…" if truncated.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max - 1])
    }
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
            "clawshake-tools not found on PATH; network.* tools will not work. \
             Install it alongside clawshake or add it to your PATH."
        );
    }
}
