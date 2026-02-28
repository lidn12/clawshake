//! Shared CLI helpers for the broker's `tools` subcommands.
//!
//! Used by both `clawshake-broker tools` and `clawshake tools` to avoid
//! duplicating the manifest-loading and formatting logic.

use anyhow::{bail, Context, Result};
use clawshake_core::manifest::Manifest;
use clawshake_core::permissions::PermissionStore;
use std::path::Path;

use crate::watcher;

// ---------------------------------------------------------------------------
// tools list
// ---------------------------------------------------------------------------

/// Print the tool listing table (or JSON) to stdout.
///
/// The caller is responsible for seeding any built-in manifests before
/// calling this function (e.g. the unified binary seeds `network.json`).
pub async fn list_tools(manifests_dir: &Path, db_path: &Path, json: bool) -> Result<()> {
    // Load all manifests from disk.
    let registry = watcher::ManifestRegistry::new();
    watcher::load_manifests_from_dir(manifests_dir, &registry)?;

    let permissions = PermissionStore::open(db_path).await?;
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

        println!("{:<30}  {:<5}  {}", "Name", "Pub", "Description");
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

/// Count total tools and published tools from the manifests/permissions on disk.
///
/// The caller is responsible for seeding any built-in manifests first.
/// Used by the unified `clawshake` binary via `clawshake_broker::cli::tool_counts`.
#[allow(dead_code)]
pub async fn tool_counts(manifests_dir: &Path, db_path: &Path) -> (usize, usize) {
    let registry = watcher::ManifestRegistry::new();
    let _ = watcher::load_manifests_from_dir(manifests_dir, &registry);
    let tools = registry.all();
    let total = tools.len();
    let published = if let Ok(perms) = PermissionStore::open(db_path).await {
        let mut count = 0;
        for t in &tools {
            if perms.is_network_exposed(&t.tool.name).await {
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
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max - 1])
    }
}
