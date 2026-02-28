//! Ed25519 keypair persistence for the P2P identity.
//!
//! The keypair is the node's permanent identity on the libp2p network.
//! It is loaded from `~/.clawshake/identity.key` (or a user-specified path)
//! and generated on first run.

use anyhow::{Context, Result};
use std::path::PathBuf;
use tracing::info;

pub(super) fn load_or_create_keypair(
    override_path: Option<&std::path::Path>,
) -> Result<libp2p::identity::Keypair> {
    let path = match override_path {
        Some(p) => p.to_path_buf(),
        None => keypair_path()?,
    };

    if path.exists() {
        let bytes = std::fs::read(&path)
            .with_context(|| format!("reading keypair from {}", path.display()))?;
        let keypair = libp2p::identity::Keypair::from_protobuf_encoding(&bytes)
            .context("decoding keypair")?;
        info!("Loaded existing keypair from {}", path.display());
        Ok(keypair)
    } else {
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let bytes = keypair.to_protobuf_encoding().context("encoding keypair")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }
        std::fs::write(&path, &bytes)
            .with_context(|| format!("writing keypair to {}", path.display()))?;
        info!("Generated new keypair, saved to {}", path.display());
        Ok(keypair)
    }
}

fn keypair_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home.join(".clawshake").join("identity.key"))
}
