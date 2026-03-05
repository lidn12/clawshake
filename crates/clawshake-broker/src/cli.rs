//! Shared CLI helpers for the broker's `tools` subcommands.
//!
//! Used by both `clawshake-broker tools` and `clawshake tools` to avoid
//! duplicating the manifest-loading and formatting logic.

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use clawshake_core::manifest::Manifest;
use clawshake_core::permissions::PermissionStore;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use crate::watcher;

// ---------------------------------------------------------------------------
// Shared CLI types
// ---------------------------------------------------------------------------

/// `tools` subcommands shared between `clawshake-broker tools` and
/// `clawshake tools`.
#[derive(Subcommand, Debug)]
pub enum ToolsAction {
    /// List all registered tools.
    List {
        /// Output as JSON instead of a human-readable table.
        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// Validate a manifest file without installing it.
    Validate {
        /// Path to the manifest JSON file.
        file: PathBuf,
    },

    /// Install a manifest file into the manifests directory.
    Add {
        /// Path to the manifest JSON file to install.
        file: PathBuf,
    },

    /// Remove an installed manifest by name.
    Remove {
        /// Manifest name (e.g. "calendar", not "calendar.json").
        name: String,
    },
}

// ---------------------------------------------------------------------------
// Code mode detection
// ---------------------------------------------------------------------------

/// Detect Node.js on PATH and resolve the effective code-mode state.
///
/// Returns `(has_node, code_mode_active)`.
/// - `has_node`: Node.js is on PATH.
/// - `code_mode_active`: `--code-mode` was explicitly passed AND Node.js is available.
///   Only when this is `true` are `run_code`/`describe_tools` registered and
///   `tools/list` filtering applied.  Node.js detection alone is not enough —
///   code execution must be explicitly opted into.
pub fn detect_code_mode(code_mode_flag: bool) -> (bool, bool) {
    let has_node = which::which("node").is_ok();
    if code_mode_flag && !has_node {
        warn!(
            "Node.js not found on PATH — code mode unavailable. \
             Install Node.js 18+ to enable."
        );
        return (false, false);
    }
    if has_node {
        if code_mode_flag {
            info!("Code mode enabled (Node.js detected)");
        } else {
            info!(
                "Node.js detected on PATH. \
                 Pass --code-mode to enable run_code and describe_tools."
            );
        }
    }
    (has_node, code_mode_flag && has_node)
}

/// Dispatch a `ToolsAction`.
pub async fn run_tools_action(
    action: &ToolsAction,
    manifests_dir: &Path,
    db_path: &Path,
) -> Result<()> {
    match action {
        ToolsAction::List { json } => list_tools(manifests_dir, db_path, *json).await,
        ToolsAction::Validate { file } => validate_manifest(file),
        ToolsAction::Add { file } => add_manifest(file, manifests_dir),
        ToolsAction::Remove { name } => remove_manifest(name, manifests_dir),
    }
}

// ---------------------------------------------------------------------------
// tools list
// ---------------------------------------------------------------------------

/// Read the registry snapshot written by the running broker.
///
/// Returns `(updated_at_unix_secs, tools_array)` when the file exists and
/// is valid, or `None` otherwise.  The broker writes this file atomically
/// (temp-rename) every time the registry changes, so the contents are always
/// consistent.
fn read_registry_snapshot(snapshot_path: &Path) -> Option<(u64, Vec<serde_json::Value>)> {
    let content = std::fs::read_to_string(snapshot_path).ok()?;
    let val: serde_json::Value = serde_json::from_str(&content).ok()?;
    let updated_at = val.get("updated_at")?.as_u64().unwrap_or(0);
    let tools = val.get("tools")?.as_array()?.clone();
    Some((updated_at, tools))
}

/// Format a Unix-epoch `updated_at` as a human-readable age string.
fn format_age(updated_at: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs = now.saturating_sub(updated_at);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

/// Print the tool listing table (or JSON) to stdout.
///
/// Reads `~/.clawshake/registry.json` (written by the broker on every
/// registry change) so that MCP-server-sourced tools (e.g. filesystem)
/// appear in the listing.  Falls back to a static manifest scan when the
/// snapshot file doesn't exist (broker has never run).
///
/// Built-in tools (events, codemode, network) are registered in-memory
/// by the broker at startup — they appear in the snapshot automatically.
pub async fn list_tools(manifests_dir: &Path, db_path: &Path, json: bool) -> Result<()> {
    let permissions = PermissionStore::open(db_path).await?;

    // ~/.clawshake/registry.json lives one level above the manifests dir.
    let snapshot_path = manifests_dir
        .parent()
        .unwrap_or(manifests_dir)
        .join("registry.json");

    // Use the registry snapshot when available (broker has run at least once).
    if let Some((updated_at, snapshot_tools)) = read_registry_snapshot(&snapshot_path) {
        let age = format_age(updated_at);
        if json {
            let mut entries = Vec::new();
            for t in &snapshot_tools {
                let name = t["name"].as_str().unwrap_or("");
                let published = permissions.is_network_exposed(name).await;
                entries.push(serde_json::json!({
                    "name": name,
                    "description": t["description"].as_str().unwrap_or(""),
                    "source": t["source"].as_str().unwrap_or(""),
                    "published": published,
                }));
            }
            println!("{}", serde_json::to_string_pretty(&entries)?);
        } else {
            if snapshot_tools.is_empty() {
                println!("No tools registered.");
                println!("Add manifests to {}", manifests_dir.display());
                return Ok(());
            }
            println!("(registry snapshot — updated {age})");
            println!("{:<30}  {:<5}  Description", "Name", "Pub");
            println!("{}", "-".repeat(78));
            for t in &snapshot_tools {
                let name = t["name"].as_str().unwrap_or("");
                let published = permissions.is_network_exposed(name).await;
                let marker = if published { "  ✓" } else { "  ✗" };
                let desc = truncate(t["description"].as_str().unwrap_or(""), 40);
                println!("{:<30}  {:<5}  {}", name, marker, desc);
            }
        }
        return Ok(());
    }

    // No snapshot — broker has never run.  Fall back to static manifest scan.
    let registry = watcher::ManifestRegistry::new();
    watcher::load_manifests_from_dir(manifests_dir, &registry)?;
    let tools = registry.all();

    if json {
        let mut entries = Vec::new();
        for t in &tools {
            let published = permissions.is_network_exposed(&t.tool.name).await;
            entries.push(serde_json::json!({
                "name": t.tool.name,
                "description": t.tool.description,
                "source": t.source,
                "published": published,
            }));
        }
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        if tools.is_empty() {
            println!("No tools registered.");
            println!("Add manifests to {}", manifests_dir.display());
            return Ok(());
        }
        println!("(offline — no registry snapshot found, showing manifest scan)");
        println!("{:<30}  {:<5}  Description", "Name", "Pub");
        println!("{}", "-".repeat(78));
        for t in &tools {
            let published = permissions.is_network_exposed(&t.tool.name).await;
            let marker = if published { "  ✓" } else { "  ✗" };
            let desc = truncate(&t.tool.description, 40);
            println!("{:<30}  {:<5}  {}", t.tool.name, marker, desc);
        }
    }

    Ok(())
}

/// Count total tools and published tools.
///
/// Reads `~/.clawshake/registry.json` (the broker's state file) so that
/// MCP-server-sourced tools are counted correctly.  Falls back to a static
/// manifest scan when the snapshot doesn't exist.
///
/// Built-in tools appear in the snapshot automatically from the running broker.
/// Used by the unified `clawshake` binary via `clawshake_broker::cli::tool_counts`.
pub async fn tool_counts(manifests_dir: &Path, db_path: &Path) -> (usize, usize) {
    let snapshot_path = manifests_dir
        .parent()
        .unwrap_or(manifests_dir)
        .join("registry.json");

    let tool_names: Vec<String> = if let Some((_ts, tools)) = read_registry_snapshot(&snapshot_path)
    {
        tools
            .iter()
            .filter_map(|t| t["name"].as_str().map(|s| s.to_string()))
            .collect()
    } else {
        let registry = watcher::ManifestRegistry::new();
        let _ = watcher::load_manifests_from_dir(manifests_dir, &registry);
        registry.all().into_iter().map(|lt| lt.tool.name).collect()
    };

    let total = tool_names.len();
    let published = if let Ok(perms) = PermissionStore::open(db_path).await {
        let mut count = 0;
        for name in &tool_names {
            if perms.is_network_exposed(name).await {
                count += 1;
            }
        }
        count
    } else {
        0
    };
    (total, published)
}

// ---------------------------------------------------------------------------
// tools validate
// ---------------------------------------------------------------------------

/// Validate a manifest file without installing it.
///
/// Parses the JSON, checks that it has at least one tool or MCP source,
/// and prints a summary of what it found.
pub fn validate_manifest(file: &Path) -> Result<()> {
    let content =
        std::fs::read_to_string(file).with_context(|| format!("Cannot read {}", file.display()))?;

    let manifest: Manifest = serde_json::from_str(&content)
        .with_context(|| format!("Invalid manifest JSON in {}", file.display()))?;

    if manifest.tools.is_empty() && manifest.mcp.is_none() {
        bail!(
            "Manifest {} has no tools and no mcp source — at least one is required",
            file.display()
        );
    }

    let name = file.file_stem().unwrap_or_default().to_string_lossy();

    println!("✓ {} is valid (version {})", name, manifest.version);
    if !manifest.tools.is_empty() {
        println!("  {} static tool(s):", manifest.tools.len());
        for t in &manifest.tools {
            println!("    - {}: {}", t.name, truncate(&t.description, 50));
        }
    }
    if let Some(ref mcp) = manifest.mcp {
        match mcp {
            clawshake_core::manifest::McpSource::Stdio { command, .. } => {
                println!("  MCP source: stdio ({})", command);
            }
            clawshake_core::manifest::McpSource::Http { url } => {
                println!("  MCP source: http ({})", url);
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// tools add
// ---------------------------------------------------------------------------

/// Validate and install a manifest file into the manifests directory.
///
/// Validates the file first, then copies it to `manifests_dir/<stem>.json`.
/// If a manifest with the same name already exists, it is overwritten.
pub fn add_manifest(file: &Path, manifests_dir: &Path) -> Result<()> {
    // Validate first.
    let content =
        std::fs::read_to_string(file).with_context(|| format!("Cannot read {}", file.display()))?;

    let manifest: Manifest = serde_json::from_str(&content)
        .with_context(|| format!("Invalid manifest JSON in {}", file.display()))?;

    if manifest.tools.is_empty() && manifest.mcp.is_none() {
        bail!(
            "Manifest {} has no tools and no mcp source — at least one is required",
            file.display()
        );
    }

    // Ensure manifests dir exists.
    std::fs::create_dir_all(manifests_dir)
        .with_context(|| format!("Cannot create {}", manifests_dir.display()))?;

    let file_name = file
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine file name from {}", file.display()))?;
    let dest = manifests_dir.join(file_name);

    std::fs::copy(file, &dest).with_context(|| format!("Cannot copy to {}", dest.display()))?;

    let name = file.file_stem().unwrap_or_default().to_string_lossy();
    let tool_count = manifest.tools.len();
    let has_mcp = manifest.mcp.is_some();

    println!("Installed {name} → {}", dest.display());
    if tool_count > 0 {
        println!("  {tool_count} static tool(s)");
    }
    if has_mcp {
        println!("  + MCP source (dynamic tools)");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// tools remove
// ---------------------------------------------------------------------------

/// Remove a manifest by name from the manifests directory.
///
/// `name` is the manifest stem (e.g. `calendar`, not `calendar.json`).
pub fn remove_manifest(name: &str, manifests_dir: &Path) -> Result<()> {
    let file = manifests_dir.join(format!("{name}.json"));
    if !file.exists() {
        bail!(
            "No manifest named \"{name}\" in {}",
            manifests_dir.display()
        );
    }
    std::fs::remove_file(&file).with_context(|| format!("Cannot remove {}", file.display()))?;
    println!("Removed {name} ({})", file.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Truncate a string to `max` chars, appending "…" if truncated.
pub fn truncate(s: &str, max: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max - 1).collect();
        format!("{truncated}…")
    }
}
