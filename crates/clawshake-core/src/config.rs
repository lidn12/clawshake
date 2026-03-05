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
//! description = "work laptop"
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
    pub models: ModelsConfig,
}

/// Network-related settings.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    /// Human-readable description of this node, e.g. "work laptop" or
    /// "build server in the closet".  Included in DHT announcements so
    /// remote agents can identify which peer is which.
    pub description: Option<String>,

    /// Bootstrap peer multiaddrs to dial on startup.
    ///
    /// When empty (or absent), the node operates in local-only mode — peers
    /// are discovered via mDNS but no external connections are made.
    pub bootstrap: Vec<String>,
}

/// Model proxy settings.
///
/// When this section is absent the model proxy is completely disabled — no
/// DHT advertisement, no model handler registered, zero overhead.
///
/// # Example
///
/// ```toml
/// [models]
/// endpoint = "http://127.0.0.1:11434"
/// advertise = "all"
/// proxy_port = 11435
/// ```
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ModelsConfig {
    /// Local model server endpoint (Ollama, vLLM, llama.cpp, etc.).
    /// Must expose an OpenAI-compatible API.
    pub endpoint: Option<String>,

    /// Which models to advertise on the P2P network.
    ///
    /// - `"all"` (default when endpoint is set) — query the backend's
    ///   `/v1/models` at startup and advertise everything.
    /// - An explicit list of model names to advertise.
    /// - Absent or empty — do not advertise any models.
    pub advertise: AdvertiseModels,

    /// Port for the local OpenAI-compatible proxy server.
    /// Applications point `OPENAI_BASE_URL` at `http://127.0.0.1:{proxy_port}/v1`.
    pub proxy_port: u16,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            advertise: AdvertiseModels::default(),
            proxy_port: 11435,
        }
    }
}

impl ModelsConfig {
    /// Returns `true` if a model server endpoint is configured.
    pub fn is_enabled(&self) -> bool {
        self.endpoint.is_some()
    }
}

/// Controls which models are advertised on the network.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum AdvertiseModels {
    /// Advertise all models discovered from the backend.
    All(AdvertiseAll),
    /// Do not advertise any models (explicit `"none"` in config).
    None(AdvertiseNone),
    /// Advertise a specific list of model names.
    List(Vec<String>),
}

/// Helper for deserializing the string `"all"`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub enum AdvertiseAll {
    #[serde(rename = "all")]
    All,
}

/// Helper for deserializing the string `"none"`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub enum AdvertiseNone {
    #[serde(rename = "none")]
    None,
}

impl Default for AdvertiseModels {
    fn default() -> Self {
        Self::None(AdvertiseNone::None)
    }
}

impl AdvertiseModels {
    /// Returns `true` if no models should be advertised.
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None(_))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let c = Config::default();
        assert!(c.network.bootstrap.is_empty());
        assert!(c.models.endpoint.is_none());
        assert_eq!(c.models.proxy_port, 11435);
        assert!(c.models.advertise.is_none());
    }

    #[test]
    fn parse_full_config() {
        let toml = r#"
[network]
bootstrap = ["/ip4/1.2.3.4/tcp/7474/p2p/12D3KooWabc"]

[models]
endpoint = "http://127.0.0.1:11434"
advertise = "all"
proxy_port = 12345
"#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            c.network.bootstrap,
            ["/ip4/1.2.3.4/tcp/7474/p2p/12D3KooWabc"]
        );
        assert_eq!(c.models.endpoint.as_deref(), Some("http://127.0.0.1:11434"));
        // Use a non-default value so the assertion fails if TOML parsing is
        // silently skipping the key and returning the default (11435) instead.
        assert_eq!(c.models.proxy_port, 12345);
        assert!(matches!(c.models.advertise, AdvertiseModels::All(_)));
    }

    #[test]
    fn parse_advertise_list() {
        let toml = r#"
[models]
endpoint = "http://localhost:11434"
advertise = ["llama3", "codellama"]
"#;
        let c: Config = toml::from_str(toml).unwrap();
        match &c.models.advertise {
            AdvertiseModels::List(names) => {
                assert_eq!(names, &["llama3", "codellama"]);
            }
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn parse_advertise_none() {
        let toml = r#"
[models]
endpoint = "http://localhost:11434"
advertise = "none"
"#;
        let c: Config = toml::from_str(toml).unwrap();
        assert!(matches!(c.models.advertise, AdvertiseModels::None(_)));
    }

    #[test]
    fn parse_minimal_config() {
        let c: Config = toml::from_str("").unwrap();
        assert!(c.network.bootstrap.is_empty());
        assert!(c.models.endpoint.is_none());
        // proxy_port default must be 11435; a change to the Default impl
        // would silently break the model proxy without this assertion.
        assert_eq!(
            c.models.proxy_port, 11435,
            "default proxy_port must be 11435"
        );
        assert!(
            c.models.advertise.is_none(),
            "default advertise must be None"
        );
    }
}
