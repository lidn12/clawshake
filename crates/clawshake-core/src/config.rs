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
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

/// Top-level config file structure.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub network: NetworkConfig,
    pub models: ModelsConfig,
    pub tools: ToolsConfig,
    pub memory: MemoryConfig,
    pub sandbox: SandboxDefaults,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

    /// TCP port for inbound P2P connections.  `0` means pick a random port.
    /// In relay-server mode this defaults to 7474 instead of 0.
    pub listen_port: Option<u16>,

    /// Enable relay-server mode: this node will forward traffic between peers
    /// behind NAT.
    #[serde(default)]
    pub relay_server: bool,

    /// Path to the Ed25519 keypair file.  When absent, defaults to
    /// `~/.clawshake/identity.key`.
    pub identity_path: Option<PathBuf>,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AdvertiseAll {
    #[serde(rename = "all")]
    All,
}

/// Helper for deserializing the string `"none"`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

// ---------------------------------------------------------------------------
// Sandbox defaults
// ---------------------------------------------------------------------------

/// `[sandbox]` — default process-isolation policy for tool execution.
///
/// These are **node-level defaults** applied by the broker when spawning
/// sandboxed processes.  Individual tool manifests can override per-tool.
///
/// # Example
///
/// ```toml
/// [sandbox]
/// enabled = false
/// network = "none"
/// memory_bytes = 536870912
/// timeout_secs = 300
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxDefaults {
    /// Apply sandboxing to CLI-invoked tools by default.  Default: false
    /// (opt-in).  When true the broker wraps child processes with the
    /// platform sandbox unless the manifest explicitly opts out.
    pub enabled: bool,

    /// Default network policy for sandboxed processes.
    /// One of `"none"`, `"allow"`, or `"outbound_only"`.
    pub network: SandboxNetworkPolicy,

    /// Maximum resident memory in bytes (0 = unlimited).
    pub memory_bytes: u64,

    /// Maximum wall-clock runtime in seconds before the process is killed
    /// (0 = unlimited).
    pub timeout_secs: u64,

    /// Additional host paths to mount read-only in every sandbox.
    pub read_only_mounts: Vec<PathBuf>,
}

impl Default for SandboxDefaults {
    fn default() -> Self {
        Self {
            enabled: false,
            network: SandboxNetworkPolicy::None,
            memory_bytes: 0,
            timeout_secs: 0,
            read_only_mounts: vec![],
        }
    }
}

/// Network policy for sandboxed processes — TOML-friendly enum.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxNetworkPolicy {
    /// Block all network access.
    None,
    /// Allow unrestricted network access.
    Allow,
    /// Allow outbound connections, block inbound listen/bind.
    OutboundOnly,
}

impl Default for SandboxNetworkPolicy {
    fn default() -> Self {
        Self::None
    }
}

// ---------------------------------------------------------------------------
// CLI override merging
// ---------------------------------------------------------------------------

impl Config {
    /// Merge CLI flag overrides into the loaded config.
    ///
    /// CLI flags take precedence over `config.toml` values; absent (default)
    /// flags leave config values intact.  This implements the priority stack:
    /// compiled defaults → config.toml → CLI flags.
    ///
    /// `p2p_port`, `boot_peers`, `relay_server`, `identity` come from
    /// `P2pArgs`; `models_endpoint` from `McpArgs` (future).
    pub fn apply_p2p_overrides(
        &mut self,
        p2p_port: u16,
        boot_peers: &[String],
        relay_server: bool,
        identity: Option<&Path>,
    ) {
        // Port: CLI 0 means "not specified" — use config value or default.
        if p2p_port != 0 {
            self.network.listen_port = Some(p2p_port);
        }
        // Bootstrap: non-empty CLI list replaces config entirely.
        if !boot_peers.is_empty() {
            self.network.bootstrap = boot_peers.to_vec();
        }
        // Relay server: CLI flag is additive (if set, enable; config can also
        // enable it even without the flag).
        if relay_server {
            self.network.relay_server = true;
        }
        // Identity: CLI path overrides config path.
        if let Some(path) = identity {
            self.network.identity_path = Some(path.to_path_buf());
        }
    }

