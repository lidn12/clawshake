use anyhow::Result;
use clap::{Parser, Subcommand};
use clawshake_bridge::cli::{run_permissions_action, McpArgs, P2pArgs, PermissionsAction};
use clawshake_core::permissions::PermissionStore;

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
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "clawshake_bridge=info,libp2p=warn"
                    .parse()
                    .expect("valid tracing filter")
            }),
        )
        .init();

    let cli = Cli::parse();

    let db_path = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
        .join(".clawshake")
        .join("permissions.db");

    // Handle permissions subcommand — no bridge startup required.
    if let Some(Command::Permissions { action }) = &cli.command {
        let store = PermissionStore::open(&db_path).await?;
        run_permissions_action(action, &store).await?;
        return Ok(());
    }

    // Build the MCP backend (if any).
    let backend = cli.mcp.build("clawshake-bridge").await?;

    let (reannounce_tx, reannounce_rx) = tokio::sync::mpsc::channel::<()>(4);

    clawshake_bridge::cli::start_bridge(cli.p2p, backend, &db_path, reannounce_tx, reannounce_rx)
        .await
}
