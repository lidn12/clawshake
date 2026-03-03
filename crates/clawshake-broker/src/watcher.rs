use anyhow::Result;
use clawshake_core::manifest::{InputSchema, InvokeConfig, Manifest, McpSource, Tool};
use notify::{Event, RecursiveMode, Watcher};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};
use tracing::{info, warn};

use crate::event_queue::EventQueue;
use clawshake_core::mcp_client::McpClient;

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// A single tool entry as loaded from a manifest, tagged with its source.
///
/// The `source` is derived from the manifest file stem (e.g. `"spotify"`
/// for `spotify.json`).
#[derive(Debug, Clone)]
pub struct LoadedTool {
    pub tool: Tool,
    pub source: String,
}

/// Thread-safe map of tool name → loaded tool.
/// e.g. `"spotify_play"` → `LoadedTool { tool: ..., source: "spotify" }`
#[derive(Debug, Clone, Default)]
pub struct ManifestRegistry {
    inner: Arc<RwLock<HashMap<String, LoadedTool>>>,
}

impl ManifestRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert all tools from a manifest, keyed by tool name.
    pub fn load_manifest(&self, source: &str, manifest: &Manifest) {
        let mut map = self.inner.write().expect("registry lock");
        for tool in &manifest.tools {
            let key = tool.name.clone();
            map.insert(
                key,
                LoadedTool {
                    tool: tool.clone(),
                    source: source.to_string(),
                },
            );
        }
    }

    /// Insert tools discovered from an MCP server, all tagged with the
    /// given `source` and pointing at `server_key` for dispatch.
    pub fn load_mcp_tools(&self, source: &str, server_key: &str, tools: Vec<Tool>) {
        let mut map = self.inner.write().expect("registry lock");
        for mut tool in tools {
            tool.invoke = InvokeConfig::Mcp {
                server_key: server_key.to_string(),
            };
            let key = tool.name.clone();
            map.insert(
                key,
                LoadedTool {
                    tool,
                    source: source.to_string(),
                },
            );
        }
    }

    /// Register a single built-in tool directly (no manifest file on disk).
    ///
    /// Built-in tools are invisible to the file watcher — they can only be
    /// replaced by another `register_builtin` call.
    pub fn register_builtin(&self, tool: Tool, source: &str) {
        let mut map = self.inner.write().expect("registry lock");
        let key = tool.name.clone();
        map.insert(
            key,
            LoadedTool {
                tool,
                source: source.to_string(),
            },
        );
    }

    /// Remove all tools that came from a specific source (on manifest removal/rename).
    pub fn unload_source(&self, source: &str) {
        let mut map = self.inner.write().expect("registry lock");
        map.retain(|_, v| v.source != source);
    }

    /// Return all currently loaded tools, sorted by name.
    pub fn all(&self) -> Vec<LoadedTool> {
        let map = self.inner.read().expect("registry lock");
        let mut tools: Vec<LoadedTool> = map.values().cloned().collect();
        tools.sort_by(|a, b| a.tool.name.cmp(&b.tool.name));
        tools
    }

    /// Look up a tool by its name.
    pub fn get(&self, name: &str) -> Option<LoadedTool> {
        let map = self.inner.read().expect("registry lock");
        map.get(name).cloned()
    }

    pub fn tool_count(&self) -> usize {
        self.inner.read().expect("registry lock").len()
    }
}

// ---------------------------------------------------------------------------
// MCP server handle map
// ---------------------------------------------------------------------------

/// Thread-safe map of server_key (= source name / file stem) → running McpClient handle.
#[derive(Clone, Default)]
pub struct McpServerMap {
    inner: Arc<RwLock<HashMap<String, McpClient>>>,
}

impl McpServerMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, key: &str, server: McpClient) {
        let mut map = self.inner.write().expect("mcp server map lock");
        map.insert(key.to_string(), server);
    }

    pub fn get(&self, key: &str) -> Option<McpClient> {
        let map = self.inner.read().expect("mcp server map lock");
        map.get(key).cloned()
    }

    pub fn remove(&self, key: &str) {
        let mut map = self.inner.write().expect("mcp server map lock");
        if let Some(server) = map.remove(key) {
            // Spawn a task to cleanly shut down the server (close stdin, kill child).
            tokio::spawn(async move {
                server.shutdown().await;
            });
        }
    }

    pub fn contains(&self, key: &str) -> bool {
        self.inner
            .read()
            .expect("mcp server map lock")
            .contains_key(key)
    }
}

