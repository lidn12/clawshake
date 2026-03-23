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
    pub tools: ToolsConfig,
    pub memory: MemoryConfig,
}

/// `[memory]` — long-term memory subsystem settings.
///
/// Path fields override defaults derived from `~/.clawshake/`.
/// Supply them as absolute paths — tilde (`~`) is not expanded.
///
/// # Example
///
/// ```toml
/// [memory]
/// enabled = true
/// # db_path = "/custom/path/memory.db"
/// # transcript_dir = "/custom/path/log"
/// # notes_dir = "/custom/path/notes"
///
/// [memory.watch]
/// debounce_secs = 2
/// watch_transcripts = true
///
/// [memory.ingest]
/// chunk_max_chars = 1600
/// chunk_overlap_chars = 320
/// ```
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// When false, memory tools are not registered and the watcher is not
    /// started.  Default: true.
    pub enabled: bool,

    /// Override `~/.clawshake/memory.db`.
    pub db_path: Option<PathBuf>,

    /// Override `~/.clawshake/log`.
    pub transcript_dir: Option<PathBuf>,

    /// Override `[~/.clawshake/skills, ~/.agents/skills]`.
    pub skill_dirs: Option<Vec<PathBuf>>,

    /// Override `~/.clawshake/notes`.
    pub notes_dir: Option<PathBuf>,

    /// Override `~/.clawshake/identity.md`.
    pub identity_path: Option<PathBuf>,

    /// Override `~/.clawshake/instructions.md`.
    pub instructions_path: Option<PathBuf>,

    pub watch: MemoryWatchConfig,
    pub ingest: MemoryIngestConfig,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            db_path: None,
            transcript_dir: None,
            skill_dirs: None,
            notes_dir: None,
            identity_path: None,
            instructions_path: None,
            watch: MemoryWatchConfig::default(),
            ingest: MemoryIngestConfig::default(),
        }
    }
}

/// `[memory.watch]` — file-system watcher settings.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct MemoryWatchConfig {
    /// Debounce interval in seconds.
    pub debounce_secs: u64,
    /// Whether to watch the transcript directory for new JSONL entries.
    pub watch_transcripts: bool,
}

impl Default for MemoryWatchConfig {
    fn default() -> Self {
        Self {
            debounce_secs: 2,
            watch_transcripts: true,
        }
    }
}

/// `[memory.ingest]` — chunking parameters for file-source ingestion.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct MemoryIngestConfig {
    /// Maximum characters per chunk (sliding-window).
    pub chunk_max_chars: usize,
    /// Overlap between consecutive chunks.
    pub chunk_overlap_chars: usize,
}

impl Default for MemoryIngestConfig {
    fn default() -> Self {
        Self {
            chunk_max_chars: 1600,
            chunk_overlap_chars: 320,
        }
    }
}

/// Tool-related settings.
///
/// # Example
///
/// ```toml
/// [tools.shell]
/// blocked_patterns = ["rm -rf /", "mkfs", "shutdown"]
/// default_timeout_secs = 30
/// max_output_bytes = 1048576
/// ```
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    /// Shell tool settings.
    pub shell: ShellConfig,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            shell: ShellConfig::default(),
        }
    }
}

/// Shell tool configuration.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    /// Additional blocked command patterns (appended to the built-in list).
    pub blocked_patterns: Vec<String>,
    /// Default timeout in seconds.
    pub default_timeout_secs: u64,
    /// Maximum output size in bytes before truncation.
    pub max_output_bytes: usize,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            blocked_patterns: vec![],
            default_timeout_secs: 30,
            max_output_bytes: 1_048_576,
        }
    }
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
        // Tools defaults.
        assert_eq!(c.tools.shell.default_timeout_secs, 30);
        assert_eq!(c.tools.shell.max_output_bytes, 1_048_576);
        assert!(c.tools.shell.blocked_patterns.is_empty());
    }

    #[test]
    fn parse_tools_config() {
        let toml = r#"
[tools.shell]
blocked_patterns = ["custom_danger"]
default_timeout_secs = 60
max_output_bytes = 2097152
"#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.tools.shell.blocked_patterns, ["custom_danger"]);
        assert_eq!(c.tools.shell.default_timeout_secs, 60);
        assert_eq!(c.tools.shell.max_output_bytes, 2_097_152);
    }
}
