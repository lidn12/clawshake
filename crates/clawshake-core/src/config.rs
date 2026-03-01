//! Node-level configuration loaded from `~/.clawshake/config.toml`.
//!
//! The config file is entirely optional.  When absent every field falls back
//! to its compiled default, which means an unconfigured node discovers peers
//! only on the local network via mDNS — no external connections are made.
//!
//! # Example `~/.clawshake/config.toml`
//!
//! ```toml
//! [network]
//! bootstrap = [
//!   "/ip4/43.143.33.106/tcp/7474/p2p/12D3KooWDi1ntKAkUYpHfijLNExUTsirFyofnkEB3yjC8P3EGcY5",
//!   "/ip4/43.143.33.106/udp/7474/quic-v1/p2p/12D3KooWDi1ntKAkUYpHfijLNExUTsirFyofnkEB3yjC8P3EGcY5",
//! ]
//! ```

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;
use tracing::{debug, info};

/// Top-level config file structure.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub network: NetworkConfig,
}

/// Network-related settings.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    /// Bootstrap peer multiaddrs to dial on startup.
    ///
    /// When empty (or absent), the node operates in local-only mode — peers
    /// are discovered via mDNS but no external connections are made.
    pub bootstrap: Vec<String>,
}

/// Return the default config directory: `~/.clawshake`.
pub fn config_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".clawshake"))
}

/// Return the default config file path: `~/.clawshake/config.toml`.
pub fn config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("config.toml"))
}

/// Load configuration from the given path, or the default path if `None`.
///
/// Returns `Config::default()` when the config file does not exist — this
/// keeps the zero-config promise intact for a fresh install.
pub fn load(path: Option<&Path>) -> Result<Config> {
    let path = match path {
        Some(p) => p.to_path_buf(),
        None => match config_path() {
            Some(p) => p,
            None => {
                debug!("Cannot determine home directory, using default config");
                return Ok(Config::default());
            }
        },
    };

    if !path.exists() {
        debug!(?path, "Config file not found, using defaults");
        return Ok(Config::default());
    }

    let contents = std::fs::read_to_string(&path)?;
    let config: Config = toml::from_str(&contents)?;
    info!(?path, "Loaded config");
    Ok(config)
}
