//! Shared CLI building blocks reused by `clawshake-bridge` and `clawshake`.
//!
//! Centralises the P2P flags, MCP backend construction, the permissions
//! subcommand, and the bridge startup sequence so both binaries stay in sync
//! without copy-paste.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use clap::{Args, Subcommand};
use clawshake_core::{
    mcp_client::{HttpClient, McpClient, StdioClient},
    network_channel::{new_connected_peers, new_outbound_call_channel},
    peer_table::PeerTable,
    permissions::{Decision, PermissionStore},
};
use tokio::sync::mpsc;
use tracing::info;

// ---------------------------------------------------------------------------
// P2P flags
// ---------------------------------------------------------------------------

/// P2P networking flags shared between `clawshake-bridge` and `clawshake`.
/// Embed with `#[command(flatten)]`.
#[derive(Args, Debug)]
pub struct P2pArgs {
    /// TCP port for inbound P2P connections (0 = random; relay-server mode
    /// defaults to 7474).
    #[arg(long, default_value_t = 0)]
    pub p2p_port: u16,

    /// Bootstrap peer multiaddr(s) to dial on startup.
    /// Can be specified multiple times.
    #[arg(long = "boot", value_name = "MULTIADDR")]
    pub boot_peers: Vec<String>,

    /// Skip the hardcoded default bootstrap peers.
    /// Useful for running isolated test networks on a LAN.
    #[arg(long, default_value_t = false)]
    pub no_default_boot: bool,

    /// Enable relay-server mode: this node will forward traffic between peers
    /// behind NAT, use a stable port (default 7474), and print a copy-ready
    /// multiaddr banner on startup.
    #[arg(long, default_value_t = false)]
    pub relay_server: bool,

    /// Path to the Ed25519 keypair file (default ~/.clawshake/identity.key).
    /// Useful for running multiple nodes on the same machine during testing.
    #[arg(long, value_name = "PATH")]
    pub identity: Option<std::path::PathBuf>,
}

// ---------------------------------------------------------------------------
// MCP backend flags
// ---------------------------------------------------------------------------

/// MCP backend source (Track-1 flags). Embed with `#[command(flatten)]`.
#[derive(Args, Debug)]
pub struct McpArgs {
    /// Proxy an MCP server listening on this HTTP port (e.g. --mcp-port 3000).
    /// Mutually exclusive with --mcp-cmd.
    #[arg(long, value_name = "PORT", conflicts_with = "mcp_cmd")]
    pub mcp_port: Option<u16>,

    /// Proxy an MCP server launched with this stdio command
    /// (e.g. --mcp-cmd "node server.js").
    /// Mutually exclusive with --mcp-port.
    #[arg(long, value_name = "COMMAND", conflicts_with = "mcp_port")]
    pub mcp_cmd: Option<String>,
}

impl McpArgs {
    /// Returns `true` if either `--mcp-cmd` or `--mcp-port` was supplied.
    pub fn is_track1(&self) -> bool {
        self.mcp_cmd.is_some() || self.mcp_port.is_some()
    }

