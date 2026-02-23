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

    /// Bootstrap peer multiaddr(s) to dial on startup.
    /// Format: /ip4/<addr>/tcp/<port>/p2p/<peer-id>
    /// Can be specified multiple times.
    #[arg(long = "boot", value_name = "MULTIADDR")]
    boot_peers: Vec<String>,

    /// Path to the Ed25519 keypair file. Defaults to ~/.clawshake/identity.key.
    /// Useful for running multiple nodes on the same machine during testing.
    #[arg(long, value_name = "PATH")]
    identity: Option<std::path::PathBuf>,
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

    p2p::run(cli.p2p_port, cli.boot_peers, cli.identity).await
}