// ---------------------------------------------------------------------------
// Registry snapshot
// ---------------------------------------------------------------------------

/// Write the full contents of `registry` to `snapshot_path` atomically.
///
/// Uses a write-then-rename pattern so the file is never partially written.
/// Called after every registry mutation so that `clawshake tools list` can
/// read up-to-date tool data without connecting to the broker.
fn write_registry_snapshot(snapshot_path: &Path, registry: &ManifestRegistry) {
    let tools: Vec<serde_json::Value> = registry
        .all()
        .into_iter()
        .map(|lt| {
            serde_json::json!({
                "name": lt.tool.name,
                "description": lt.tool.description,
                "source": lt.source,
            })
        })
        .collect();
    let updated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let snapshot = serde_json::json!({
        "updated_at": updated_at,
        "tools": tools,
    });
    let tmp = snapshot_path.with_extension("json.tmp");
    match serde_json::to_string_pretty(&snapshot) {
        Ok(content) => {
            if let Err(e) = std::fs::write(&tmp, &content) {
                warn!("Failed to write registry snapshot tmp file: {e}");
                return;
            }
            if let Err(e) = std::fs::rename(&tmp, snapshot_path) {
                warn!("Failed to rename registry snapshot into place: {e}");
            }
        }
        Err(e) => warn!("Failed to serialise registry snapshot: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Loader helpers
// ---------------------------------------------------------------------------

/// Parse and load a single manifest file into the registry.
/// Static-tool manifests are loaded synchronously.
/// MCP-source manifests spawn a background task that connects to the server,
/// discovers tools, and registers them.  After the async load completes,
/// `change_tx` and `sse_notify_tx` are fired so the bridge re-announces to
/// the DHT and SSE clients receive an updated tool list.
///
/// Returns `true` if static tools were loaded synchronously (caller may
/// announce immediately), or `false` if only async MCP work was spawned
/// (the spawned task will announce when tools are actually ready).
fn load_file(
    path: &Path,
    registry: &ManifestRegistry,
    servers: &McpServerMap,
    rt: &tokio::runtime::Handle,
    snapshot_path: Option<PathBuf>,
    change_tx: Option<tokio::sync::mpsc::Sender<()>>,
    sse_notify_tx: Option<tokio::sync::mpsc::Sender<()>>,
) -> bool {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to read manifest {:?}: {e}", path);
            return false;
        }
    };
    let manifest = match serde_json::from_str::<Manifest>(&content) {
        Ok(m) => m,
        Err(e) => {
            warn!("Failed to parse manifest {:?}: {e}", path);
            return false;
        }
    };

    let source = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    // Track whether static tools were synchronously loaded.
    let mut has_static = false;

    // Load static tools (if any).
    if !manifest.tools.is_empty() {
        info!(
            source = source,
            tools = manifest.tools.len(),
            "Loaded static tools from {:?}",
            path.file_name().unwrap_or_default()
        );
        registry.load_manifest(&source, &manifest);
        if let Some(ref sp) = snapshot_path {
            write_registry_snapshot(sp, registry);
        }
        has_static = true;
    }

    // If this manifest has an MCP source, spawn a task to connect and discover.
    if let Some(ref mcp) = manifest.mcp {
        // Skip if this server is already running (e.g. duplicate load).
        if servers.contains(&source) {
            info!(
                source = source,
                "MCP server already running, skipping spawn"
            );
            return has_static;
        }

        let mcp = mcp.clone();
        let registry = registry.clone();
        let servers = servers.clone();
        let filename = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let snap = snapshot_path.clone();
        let mcp_change_tx = change_tx.clone();
        let mcp_sse_tx = sse_notify_tx.clone();
        rt.spawn(async move {
            match connect_mcp_source(&source, &mcp).await {
                Ok(server) => match server.tools_list().await {
                    Ok(raw_tools) => {
                        let tools = parse_mcp_tools(&raw_tools);
                        info!(
                            source = source,
                            tools = tools.len(),
                            "Discovered MCP tools from {filename}"
                        );
                        registry.load_mcp_tools(&source, &source, tools);
                        servers.insert(&source, server);
                        if let Some(ref sp) = snap {
                            write_registry_snapshot(sp, &registry);
                        }
                        // Signal the bridge to re-announce to the DHT now that
                        // MCP tools are loaded (they weren't available at startup).
                        if let Some(ref tx) = mcp_change_tx {
                            let _ = tx.try_send(());
                        }
                        if let Some(ref tx) = mcp_sse_tx {
                            let _ = tx.try_send(());
                        }
                    }
                    Err(e) => warn!("Failed to list tools from MCP server '{source}': {e}"),
                },
                Err(e) => warn!("Failed to connect MCP server '{source}': {e}"),
            }
        });
    }

    // Warn if manifest has neither tools nor mcp.
    if manifest.tools.is_empty() && manifest.mcp.is_none() {
        warn!(
            "Manifest {:?} has no tools and no mcp source",
            path.file_name().unwrap_or_default()
        );
    }

    // Static tools were loaded synchronously; MCP-only manifests will
    // announce themselves from the async task when ready.
    has_static
}

/// Connect to an MCP server based on the source config.
async fn connect_mcp_source(source: &str, mcp: &McpSource) -> Result<McpClient> {
    match mcp {
        McpSource::Stdio { command, args } => {
            info!(source, command, "Spawning MCP stdio server");
            McpClient::spawn_stdio(command, args, "clawshake-broker").await
        }
        McpSource::Http { url } => {
            info!(source, url, "Connecting to MCP HTTP server");
            Ok(McpClient::connect_http(url))
        }
    }
}

/// Convert raw tool JSON objects from `tools/list` into `Tool` structs.
fn parse_mcp_tools(raw: &[serde_json::Value]) -> Vec<Tool> {
    raw.iter()
        .filter_map(|v| {
            let name = v.get("name")?.as_str()?.to_string();
            let description = v
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();

            // Parse inputSchema from the raw JSON.
            let input_schema = v
                .get("inputSchema")
                .and_then(|s| serde_json::from_value::<InputSchema>(s.clone()).ok())
                .unwrap_or_default();

            Some(Tool {
                name,
                description,
                input_schema,
                requires: None,
                // Placeholder — will be overwritten by load_mcp_tools.
                invoke: InvokeConfig::Mcp {
                    server_key: String::new(),
                },
            })
        })
        .collect()
}

/// Derive the source name from a manifest file path (its file stem).
/// e.g. `/home/user/.clawshake/manifests/spotify.json` → `"spotify"`.
fn source_name_from_path(path: &Path) -> Option<String> {
    Some(path.file_stem()?.to_string_lossy().to_string())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Load every `*.json` manifest in `dir` into `registry` (static tools only).
///
/// This is a lightweight alternative to [`start`] intended for CLI commands
/// that need to inspect the tool set without starting a file-watcher or
/// connecting to MCP servers.
pub fn load_manifests_from_dir(dir: &Path, registry: &ManifestRegistry) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            warn!("Cannot read manifests dir {:?}: {e}", dir);
            return Ok(());
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to read manifest {:?}: {e}", path);
                continue;
            }
        };
        let manifest = match serde_json::from_str::<Manifest>(&content) {
            Ok(m) => m,
            Err(e) => {
                warn!("Failed to parse manifest {:?}: {e}", path);
                continue;
            }
        };
        let source = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if !manifest.tools.is_empty() {
            registry.load_manifest(&source, &manifest);
        }
    }
    Ok(())
}

