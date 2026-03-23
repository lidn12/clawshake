//! Platform dispatch.
//!
//! Each platform module exposes a single `spawn(config) -> Result<SandboxHandle>`
//! function that the public API calls.  The compile-time cfg gates below ensure
//! only the relevant implementation is compiled in.

#[cfg(target_os = "linux")]
pub(crate) mod linux;

#[cfg(target_os = "macos")]
pub(crate) mod macos;

#[cfg(windows)]
pub(crate) mod windows;

use crate::{config::SandboxConfig, error::SandboxError, handle::SandboxHandle};

/// Spawn a sandboxed process according to `config`.
pub(crate) async fn spawn(config: &SandboxConfig) -> Result<SandboxHandle, SandboxError> {
    #[cfg(target_os = "linux")]
    return linux::spawn(config).await;

    #[cfg(target_os = "macos")]
    return macos::spawn(config).await;

    #[cfg(windows)]
    return windows::spawn(config).await;

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    Err(SandboxError::Unsupported(
        "clawshake-sandbox: no implementation for this platform".into(),
    ))
}
