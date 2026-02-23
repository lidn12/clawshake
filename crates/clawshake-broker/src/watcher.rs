use anyhow::Result;

/// Watches `~/.clawshake/manifests/` for added, modified, and removed
/// manifest files and updates the live manifest registry accordingly.
///
/// Implemented in Track 2 (Milestone 1 of the broker).
pub async fn watch(_manifests_dir: &std::path::Path) -> Result<()> {
    // TODO(track-2): use `notify` crate to watch for manifest changes
    Ok(())
}
