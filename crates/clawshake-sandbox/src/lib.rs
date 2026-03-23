//! `clawshake-sandbox` — process-level sandboxing for clawshake tool execution.
//!
//! # What this crate provides
//!
//! A sandboxed process with:
//!
//! - **An ephemeral scratch directory** used as `HOME`, `TMPDIR`, and
//!   package-manager prefix.  All writes by the agent land here and are
//!   deleted when the [`SandboxHandle`] is dropped.
//! - **Declared host-directory mounts** exposed at configurable guest paths
//!   with read-only or read-write access.
//! - **Network isolation** — block all sockets, allow all, or allow outbound
//!   only.
//! - **Platform-specific syscall / path restriction**:
//!   - **Linux**: seccomp BPF denylist (via `seccompiler`) + Landlock LSM
//!     path allowlist (via `landlock`).
//!   - **macOS**: Seatbelt (`sandbox_init`) SBPL profile.
//!   - **Windows**: Win32 Job object with `KILL_ON_JOB_CLOSE`.
//!
//! # Example
//!
//! ```no_run
//! use clawshake_sandbox::{Sandbox, Mount, NetworkPolicy};
//!
//! # async fn example() -> anyhow::Result<()> {
//! let handle = Sandbox::builder()
//!     .command("bash")
//!     .arg("-c")
//!     .arg("npm install && node index.js")
//!     .mount(Mount::read_write("/home/user/project", "/workspace"))
//!     .mount(Mount::read_only("/home/user/.cargo", "/root/.cargo"))
//!     .network(NetworkPolicy::None)
//!     .build()?
//!     .spawn()
//!     .await?;
//!
//! let status = handle.wait().await?;
//! println!("exit: {status}");
//! // Scratch dir deleted, child killed — host is untouched.
//! # Ok(())
//! # }
//! ```

pub mod config;
pub mod error;
pub mod handle;
mod platform;

pub use config::{Mount, NetworkPolicy, ResourceLimits, SandboxConfig};
pub use error::SandboxError;
pub use handle::SandboxHandle;

use std::collections::HashMap;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// SandboxBuilder
// ---------------------------------------------------------------------------

/// Builder for a [`Sandbox`].
///
/// Obtain via [`Sandbox::builder`].
#[derive(Debug, Default)]
pub struct SandboxBuilder {
    argv:        Vec<String>,
    workdir:     Option<PathBuf>,
    mounts:      Vec<Mount>,
    network:     NetworkPolicy,
    extra_env:   HashMap<String, String>,
    inherit_env: bool,
    limits:      ResourceLimits,
    capture:     bool,
}

impl SandboxBuilder {
    /// Set the executable to run.
    pub fn command(mut self, cmd: impl Into<String>) -> Self {
        if self.argv.is_empty() {
            self.argv.push(cmd.into());
        } else {
            self.argv[0] = cmd.into();
        }
        self
    }

    /// Append a single argument.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.argv.push(arg.into());
        self
    }

    /// Append multiple arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.argv.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set the working directory inside the sandbox.
    pub fn workdir(mut self, path: impl Into<PathBuf>) -> Self {
        self.workdir = Some(path.into());
        self
    }

    /// Declare a host directory to expose inside the sandbox.
    pub fn mount(mut self, mount: Mount) -> Self {
        self.mounts.push(mount);
        self
    }

    /// Set the network access policy (default: [`NetworkPolicy::None`]).
    pub fn network(mut self, policy: NetworkPolicy) -> Self {
        self.network = policy;
        self
    }

    /// Set an environment variable that will be present in the sandbox.
    ///
    /// These are applied **after** the environment-rerouting variables set by
    /// the sandbox itself, so they can override the defaults.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_env.insert(key.into(), value.into());
        self
    }

    /// Whether to inherit the parent process's environment variables
    /// (default: `true`).
    pub fn inherit_env(mut self, inherit: bool) -> Self {
        self.inherit_env = inherit;
        self
    }

    /// Set resource limits for the sandbox.
    pub fn limits(mut self, limits: ResourceLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Capture stdout and stderr instead of inheriting them from the parent.
    ///
    /// When enabled, use [`SandboxHandle::wait_with_output`] to retrieve them.
    pub fn capture_output(mut self) -> Self {
        self.capture = true;
        self
    }

    /// Validate the configuration and produce a [`Sandbox`] ready to spawn.
    pub fn build(self) -> Result<Sandbox, SandboxError> {
        let config = SandboxConfig {
            argv:        self.argv,
            workdir:     self.workdir,
            mounts:      self.mounts,
            network:     self.network,
            extra_env:   self.extra_env,
            inherit_env: self.inherit_env,
            limits:      self.limits,
        };
        config.validate()?;
        Ok(Sandbox { config, capture: self.capture })
    }
}

// ---------------------------------------------------------------------------
// Sandbox
// ---------------------------------------------------------------------------

/// A validated, ready-to-spawn sandbox configuration.
///
/// Produced by [`SandboxBuilder::build`]; consumed by [`Sandbox::spawn`].
pub struct Sandbox {
    config:  SandboxConfig,
    capture: bool,
}

impl Sandbox {
    /// Return a new [`SandboxBuilder`].
    pub fn builder() -> SandboxBuilder {
        SandboxBuilder {
            inherit_env: true,
            ..Default::default()
        }
    }

    /// Spawn the sandboxed process and return a [`SandboxHandle`].
    pub async fn spawn(mut self) -> Result<SandboxHandle, SandboxError> {
        // Capture mode: override stdout/stderr in config.
        // (handled inside platform impls by checking this flag — kept simple
        //  for now by injecting it via extra_env sentinel; full pipe support
        //  is straightforward but adds tokio::process::Stdio plumbing.)
        if self.capture {
            self.config.extra_env
                .insert("__CLAWSHAKE_SANDBOX_CAPTURE__".into(), "1".into());
        }
        platform::spawn(&self.config).await
    }
}
