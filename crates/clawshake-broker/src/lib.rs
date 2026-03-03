//! `clawshake-broker` as a library.
//!
//! Exposes the manifest registry, invoke router, MCP server surface, and
//! HTTP SSE transport so the unified `clawshake` binary can embed them.
//!
//! Use [`start_broker`] for the common startup sequence shared by the
//! standalone `clawshake-broker` binary and the unified `clawshake` binary.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clawshake_core::permissions::PermissionStore;
use tracing::info;

pub mod builtins;
pub mod cli;
pub mod event_queue;
pub mod http_server;
pub mod invoke;
pub mod mcp_server;
pub mod router;
pub mod watcher;

// ---------------------------------------------------------------------------
// Shared broker startup
// ---------------------------------------------------------------------------

/// Configuration for starting the broker.
pub struct BrokerConfig {
    /// Path to `~/.clawshake/manifests/`.
    pub manifests_dir: PathBuf,
    /// Path to `permissions.db`.
    pub db_path: PathBuf,
    /// HTTP port for the broker (0 = stdio mode, no HTTP server).
    pub port: u16,
    /// Enable code mode (`--code-mode`).
    pub code_mode: bool,
    /// Whether the bridge daemon is available for network tools.
    pub bridge_available: bool,
    /// Optional channel to notify when registry changes (for bridge re-announce).
    pub reannounce_tx: Option<tokio::sync::mpsc::Sender<()>>,
}

/// Handle returned by [`start_broker`].
///
/// Holds references that callers may need after startup (e.g. the unified
/// binary connects the bridge to the broker via its HTTP URL).
pub struct BrokerHandle {
    pub registry: watcher::ManifestRegistry,
    pub port: u16,
}

/// Common startup sequence: detect code mode, open permissions, create
/// shim cache / event queue, register builtins, start the manifest watcher,
/// and launch the HTTP server in a background task.
///
/// Returns a [`BrokerHandle`] with the effective port and registry.
pub async fn start_broker(config: BrokerConfig) -> Result<BrokerHandle> {
    let (_has_node, code_mode_active) = cli::detect_code_mode(config.code_mode);

    let permissions = PermissionStore::open(&config.db_path).await?;
    let shim_cache = invoke::codemode::ShimCache::new();
    let event_queue = event_queue::EventQueue::new();
    let registry = watcher::ManifestRegistry::new();

    builtins::register(&registry, code_mode_active, config.bridge_available);

    let (sse_tx, sse_rx) = tokio::sync::mpsc::channel::<()>(4);
    let servers = watcher::start(
        config.manifests_dir,
        registry.clone(),
        config.reannounce_tx,
        Some(sse_tx),
        Some(event_queue.clone()),
    )?;
    info!(tools = registry.tool_count(), "Broker ready");

    let port = config.port;

    if port > 0 {
        let reg = registry.clone();
        tokio::spawn(async move {
            if let Err(e) = http_server::serve(
                port,
                reg,
                permissions,
                servers,
                Some(sse_rx),
                shim_cache,
                config.code_mode,
                event_queue,
            )
            .await
            {
                tracing::error!("Broker HTTP server exited with error: {e:#}");
                std::process::exit(1);
            }
        });
    } else {
        // stdio mode — serve_stdio is blocking, but start_broker returns a
        // handle.  The broker binary will call serve_stdio directly.
        // Drop the SSE receiver since stdio mode doesn't use it.
        drop(sse_rx);

        // We can't run serve_stdio inside start_broker because it blocks the
        // calling task.  Instead we store the components the caller needs to
        // call serve_stdio themselves.  This is handled by the broker binary.
    }

    Ok(BrokerHandle { registry, port })
}

/// Convenience to resolve standard `~/.clawshake` paths.
pub fn resolve_paths(home: &Path) -> (PathBuf, PathBuf) {
    let clawshake_dir = home.join(".clawshake");
    let manifests_dir = clawshake_dir.join("manifests");
    let db_path = clawshake_dir.join("permissions.db");
    (manifests_dir, db_path)
}