/// Load all `*.json` files in `manifests_dir` into `registry`, then spawn a
/// background task that watches for file-system changes and keeps the registry
/// live.  Returns immediately after the initial load.
///
/// If `change_tx` is provided the watcher will send `()` through it whenever
/// the tool set changes (manifest created / modified / removed).  The bridge
/// uses this to re-publish the DHT announcement immediately.
///
/// Returns an `McpServerMap` used by the router to dispatch calls to MCP
/// server-backed tools.
pub fn start(
    manifests_dir: PathBuf,
    registry: ManifestRegistry,
    change_tx: Option<tokio::sync::mpsc::Sender<()>>,
    sse_notify_tx: Option<tokio::sync::mpsc::Sender<()>>,
    event_queue: Option<EventQueue>,
) -> Result<McpServerMap> {
    let servers = McpServerMap::new();
    let rt = tokio::runtime::Handle::current();

    // Compute the registry snapshot path: ~/.clawshake/registry.json
    let snapshot_path: Option<PathBuf> = manifests_dir.parent().map(|p| p.join("registry.json"));

    // Initial load.
    match std::fs::read_dir(&manifests_dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("json") {
                    load_file(
                        &path,
                        &registry,
                        &servers,
                        &rt,
                        snapshot_path.clone(),
                        change_tx.clone(),
                        sse_notify_tx.clone(),
                    );
                }
            }
        }
        Err(e) => {
            warn!("Cannot read manifests dir {:?}: {e}", manifests_dir);
        }
    }
    info!(
        dir = ?manifests_dir,
        tools = registry.tool_count(),
        "Manifest registry loaded"
    );

    // Background watcher.
    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(tx)?;
    let _ = std::fs::create_dir_all(&manifests_dir);
    watcher.watch(&manifests_dir, RecursiveMode::NonRecursive)?;

    let watch_servers = servers.clone();
    let watch_eq = event_queue.clone();
    std::thread::spawn(move || {
        let _watcher = watcher;
        for res in rx {
            match res {
                Ok(event) => {
                    let changed = handle_event(
                        event,
                        &registry,
                        &watch_servers,
                        &rt,
                        snapshot_path.as_deref(),
                        change_tx.as_ref(),
                        sse_notify_tx.as_ref(),
                        watch_eq.as_ref(),
                    );
                    if changed {
                        // Note: static-tool manifests re-announce here.
                        // MCP-sourced manifests re-announce from inside the
                        // async task once tools are actually loaded.
                        if let Some(ref tx) = change_tx {
                            let _ = tx.try_send(());
                        }
                        if let Some(ref tx) = sse_notify_tx {
                            let _ = tx.try_send(());
                        }
                    }
                }
                Err(e) => warn!("Manifest watcher error: {e}"),
            }
        }
    });

    Ok(servers)
}

