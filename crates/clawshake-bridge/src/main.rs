use anyhow::Result;
use clap::Parser;
use tracing::info;

mod announce;
mod p2p;
mod proxy;

/// Clawshake Bridge — expose an existing MCP server to the peer-to-peer network.
#[derive(Parser, Debug)]
#[command(name = "clawshake-bridge", version, about)]
struct Cli {
    /// TCP port the local MCP server is listening on.
    #[arg(long, default_value_t = 3000)]
    mcp_port: u16,

    /// TCP port to listen on for inbound P2P connections (0 = random).
    #[arg(long, default_value_t = 0)]
    p2p_port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "clawshake_bridge=info,libp2p=warn".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();

    info!(
        mcp_port = cli.mcp_port,
        p2p_port = cli.p2p_port,
        "Starting clawshake-bridge"
    );

    p2p::run(cli.p2p_port).await
}
