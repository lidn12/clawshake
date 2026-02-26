use anyhow::Result;
use clap::Parser;
use clawshake_core::permissions::PermissionStore;
use tracing::info;

mod consent;
mod invoke;
mod mcp_server;
mod router;
mod watcher;

/// Clawshake Broker — manifest watcher and MCP server for local capabilities.
#[derive(Parser, Debug)]
#[command(name = "clawshake-broker", version, about)]
struct Cli {
    /// Run as a stdio MCP server (default when no --port given).
    /// Pass `--port <N>` to listen on a TCP port instead (not yet implemented).
    #[arg(long)]
    port: Option<u16>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "clawshake_broker=info,warn".parse().unwrap()),
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

    // Open permission store.
    let permissions = PermissionStore::open(&db_path).await?;

    // Load manifests and start file watcher.
    let registry = watcher::ManifestRegistry::new();
    watcher::start(manifests_dir, registry.clone())?;
    info!(tools = registry.tool_count(), "Broker ready");

    if let Some(port) = cli.port {
        anyhow::bail!("TCP mode (--port {port}) is not yet implemented; omit --port to use stdio");
    }

    // Default: MCP stdio loop.
    mcp_server::serve_stdio(registry, permissions).await
}
