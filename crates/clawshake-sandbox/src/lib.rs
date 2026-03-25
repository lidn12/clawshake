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
//! # async fn example() -> Result<(), clawshake_sandbox::SandboxError> {
//! let handle = Sandbox::builder()
//!     .command("bash")
//!     .arg("-c")
//!     .arg("npm install && node index.js")
//!     .mount(Mount::read_write_at("/home/user/project", "/workspace"))
//!     .mount(Mount::read_only_at("/home/user/.cargo", "/root/.cargo"))
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
    argv: Vec<String>,
    workdir: Option<PathBuf>,
    mounts: Vec<Mount>,
    network: NetworkPolicy,
    extra_env: HashMap<String, String>,
    inherit_env: bool,
    limits: ResourceLimits,
    capture: bool,
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
            argv: self.argv,
            workdir: self.workdir,
            mounts: self.mounts,
            network: self.network,
            extra_env: self.extra_env,
            inherit_env: self.inherit_env,
            limits: self.limits,
        };
        config.validate()?;
        Ok(Sandbox {
            config,
            capture: self.capture,
        })
    }
}

// ---------------------------------------------------------------------------
// Sandbox
// ---------------------------------------------------------------------------

/// A validated, ready-to-spawn sandbox configuration.
///
/// Produced by [`SandboxBuilder::build`]; consumed by [`Sandbox::spawn`].
#[derive(Debug)]
pub struct Sandbox {
    config: SandboxConfig,
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
            self.config
                .extra_env
                .insert("__CLAWSHAKE_SANDBOX_CAPTURE__".into(), "1".into());
        }
        platform::spawn(&self.config).await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Builder validation -------------------------------------------------

    #[test]
    fn build_fails_without_command() {
        let err = Sandbox::builder().build().unwrap_err();
        assert!(
            matches!(err, SandboxError::Config(_)),
            "expected Config error, got: {err:?}"
        );
    }

    #[test]
    fn build_fails_with_relative_host_mount() {
        let err = Sandbox::builder()
            .command("echo")
            .mount(Mount {
                host_path: "relative/path".into(),
                sandbox_path: "/absolute".into(),
                writable: false,
            })
            .build()
            .unwrap_err();
        assert!(
            matches!(err, SandboxError::Config(_)),
            "expected Config error, got: {err:?}"
        );
    }

    #[test]
    fn build_fails_with_relative_sandbox_mount() {
        let err = Sandbox::builder()
            .command("echo")
            .mount(Mount {
                host_path: if cfg!(windows) {
                    "C:\\abs".into()
                } else {
                    "/abs".into()
                },
                sandbox_path: "relative".into(),
                writable: false,
            })
            .build()
            .unwrap_err();
        assert!(
            matches!(err, SandboxError::Config(_)),
            "expected Config error, got: {err:?}"
        );
    }

    #[test]
    fn build_succeeds_with_minimal_config() {
        Sandbox::builder()
            .command("echo")
            .arg("hello")
            .build()
            .expect("minimal builder should succeed");
    }

    // -- Config defaults ----------------------------------------------------

    #[test]
    fn network_policy_default_is_none() {
        assert_eq!(NetworkPolicy::default(), NetworkPolicy::None);
    }

    #[test]
    fn resource_limits_default_is_unbounded() {
        let rl = ResourceLimits::default();
        assert!(rl.memory_bytes.is_none());
        assert!(rl.timeout.is_none());
    }

    // -- Mount constructors -------------------------------------------------

    #[test]
    fn mount_read_only_mirrors_path() {
        let m = Mount::read_only("/data");
        assert_eq!(m.host_path.to_str().unwrap(), "/data");
        assert_eq!(m.sandbox_path.to_str().unwrap(), "/data");
        assert!(!m.writable);
    }

    #[test]
    fn mount_read_write_mirrors_path() {
        let m = Mount::read_write("/data");
        assert_eq!(m.host_path, m.sandbox_path);
        assert!(m.writable);
    }

    #[test]
    fn mount_read_only_at_uses_distinct_paths() {
        let m = Mount::read_only_at("/host", "/guest");
        assert_eq!(m.host_path.to_str().unwrap(), "/host");
        assert_eq!(m.sandbox_path.to_str().unwrap(), "/guest");
        assert!(!m.writable);
    }

    #[test]
    fn mount_read_write_at_uses_distinct_paths() {
        let m = Mount::read_write_at("/host", "/guest");
        assert_eq!(m.host_path.to_str().unwrap(), "/host");
        assert_eq!(m.sandbox_path.to_str().unwrap(), "/guest");
        assert!(m.writable);
    }

    // -- Builder chaining ---------------------------------------------------

    #[test]
    fn builder_chaining() {
        let sb = Sandbox::builder()
            .command("bash")
            .arg("-c")
            .args(["echo", "hello"])
            .workdir("/tmp")
            .network(NetworkPolicy::Allow)
            .env("FOO", "bar")
            .inherit_env(false)
            .limits(ResourceLimits {
                memory_bytes: Some(1024 * 1024),
                timeout: Some(std::time::Duration::from_secs(10)),
            })
            .capture_output()
            .build()
            .expect("chained builder should succeed");
        // Verifying it compiles and validates is sufficient.
        let _ = sb;
    }

    // -- Spawn + wait (integration) -----------------------------------------

    #[tokio::test]
    async fn spawn_echo_succeeds() {
        let cmd = if cfg!(windows) { "cmd" } else { "echo" };
        let mut builder = Sandbox::builder().command(cmd);
        if cfg!(windows) {
            builder = builder.arg("/C").arg("echo hello");
        } else {
            builder = builder.arg("hello");
        }
        let handle = builder
            .build()
            .unwrap()
            .spawn()
            .await
            .expect("spawn should succeed");
        let status = handle.wait().await.expect("wait should succeed");
        assert!(status.success(), "exit status: {:?}", status);
    }

    #[tokio::test]
    async fn spawn_nonexistent_command_fails() {
        let result = Sandbox::builder()
            .command("__nonexistent_clawshake_test_binary__")
            .build()
            .unwrap()
            .spawn()
            .await;
        assert!(result.is_err(), "spawning a nonexistent binary should fail");
    }
}
