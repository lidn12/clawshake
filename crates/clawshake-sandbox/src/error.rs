//! Error types for `clawshake-sandbox`.

use std::fmt;

/// All errors that can be produced by the sandbox.
#[derive(Debug)]
pub enum SandboxError {
    /// A required configuration field was missing or invalid.
    Config(String),
    /// Platform-level setup failed (namespace, seccomp, landlock, Job object …).
    Setup(String),
    /// Spawning the sandboxed process failed.
    Spawn(std::io::Error),
    /// Waiting for the sandboxed process failed.
    Wait(std::io::Error),
    /// An I/O error not related to process control.
    Io(std::io::Error),
    /// The sandbox feature is not supported on this platform or kernel version.
    Unsupported(String),
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(msg) => write!(f, "sandbox config error: {msg}"),
            Self::Setup(msg) => write!(f, "sandbox setup error: {msg}"),
            Self::Spawn(e) => write!(f, "sandbox spawn error: {e}"),
            Self::Wait(e) => write!(f, "sandbox wait error: {e}"),
            Self::Io(e) => write!(f, "sandbox I/O error: {e}"),
            Self::Unsupported(msg) => write!(f, "sandbox unsupported: {msg}"),
        }
    }
}

impl std::error::Error for SandboxError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(e) | Self::Wait(e) | Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for SandboxError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
