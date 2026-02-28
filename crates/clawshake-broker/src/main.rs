use anyhow::Result;
use clap::{Parser, Subcommand};
use clawshake_core::permissions::PermissionStore;
use tracing::info;

mod builtins;
mod cli;
mod http_server;
mod invoke;
mod mcp_server;
mod router;
mod watcher;

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
    },

    /// List all tools registered with the broker.
    ///
    /// Shows tool name, published status, and description.
    /// Reads manifests and permissions — no running server needed.
    Tools {
        /// Output as JSON instead of a human-readable table.
        #[arg(long, default_value_t = false)]
        json: bool,
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
        Command::Tools { json } => {
            cli::list_tools(&manifests_dir, &db_path, json).await?;
        }

        Command::Run { port } => {
            // Open permission store.
            let permissions = PermissionStore::open(&db_path).await?;

            // Seed built-in manifests (no-op if files already exist).
            builtins::seed(&manifests_dir)?;

            // Load manifests and start file watcher.
            let registry = watcher::ManifestRegistry::new();
            let (sse_tx, sse_rx) = tokio::sync::mpsc::channel::<()>(4);
            let servers = watcher::start(manifests_dir, registry.clone(), None, Some(sse_tx))?;
            info!(tools = registry.tool_count(), "Broker ready");

            if let Some(port) = port {
                return http_server::serve(port, registry, permissions, servers, Some(sse_rx))
                    .await;
            }

            // Default: MCP stdio loop (no SSE sessions — drop the receiver).
            drop(sse_rx);
            mcp_server::serve_stdio(registry, permissions, servers).await?;
        }
    }

    Ok(())
}