    /// Build an `McpClient` from the provided flags.
    /// Returns `None` (with a log line) if neither flag was provided.
    pub async fn build(&self, label: &str) -> Result<Option<McpClient>> {
        if let Some(cmd) = &self.mcp_cmd {
            info!("MCP backend: stdio — {cmd}");
            let c = StdioClient::spawn(cmd, &[], label).await?;
            Ok(Some(McpClient::Stdio(c)))
        } else if let Some(port) = self.mcp_port {
            info!("MCP backend: HTTP — http://127.0.0.1:{port}");
            Ok(Some(McpClient::Http(HttpClient::new(format!(
                "http://127.0.0.1:{port}"
            )))))
        } else {
            info!("No MCP backend configured — running in discovery-only mode");
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// Permissions subcommand
// ---------------------------------------------------------------------------

#[derive(Subcommand, Debug)]
pub enum PermissionsAction {
    /// Allow an agent to call a tool (or wildcard).
    Allow {
        /// Agent ID: "p2p:*", "p2p:<peer-id>", "tailscale:*", "local"
        agent_id: String,
        /// Tool name: "*" for all tools, a substring to match (e.g. "network"
        /// expands to every tool whose name contains "network"), or an exact
        /// name like "read_file".  The expansion is read from the local
        /// registry snapshot; individual rules are written to the DB.
        tool_name: String,
    },
    /// Deny an agent from calling a tool (or wildcard).
    Deny {
        /// Agent ID: "p2p:*", "p2p:<peer-id>", "tailscale:*", "local"
        agent_id: String,
        /// Tool name: "*" for all tools, a substring to match (e.g. "network"
        /// expands to every tool whose name contains "network"), or an exact
        /// name like "read_file".  The expansion is read from the local
        /// registry snapshot; individual rules are written to the DB.
        tool_name: String,
    },
    /// Remove a permission rule entirely (falls back to default behaviour).
    Remove {
        /// Agent ID to remove the rule for.
        agent_id: String,
        /// Tool name to remove. Supports the same substring expansion as
        /// allow/deny, or "*" to remove all rules for this agent.
        tool_name: String,
    },
    /// List all permission rules.
    List,
}

/// Resolve `pattern` to a list of tool names to act on.
///
/// - `"*"` → `["*"]` (global wildcard, passed through to the DB as-is)
/// - anything else → case-insensitive substring match against the local
///   registry snapshot (`~/.clawshake/registry.json`).
///
/// If the registry snapshot doesn't exist yet (broker never ran), falls back
/// to `[pattern]` so the rule isn't silently dropped, and prints a notice.
fn resolve_tool_names(pattern: &str, clawshake_dir: &Path) -> Vec<String> {
    if pattern == "*" {
        return vec!["*".to_string()];
    }

    let snapshot_path = clawshake_dir.join("registry.json");
    let content = match std::fs::read_to_string(&snapshot_path) {
        Ok(c) => c,
        Err(_) => {
            println!(
                "notice: registry snapshot not found — broker has not run yet. \
                 Writing literal rule '{}' which will only match that exact name.",
                pattern
            );
            return vec![pattern.to_string()];
        }
    };

    let val: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return vec![pattern.to_string()],
    };

    let pattern_lower = pattern.to_lowercase();
    let matched: Vec<String> = val
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t["name"].as_str())
                .filter(|name| name.to_lowercase().contains(&pattern_lower))
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();

    if matched.is_empty() {
        println!(
            "notice: no registered tools match '{}' — no rules written.",
            pattern
        );
    }

    matched
}

/// Execute a permissions subcommand against the given store and print the result.
///
/// `clawshake_dir` is `~/.clawshake` — used to locate the registry snapshot
/// for substring pattern expansion.
pub async fn run_permissions_action(
    action: &PermissionsAction,
    store: &PermissionStore,
    clawshake_dir: &Path,
) -> Result<()> {
    match action {
        PermissionsAction::Allow {
            agent_id,
            tool_name,
        } => {
            let names = resolve_tool_names(tool_name, clawshake_dir);
            for name in &names {
                store.set(agent_id, name, Decision::Allow).await?;
            }
            if names.len() == 1 {
                println!("✓ allow  {agent_id}  {}", names[0]);
            } else {
                println!("✓ allow  {agent_id}  {} rules:", names.len());
                for name in &names {
                    println!("    {name}");
                }
            }
        }
        PermissionsAction::Deny {
            agent_id,
            tool_name,
        } => {
            let names = resolve_tool_names(tool_name, clawshake_dir);
            for name in &names {
                store.set(agent_id, name, Decision::Deny).await?;
            }
            if names.len() == 1 {
                println!("✓ deny   {agent_id}  {}", names[0]);
            } else {
                println!("✓ deny   {agent_id}  {} rules:", names.len());
                for name in &names {
                    println!("    {name}");
                }
            }
        }
        PermissionsAction::Remove {
            agent_id,
            tool_name,
        } => {
            let names = resolve_tool_names(tool_name, clawshake_dir);
            for name in &names {
                store.remove(agent_id, name).await?;
            }
            if names.len() == 1 {
                println!("✓ removed  {agent_id}  {}", names[0]);
            } else {
                println!("✓ removed  {agent_id}  {} rules:", names.len());
                for name in &names {
                    println!("    {name}");
                }
            }
        }
        PermissionsAction::List => {
            let records = store.list().await?;
            if records.is_empty() {
                println!("(no rules)");
            } else {
                println!("{:<12}  {:<40}  {}", "decision", "agent_id", "tool_name");
                println!("{}", "-".repeat(72));
                for r in records {
                    println!("{:<12}  {:<40}  {}", r.decision, r.agent_id, r.tool_name);
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Bridge startup
// ---------------------------------------------------------------------------

/// Run the bridge node: open PermissionStore, watch for permission changes,
/// spawn IPC, and start the P2P swarm.
///
/// Both `clawshake-bridge` and the unified `clawshake` binary call this after
/// building the MCP backend.  The caller creates the `reannounce` channel so
/// it can clone `tx` for other watchers (e.g. the broker manifest watcher)
/// before handing it in here.
pub async fn start_bridge(
    p2p_args: P2pArgs,
    backend: Option<McpClient>,
    db_path: &Path,
    reannounce_tx: mpsc::Sender<()>,
    reannounce_rx: mpsc::Receiver<()>,
) -> Result<()> {
    let p2p_port = if p2p_args.relay_server && p2p_args.p2p_port == 0 {
        crate::p2p::RELAY_DEFAULT_PORT
    } else {
        p2p_args.p2p_port
    };

    // Open the permission store (creates DB + schema if absent).
    let store = PermissionStore::open(db_path).await?;
    store.seed_p2p_deny_default().await?;
    let store = Arc::new(store);

    // Watch permissions.db so DHT re-announces when permissions change.
    crate::watcher::watch_permissions_db(db_path, reannounce_tx);

    // Peer table and connected-peer tracker for network_* built-in tools.
    let table = Arc::new(PeerTable::new());
    let connected = new_connected_peers();

    // Outbound P2P call channel: the IPC task drives network_call from any
    // local process; the p2p event loop owns the receiver.
    let (call_tx, call_rx) = new_outbound_call_channel();

    // Spawn the IPC socket listener so clawshake-tools CLI (and any other
    // local process) can reach network_* handlers.
    tokio::spawn(clawshake_tools::ipc::run(
        Arc::clone(&table),
        connected.clone(),
        call_tx,
    ));

    crate::p2p::run(
        p2p_port,
        p2p_args.boot_peers,
        p2p_args.identity,
        backend,
        store,
        table,
        connected,
        p2p_args.no_default_boot,
        p2p_args.relay_server,
        call_rx,
        Some(reannounce_rx),
    )
    .await
}

// ---------------------------------------------------------------------------
// Status subcommand
// ---------------------------------------------------------------------------

/// Live stats obtained by probing the bridge IPC socket.
pub struct LiveStats {
    pub peer_count: usize,
}

/// Try to reach the bridge daemon via IPC and return live stats.
///
/// Returns `None` if the socket is unreachable (node not running).
pub async fn probe_node() -> Option<LiveStats> {
    let resp = clawshake_tools::client::send_request("network_peers", serde_json::json!({}))
        .await
        .ok()?;
    let peers = resp.as_array().map(|a| a.len()).unwrap_or(0);
    Some(LiveStats { peer_count: peers })
}

/// Show the local peer ID and, if a node is running, live stats.
///
/// `tool_info` is an optional `(total, published)` pair — the unified binary
/// passes manifest/permission counts here; the bridge-only binary passes
/// `None`.
pub async fn show_status(json: bool, tool_info: Option<(usize, usize)>) -> Result<()> {
    // ---- Peer ID (always available from disk) -----
    let peer_id = match crate::p2p::peer_id_from_disk(None) {
        Ok(id) => Some(id.to_string()),
        Err(_) => None,
    };

    // ---- Probe for a running node via IPC -----
    let live = probe_node().await;

    if json {
        let mut obj = serde_json::json!({
            "peer_id": peer_id,
            "running": live.is_some(),
            "peers": live.as_ref().map(|l| l.peer_count),
        });
        if let Some((total, published)) = tool_info {
            obj["tools"] = serde_json::json!(total);
            obj["published"] = serde_json::json!(published);
        }
        println!("{}", serde_json::to_string_pretty(&obj)?);
    } else {
        println!(
            "Peer ID:    {}",
            peer_id
                .as_deref()
                .unwrap_or("(no identity key yet — run the node once to generate)")
        );
        if let Some(stats) = &live {
            println!("Node:       running");
            println!("Peers:      {} connected", stats.peer_count);
        } else {
            println!("Node:       not running");
        }
        if let Some((total, published)) = tool_info {
            println!("Tools:      {} registered ({} published)", total, published);
        }
    }

    Ok(())
}
