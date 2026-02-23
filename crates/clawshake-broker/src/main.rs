use anyhow::Result;
use clap::Parser;
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
    /// Port to expose the MCP stdio server on (0 = stdio mode).
    #[arg(long, default_value_t = 7474)]
    port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "clawshake_broker=info".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();
    info!(port = cli.port, "Starting clawshake-broker (Track 2 — not yet implemented)");

    // TODO(track-2): boot manifest watcher, MCP server, permission store, DHT announce
    Ok(())
}
