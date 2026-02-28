use anyhow::Result;
use clap::{Parser, Subcommand};
use clawshake_bridge::cli::{run_permissions_action, McpArgs, P2pArgs, PermissionsAction};
use clawshake_core::permissions::PermissionStore;
use serde_json;

/// Clawshake Bridge — expose an existing MCP server to the peer-to-peer network.
#[derive(Parser, Debug)]
#[command(name = "clawshake-bridge", version, about)]
#[command(subcommand_required = true, arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the bridge node.
    ///
    /// Connects to an existing MCP server (via --mcp-cmd or --mcp-port) and
    /// exposes it on the P2P network.
    ///
    /// Examples:
    ///   clawshake-bridge run --mcp-cmd "node server.js"
    ///   clawshake-bridge run --mcp-port 3000
    Run {
        #[command(flatten)]
        p2p: P2pArgs,

        #[command(flatten)]
        mcp: McpArgs,
    },

    /// Show node identity and status.
    ///
    /// Displays the local peer ID and, if a node is running, live stats.
    Status {
        /// Output as JSON instead of a human-readable table.
        #[arg(long, default_value_t = false)]
        json: bool,
    },

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

    match cli.command {
        Command::Permissions { action } => {
            let store = PermissionStore::open(&db_path).await?;
            run_permissions_action(&action, &store).await?;
        }

        Command::Status { json } => {
            let clawshake_dir = db_path.parent().expect("db_path has parent");
            show_status(clawshake_dir, json).await?;
        }

        Command::Run { p2p, mcp } => {
            let backend = mcp.build("clawshake-bridge").await?;
            let (reannounce_tx, reannounce_rx) = tokio::sync::mpsc::channel::<()>(4);
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
// status subcommand
// ---------------------------------------------------------------------------

/// Show the local peer ID and, if a node is running, live stats.
async fn show_status(_clawshake_dir: &std::path::Path, json: bool) -> Result<()> {
    // ---- Peer ID (always available from disk) -----
    let peer_id = match clawshake_bridge::p2p::peer_id_from_disk(None) {
        Ok(id) => Some(id.to_string()),
        Err(_) => None,
    };

    // ---- Probe for a running node via IPC -----
    let live = probe_node().await;

    if json {
        let obj = serde_json::json!({
            "peer_id": peer_id,
            "running": live.is_some(),
            "peers": live.as_ref().map(|l| l.peer_count),
        });
        println!("{}", serde_json::to_string_pretty(&obj)?);
    } else {
        println!(
            "Peer ID:    {}",
            peer_id.as_deref().unwrap_or("(no identity key yet — run the node once to generate)")
        );
        if let Some(stats) = &live {
            println!("Node:       running");
            println!("Peers:      {} connected", stats.peer_count);
        } else {
            println!("Node:       not running");
        }
    }

    Ok(())
}

struct LiveStats {
    peer_count: usize,
}

/// Try to reach the bridge daemon via IPC and return live stats.
async fn probe_node() -> Option<LiveStats> {
    let resp = clawshake_tools::client::send_request("network_peers", serde_json::json!({})).await.ok()?;
    let peers = resp.as_array().map(|a| a.len()).unwrap_or(0);
    Some(LiveStats { peer_count: peers })
}
