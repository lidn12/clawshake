use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use clawshake_core::{peer_table::PeerTable, permissions::PermissionStore};
use tracing::info;

mod announce;
mod backend;
mod network;
mod p2p;
mod proxy;

use backend::{HttpBackend, McpBackend, StdioBackend};

/// Clawshake Bridge — expose an existing MCP server to the peer-to-peer network.
#[derive(Parser, Debug)]
#[command(name = "clawshake-bridge", version, about)]
struct Cli {
    /// TCP port to listen on for inbound P2P connections (0 = random).
    #[arg(long, default_value_t = 0)]
    p2p_port: u16,

    /// Bootstrap peer multiaddr(s) to dial on startup.
    /// Format: /ip4/<addr>/tcp/<port>/p2p/<peer-id>
    /// Can be specified multiple times.
    #[arg(long = "boot", value_name = "MULTIADDR")]
    boot_peers: Vec<String>,

    /// Skip the hardcoded default bootstrap peers.
    /// Useful for running isolated test networks on a LAN.
    #[arg(long, default_value_t = false)]
    no_default_boot: bool,

    /// Run as a bootstrap node: no MCP backend required, stable port (default
    /// 7474 unless --p2p-port is set), and prints a copy-ready multiaddr
    /// banner on startup for distribution to other users.
    /// Also implies --relay-server.
    #[arg(long, default_value_t = false)]
    bootstrap_mode: bool,

    /// Enable relay server mode: this node will forward traffic between peers
    /// behind NAT.  Only effective if this node has a public IP address.
    /// Implied by --bootstrap-mode.
    #[arg(long, default_value_t = false)]
    relay_server: bool,

    /// Path to the Ed25519 keypair file. Defaults to ~/.clawshake/identity.key.
    /// Useful for running multiple nodes on the same machine during testing.
    #[arg(long, value_name = "PATH")]
    identity: Option<std::path::PathBuf>,

    /// Proxy an MCP server listening on this HTTP port (e.g. --mcp-port 3000).
    /// Mutually exclusive with --mcp-cmd.
    #[arg(long, value_name = "PORT", conflicts_with = "mcp_cmd")]
    mcp_port: Option<u16>,

    /// Proxy an MCP server launched with this stdio command (e.g. --mcp-cmd "node server.js").
    /// Mutually exclusive with --mcp-port.
    #[arg(long, value_name = "COMMAND", conflicts_with = "mcp_port")]
    mcp_cmd: Option<String>,
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

    // Bootstrap mode: use a stable port so the address is predictable.
    let p2p_port = if cli.bootstrap_mode && cli.p2p_port == 0 {
        p2p::BOOTSTRAP_DEFAULT_PORT
    } else {
        cli.p2p_port
    };
    if cli.bootstrap_mode {
        info!("Bootstrap mode — listening on port {p2p_port} with no MCP backend");
    }

    // Build the MCP backend (if any).
    let backend: Option<McpBackend> = if let Some(cmd) = &cli.mcp_cmd {
        info!("MCP backend: stdio — {cmd}");
        let b = StdioBackend::spawn(cmd).await?;
        Some(McpBackend::Stdio(b))
    } else if let Some(port) = cli.mcp_port {
        info!("MCP backend: HTTP — http://127.0.0.1:{port}");
        Some(McpBackend::Http(HttpBackend::new(port)))
    } else {
        info!("No MCP backend configured — running in discovery-only mode");
        None
    };

    // Open the permission store (creates DB + schema if absent, seeds p2p deny default).
    let db_path = dirs::home_dir()
        .expect("cannot determine home directory")
        .join(".clawshake")
        .join("permissions.db");
    let store = PermissionStore::open(&db_path).await?;
    store.seed_p2p_deny_default().await?;
    let store = Arc::new(store);

    // Peer table and connected-peer tracker for the network.* built-in tools.
    let table = Arc::new(PeerTable::new());
    let connected = network::new_connected_peers();

    p2p::run(
        p2p_port,
        cli.boot_peers,
        cli.identity,
        backend,
        store,
        table,
        connected,
        cli.no_default_boot,
        cli.bootstrap_mode,
        cli.bootstrap_mode || cli.relay_server,
    )
    .await
}