    /// Effective P2P listen port after defaults and relay-server logic.
    pub fn effective_p2p_port(&self, relay_default: u16) -> u16 {
        match self.network.listen_port {
            Some(p) => p,
            None if self.network.relay_server => relay_default,
            None => 0,
        }
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

    // ── Sandbox config tests ──────────────────────────────────────────

    #[test]
    fn default_sandbox() {
        let c = Config::default();
        assert!(!c.sandbox.enabled);
        assert_eq!(c.sandbox.network, SandboxNetworkPolicy::None);
        assert_eq!(c.sandbox.memory_bytes, 0);
        assert_eq!(c.sandbox.timeout_secs, 0);
        assert!(c.sandbox.read_only_mounts.is_empty());
    }

    #[test]
    fn parse_sandbox_config() {
        let toml = r#"
[sandbox]
enabled = true
network = "outbound_only"
memory_bytes = 536870912
timeout_secs = 300
read_only_mounts = ["/usr/share/ca-certificates"]
"#;
        let c: Config = toml::from_str(toml).unwrap();
        assert!(c.sandbox.enabled);
        assert_eq!(c.sandbox.network, SandboxNetworkPolicy::OutboundOnly);
        assert_eq!(c.sandbox.memory_bytes, 536_870_912);
        assert_eq!(c.sandbox.timeout_secs, 300);
        assert_eq!(
            c.sandbox.read_only_mounts,
            [std::path::PathBuf::from("/usr/share/ca-certificates")]
        );
    }

    #[test]
    fn parse_sandbox_network_allow() {
        let toml = r#"
[sandbox]
network = "allow"
"#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.sandbox.network, SandboxNetworkPolicy::Allow);
    }

    // ── Network config tests ──────────────────────────────────────────

    #[test]
    fn parse_network_listen_port() {
        let toml = r#"
[network]
listen_port = 9999
relay_server = true
identity_path = "/tmp/test.key"
"#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.network.listen_port, Some(9999));
        assert!(c.network.relay_server);
        assert_eq!(
            c.network.identity_path,
            Some(std::path::PathBuf::from("/tmp/test.key"))
        );
    }

    // ── CLI override merging tests ────────────────────────────────────

    #[test]
    fn apply_p2p_overrides_port() {
        let mut c = Config::default();
        // 0 means "not specified" — should not override.
        c.network.listen_port = Some(8888);
        c.apply_p2p_overrides(0, &[], false, None);
        assert_eq!(c.network.listen_port, Some(8888));
        // Non-zero overrides.
        c.apply_p2p_overrides(9999, &[], false, None);
        assert_eq!(c.network.listen_port, Some(9999));
    }

    #[test]
    fn apply_p2p_overrides_boot_peers() {
        let mut c = Config::default();
        c.network.bootstrap = vec!["old".into()];
        // Empty CLI list leaves config intact.
        c.apply_p2p_overrides(0, &[], false, None);
        assert_eq!(c.network.bootstrap, ["old"]);
        // Non-empty replaces entirely.
        c.apply_p2p_overrides(0, &["new1".into(), "new2".into()], false, None);
        assert_eq!(c.network.bootstrap, ["new1", "new2"]);
    }

    #[test]
    fn apply_p2p_overrides_relay_and_identity() {
        let mut c = Config::default();
        assert!(!c.network.relay_server);
        assert!(c.network.identity_path.is_none());

        c.apply_p2p_overrides(0, &[], true, Some(std::path::Path::new("/k.key")));
        assert!(c.network.relay_server);
        assert_eq!(
            c.network.identity_path,
            Some(std::path::PathBuf::from("/k.key"))
        );
    }

    // ── effective_p2p_port tests ──────────────────────────────────────

    #[test]
    fn effective_p2p_port_explicit() {
        let mut c = Config::default();
        c.network.listen_port = Some(5555);
        assert_eq!(c.effective_p2p_port(7474), 5555);
    }

    #[test]
    fn effective_p2p_port_relay_default() {
        let mut c = Config::default();
        c.network.relay_server = true;
        // No explicit port + relay_server → relay default.
        assert_eq!(c.effective_p2p_port(7474), 7474);
    }

    #[test]
    fn effective_p2p_port_random() {
        let c = Config::default();
        // No port, no relay → 0 (random).
        assert_eq!(c.effective_p2p_port(7474), 0);
    }

    // ── Serialization round-trip ──────────────────────────────────────

    #[test]
    fn config_round_trip() {
        let original = Config::default();
        let toml_str = toml::to_string_pretty(&original).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        // Spot-check key fields survived the round-trip.
        assert_eq!(parsed.models.proxy_port, original.models.proxy_port);
        assert_eq!(parsed.sandbox.enabled, original.sandbox.enabled);
        assert_eq!(parsed.sandbox.network, original.sandbox.network);
        assert_eq!(
            parsed.tools.shell.default_timeout_secs,
            original.tools.shell.default_timeout_secs
        );
    }
}