/// Process a single file-system event.  Returns `true` if the tool set
/// was modified (so the caller can fire a re-announce signal).
fn handle_event(
    event: Event,
    registry: &ManifestRegistry,
    servers: &McpServerMap,
    rt: &tokio::runtime::Handle,
    snapshot_path: Option<&Path>,
    change_tx: Option<&tokio::sync::mpsc::Sender<()>>,
    sse_notify_tx: Option<&tokio::sync::mpsc::Sender<()>>,
    event_queue: Option<&EventQueue>,
) -> bool {
    use notify::EventKind::*;
    let paths: Vec<&PathBuf> = event
        .paths
        .iter()
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    if paths.is_empty() {
        return false;
    }
    let mut changed = false;
    match event.kind {
        Create(_) | Modify(_) => {
            for path in paths {
                // Unload the old version first so renamed tools don't linger.
                if let Some(source) = source_name_from_path(path) {
                    registry.unload_source(&source);
                    servers.remove(&source);
                }
                // Returns true only when static tools were synchronously loaded.
                // MCP-only manifests return false — the async task announces
                // when tools are actually ready, with no premature re-announce.
                let static_loaded = load_file(
                    path,
                    registry,
                    servers,
                    rt,
                    snapshot_path.map(|p| p.to_path_buf()),
                    change_tx.cloned(),
                    sse_notify_tx.cloned(),
                );
                if static_loaded {
                    if let (Some(eq), Some(source)) = (event_queue, source_name_from_path(path)) {
                        let tools = registry.tool_count();
                        let eq = eq.clone();
                        rt.spawn(async move {
                            eq.push(
                                "manifest.loaded",
                                "watcher",
                                serde_json::json!({ "name": source, "tools": tools }),
                            )
                            .await;
                        });
                    }
                }
                changed |= static_loaded;
            }
        }
        Remove(_) => {
            for path in paths {
                if let Some(source) = source_name_from_path(path) {
                    registry.unload_source(&source);
                    servers.remove(&source);
                    info!(source, "Unloaded manifest (file removed)");
                    if let Some(sp) = snapshot_path {
                        write_registry_snapshot(sp, registry);
                    }
                    if let Some(eq) = event_queue {
                        let eq = eq.clone();
                        let s = source.clone();
                        rt.spawn(async move {
                            eq.push(
                                "manifest.removed",
                                "watcher",
                                serde_json::json!({ "name": s }),
                            )
                            .await;
                        });
                    }
                    changed = true;
                }
            }
        }
        _ => {}
    }
    changed
}
