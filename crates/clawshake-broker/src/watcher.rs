use anyhow::Result;
use clawshake_core::manifest::{Manifest, Tool};
use notify::{Event, RecursiveMode, Watcher};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// A single tool entry as loaded from a manifest, with its app prefix.
#[derive(Debug, Clone)]
pub struct LoadedTool {
    pub tool: Tool,
    pub app: String,
}

/// Thread-safe map of qualified tool name → loaded tool.
/// e.g. `"spotify.play"` → `LoadedTool { tool: ..., app: "spotify" }`
#[derive(Debug, Clone, Default)]
pub struct ManifestRegistry {
    inner: Arc<RwLock<HashMap<String, LoadedTool>>>,
}

impl ManifestRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert all tools from a manifest, keyed by qualified name.
    pub fn load_manifest(&self, manifest: &Manifest) {
        let mut map = self.inner.write().expect("registry lock");
        for tool in &manifest.tools {
            let key = tool.name.clone();
            map.insert(
                key,
                LoadedTool {
                    tool: tool.clone(),
                    app: manifest.app.clone(),
                },
            );
        }
    }

    /// Remove all tools that came from a specific app (on manifest removal/rename).
    pub fn unload_app(&self, app: &str) {
        let mut map = self.inner.write().expect("registry lock");
        map.retain(|_, v| v.app != app);
    }

    /// Return all currently loaded tools, sorted by name.
    pub fn all(&self) -> Vec<LoadedTool> {
        let map = self.inner.read().expect("registry lock");
        let mut tools: Vec<LoadedTool> = map.values().cloned().collect();
        tools.sort_by(|a, b| a.tool.name.cmp(&b.tool.name));
        tools
    }

    /// Look up a tool by its qualified name.
    pub fn get(&self, qualified_name: &str) -> Option<LoadedTool> {
        let map = self.inner.read().expect("registry lock");
        map.get(qualified_name).cloned()
    }

    pub fn tool_count(&self) -> usize {
        self.inner.read().expect("registry lock").len()
    }
}

// ---------------------------------------------------------------------------
// Loader helpers
// ---------------------------------------------------------------------------

/// Parse and load a single manifest file into the registry.
/// Logs a warning and skips on parse error.
fn load_file(path: &Path, registry: &ManifestRegistry) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to read manifest {:?}: {e}", path);
            return;
        }
    };
    match serde_json::from_str::<Manifest>(&content) {
        Ok(manifest) => {
            info!(
                app = manifest.app,
                tools = manifest.tools.len(),
                "Loaded manifest {:?}",
                path.file_name().unwrap_or_default()
            );
            registry.load_manifest(&manifest);
        }
        Err(e) => {
            warn!("Failed to parse manifest {:?}: {e}", path);
        }
    }
}

/// Derive the app name from a manifest file path by parsing the file.
/// Returns `None` if the file can't be read or parsed.
fn app_name_from_path(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let manifest: Manifest = serde_json::from_str(&content).ok()?;
    Some(manifest.app.clone())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Load all `*.json` files in `manifests_dir` into `registry`, then spawn a
/// background task that watches for file-system changes and keeps the registry
/// live.  Returns immediately after the initial load.
pub fn start(manifests_dir: PathBuf, registry: ManifestRegistry) -> Result<()> {
    // Initial load.
    match std::fs::read_dir(&manifests_dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("json") {
                    load_file(&path, &registry);
                }
            }
        }
        Err(e) => {
            warn!("Cannot read manifests dir {:?}: {e}", manifests_dir);
            // Not fatal — dir may not exist yet on first run.
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
    // Create the directory if it doesn't exist so the watcher has something to watch.
    let _ = std::fs::create_dir_all(&manifests_dir);
    watcher.watch(&manifests_dir, RecursiveMode::NonRecursive)?;

    std::thread::spawn(move || {
        // Keep watcher alive for the lifetime of this thread.
        let _watcher = watcher;
        for res in rx {
            match res {
                Ok(event) => handle_event(event, &registry),
                Err(e) => warn!("Manifest watcher error: {e}"),
            }
        }
    });

    Ok(())
}

fn handle_event(event: Event, registry: &ManifestRegistry) {
    use notify::EventKind::*;
    let paths: Vec<&PathBuf> = event
        .paths
        .iter()
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    if paths.is_empty() {
        return;
    }
    match event.kind {
        Create(_) | Modify(_) => {
            for path in paths {
                // Unload the old version first so renamed tools don't linger.
                if let Some(app) = app_name_from_path(path) {
                    registry.unload_app(&app);
                }
                load_file(path, registry);
            }
        }
        Remove(_) => {
            for path in paths {
                if let Some(app) = app_name_from_path(path) {
                    registry.unload_app(&app);
                    info!(app, "Unloaded manifest (file removed)");
                }
            }
        }
        _ => {}
    }
}
