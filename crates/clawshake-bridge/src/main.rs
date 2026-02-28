use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use clawshake_bridge::cli::{run_permissions_action, McpArgs, P2pArgs, PermissionsAction};
use clawshake_core::{
    network_channel::{new_connected_peers, new_outbound_call_channel},
    peer_table::PeerTable,
    permissions::PermissionStore,
};

/// Clawshake Bridge — expose an existing MCP server to the peer-to-peer network.
#[derive(Parser, Debug)]
#[command(name = "clawshake-bridge", version, about)]
struct Cli {
    /// Manage the local permission store (no bridge connection required).
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    p2p: P2pArgs,

    #[command(flatten)]
    mcp: McpArgs,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Manage the local permission store.
    ///
    /// Examples:
    ///   clawshake-bridge permissions allow p2p:* *
    ///   clawshake-bridge permissions allow p2p:12D3KooW... filesystem.*
    ///   clawshake-bridge permissions deny  p2p:* mail.*
    ///   clawshake-bridge permissions remove p2p:* *
    ///   clawshake-bridge permissions list
    Permissions {
        #[command(subcommand)]
        action: PermissionsAction,
    },
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

    let db_path = dirs::home_dir()
        .expect("cannot determine home directory")
        .join(".clawshake")
        .join("permissions.db");

    // Handle permissions subcommand — no bridge startup required.
    if let Some(Command::Permissions { action }) = &cli.command {
        let store = PermissionStore::open(&db_path).await?;
        run_permissions_action(action, &store).await?;
        return Ok(());
    }

    // --- Normal bridge startup ---

    // Relay server mode: use a stable port so the address is predictable.
    let p2p_port = if cli.p2p.relay_server && cli.p2p.p2p_port == 0 {
        clawshake_bridge::p2p::RELAY_DEFAULT_PORT
    } else {
        cli.p2p.p2p_port
    };

    // Build the MCP backend (if any).
    let backend = cli.mcp.build("clawshake-bridge").await?;

    // Open the permission store (creates DB + schema if absent, seeds p2p deny default).
    let store = PermissionStore::open(&db_path).await?;
    store.seed_p2p_deny_default().await?;
    let store = Arc::new(store);

    // Watch permissions.db so DHT re-announces when permissions change.
    let (reannounce_tx, reannounce_rx) = tokio::sync::mpsc::channel::<()>(4);
    clawshake_bridge::watch::watch_permissions_db(&db_path, reannounce_tx);

    // Peer table and connected-peer tracker for the network.* built-in tools.
    let table = Arc::new(PeerTable::new());
    let connected = new_connected_peers();

    // Outbound P2P call channel: the IPC task drives network_call from any
    // local process; the p2p event loop owns the receiver.
    let (call_tx, call_rx) = new_outbound_call_channel();

    // Spawn the IPC socket listener so clawshake-tools CLI (and any other
    // local process) can reach network.* handlers without in-process channels.
    tokio::spawn(clawshake_tools::ipc::run(
        Arc::clone(&table),
        connected.clone(),
        call_tx,
    ));

    clawshake_bridge::p2p::run(
        p2p_port,
        cli.p2p.boot_peers,
        cli.p2p.identity,
        backend,
        store,
        table,
        connected,
        cli.p2p.no_default_boot,
        cli.p2p.relay_server,
        call_rx,
        Some(reannounce_rx),
    )
    .await
}
