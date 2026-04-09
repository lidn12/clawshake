use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use clawshake_broker::{builtins, cli, http_server, invoke, mcp_server, router, watcher};
use clawshake_core::config as core_config;
use clawshake_core::permissions::PermissionStore;
use tracing::info;

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

        /// Enable code mode: hide individual tools from tools/list and expose
        /// only run_code + describe_tools.  Tools remain callable by name
        /// through run_code scripts.  Requires Node.js on PATH.
        #[arg(long, default_value_t = false)]
        code_mode: bool,

        /// Skip network tool registration even if the bridge daemon is running.
        /// Useful for purely local operation.
        #[arg(long, default_value_t = false)]
        local: bool,
    },

    /// List all tools registered with the broker.
    ///
    /// Shows tool name, published status, and description.
    /// Reads manifests and permissions — no running server needed.
    Tools {
        #[command(subcommand)]
        action: cli::ToolsAction,
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
    let clawshake_dir = clawshake_core::config::config_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    let manifests_dir = clawshake_core::config::manifests_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    let db_path = clawshake_core::config::permissions_db_path()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;

    match cli.command {
        Command::Tools { action } => {
            cli::run_tools_action(&action, &manifests_dir, &db_path).await?;
        }

        Command::Run {
            port,
            code_mode,
            local,
        } => {
            // ── Load config early so tools.code_mode is available ────
            let config = core_config::load(None)?;

            // Detect bridge daemon (unless --local).
            let bridge_available = if local {
                info!("--local flag set, skipping network tools");
                false
            } else {
                builtins::detect_bridge().await
            };

            let (_, code_mode_active) = cli::detect_code_mode(code_mode || config.tools.code_mode);
            let permissions = PermissionStore::open(&db_path).await?;
            let shim_cache = clawshake_broker::invoke::codemode::ShimCache::new();
            let event_queue = clawshake_broker::event_queue::EventQueue::new();
            let registry = watcher::ManifestRegistry::new();

            builtins::register(&registry, code_mode_active, bridge_available);

            let (sse_tx, sse_rx) = tokio::sync::mpsc::channel::<()>(4);
            let servers = watcher::start(
                manifests_dir,
                registry.clone(),
                None,
                Some(sse_tx),
                Some(event_queue.clone()),
            )?;
            info!(tools = registry.tool_count(), "Broker ready");

            // ── Config + Memory subsystem ───────────────────────────────
            let config = Arc::new(config);
            #[cfg(feature = "memory")]
            let memory = if config.memory.enabled {
                let mem_ctx = invoke::memory::build_memory_context(&clawshake_dir, &config.memory);
                invoke::memory::start_watcher(&clawshake_dir, &config.memory);
                Some(mem_ctx)
            } else {
                info!("Memory subsystem disabled");
                None
            };
            #[cfg(not(feature = "memory"))]
            let memory: Option<router::MemoryContext> = None;

            // stdio mode has no HTTP port for /invoke callbacks.
            let effective_port = port.unwrap_or(0);
            let broker = router::BrokerContext {
                config,
                registry,
                permissions,
                servers,
                event_queue,
                shim_cache,
                cron: invoke::cron::CronScheduler::new(),
                port: effective_port,
                code_mode: code_mode_active,
                memory,
                frame_store: clawshake_broker::webview::FrameStore::new(),
                expose_table: clawshake_broker::expose::ExposeTable::new(),
                reannounce_tx: None,
            };

            if port.is_some() {
                // Spawn the persistent window server process (connects back
                // via WS once the HTTP server is ready).
                clawshake_broker::webview::spawn_window_server(effective_port);
                http_server::serve(broker, Some(sse_rx)).await?;
            } else {
                // In stdio mode there is no HTTP notification loop, so
                // drain the channel here to invalidate the JS shim cache
                // whenever manifests change.
                let sc = broker.shim_cache.clone();
                tokio::spawn(async move {
                    let mut rx = sse_rx;
                    while rx.recv().await.is_some() {
                        sc.invalidate();
                    }
                });
                mcp_server::serve_stdio(broker).await?;
            }
        }
    }

    Ok(())
}
