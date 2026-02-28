//! Shared CLI helpers for the broker's `tools` listing.
//!
//! Used by both `clawshake-broker tools` and `clawshake tools` to avoid
//! duplicating the manifest-loading and formatting logic.

use anyhow::Result;
use clawshake_core::permissions::PermissionStore;
use std::path::Path;

use crate::{builtins, watcher};

/// Print the tool listing table (or JSON) to stdout.
pub async fn list_tools(manifests_dir: &Path, db_path: &Path, json: bool) -> Result<()> {
    // Seed built-in manifests so network_* always appears.
    builtins::seed(manifests_dir)?;

    // Load all manifests exactly the way the broker does.
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
/// Used by the unified `clawshake` binary via `clawshake_broker::cli::tool_counts`.
#[allow(dead_code)]
pub async fn tool_counts(manifests_dir: &Path, db_path: &Path) -> (usize, usize) {
    let _ = builtins::seed(manifests_dir);
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

/// Truncate a string to `max` chars, appending "…" if truncated.
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max - 1])
    }
}
