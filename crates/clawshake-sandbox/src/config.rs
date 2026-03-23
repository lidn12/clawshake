//! Configuration types for `clawshake-sandbox`.
//!
//! [`SandboxConfig`] is the validated, immutable description of a sandbox.
//! It is built via [`crate::SandboxBuilder`] and consumed by the platform
//! implementation to spawn the sandboxed process.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::error::SandboxError;

// ---------------------------------------------------------------------------
// Mount
// ---------------------------------------------------------------------------

/// Describes a host directory that should be visible inside the sandbox.
#[derive(Debug, Clone)]
pub struct Mount {
    /// Absolute path on the **host** filesystem.
    pub host_path: PathBuf,
    /// Absolute path inside the **sandbox** where the host path will appear.
    ///
    /// Defaults to `host_path` when not specified.
    pub sandbox_path: PathBuf,
    /// Whether the sandbox may write to this mount.
    pub writable: bool,
}

impl Mount {
    /// Expose `host_path` inside the sandbox at the same path, read-only.
    pub fn read_only(host_path: impl Into<PathBuf>) -> Self {
        let host_path = host_path.into();
        let sandbox_path = host_path.clone();
        Self {
            host_path,
            sandbox_path,
            writable: false,
        }
    }

    /// Expose `host_path` inside the sandbox at the same path, read-write.
    pub fn read_write(host_path: impl Into<PathBuf>) -> Self {
        let host_path = host_path.into();
        let sandbox_path = host_path.clone();
        Self {
            host_path,
            sandbox_path,
            writable: true,
        }
    }

    /// Expose `host_path` inside the sandbox at a different `sandbox_path`,
    /// with read-only access.
    pub fn read_only_at(host_path: impl Into<PathBuf>, sandbox_path: impl Into<PathBuf>) -> Self {
        Self {
            host_path: host_path.into(),
            sandbox_path: sandbox_path.into(),
            writable: false,
        }
    }

    /// Expose `host_path` inside the sandbox at a different `sandbox_path`,
    /// with read-write access.
    pub fn read_write_at(host_path: impl Into<PathBuf>, sandbox_path: impl Into<PathBuf>) -> Self {
        Self {
            host_path: host_path.into(),
            sandbox_path: sandbox_path.into(),
            writable: true,
        }
    }
}

// ---------------------------------------------------------------------------
// NetworkPolicy
// ---------------------------------------------------------------------------

/// Controls outbound and inbound network access for the sandboxed process.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NetworkPolicy {
    /// Block all network access (AF_INET + AF_INET6 socket creation denied).
    #[default]
    None,
    /// Allow unrestricted network access.
    Allow,
    /// Allow outbound connections but block inbound listen/bind.
    ///
    /// Implemented on Linux via seccomp; approximated via env vars on
    /// Windows/macOS.
    OutboundOnly,
}

// ---------------------------------------------------------------------------
// ResourceLimits
// ---------------------------------------------------------------------------

/// Optional resource ceilings for the sandboxed process.
///
/// On Linux these map to cgroup v2 limits (if available) or are left
/// unenforced in v1.  On Windows they map to Job object limits.
#[derive(Debug, Clone, Default)]
pub struct ResourceLimits {
    /// Maximum resident memory in bytes.
    pub memory_bytes: Option<u64>,
    /// Maximum wall-clock runtime before SIGKILL is sent.
    pub timeout: Option<std::time::Duration>,
}

// ---------------------------------------------------------------------------
// SandboxConfig
// ---------------------------------------------------------------------------

/// Fully validated, immutable sandbox configuration.
///
/// Obtain via [`crate::SandboxBuilder::build`].
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// The executable to run (first element) and its arguments.
    pub(crate) argv: Vec<String>,
    /// Working directory for the process inside the sandbox.
    pub(crate) workdir: Option<PathBuf>,
    /// Host directories to expose inside the sandbox.
    pub(crate) mounts: Vec<Mount>,
    /// Network access policy.
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    pub(crate) network: NetworkPolicy,
    /// Additional environment variables to inject (merge with inherited env).
    pub(crate) extra_env: HashMap<String, String>,
    /// Whether to inherit the parent process's environment variables.
    pub(crate) inherit_env: bool,
    /// Optional resource limits.
    pub(crate) limits: ResourceLimits,
}

impl SandboxConfig {
    pub(crate) fn validate(&self) -> Result<(), SandboxError> {
        if self.argv.is_empty() {
            return Err(SandboxError::Config("command must not be empty".into()));
        }
        for mount in &self.mounts {
            if !mount.host_path.is_absolute() {
                return Err(SandboxError::Config(format!(
                    "mount host_path must be absolute: {}",
                    mount.host_path.display()
                )));
            }
            if !mount.sandbox_path.is_absolute() {
                return Err(SandboxError::Config(format!(
                    "mount sandbox_path must be absolute: {}",
                    mount.sandbox_path.display()
                )));
            }
        }
        Ok(())
    }
}
