//! [`SandboxHandle`] — a running sandboxed process.
//!
//! Dropping the handle kills the process tree and deletes the ephemeral
//! scratch directory.

use std::process::ExitStatus;

use tempfile::TempDir;
use tokio::process::Child;
use tracing::warn;

use crate::error::SandboxError;

/// A running sandboxed process.
///
/// Obtained by calling [`crate::Sandbox::spawn`].
///
/// # Cleanup
///
/// When the handle is dropped (or [`SandboxHandle::destroy`] is called):
/// - The child process (and its process group on Unix) receives `SIGKILL`.
/// - The ephemeral scratch directory is deleted.
/// - Any declared writable [`crate::Mount`] paths on the host are **not**
///   deleted — they are user-owned and may contain deliberate output.
pub struct SandboxHandle {
    pub(crate) child: Child,
    /// Ephemeral scratch home directory; deleted on drop.
    #[allow(dead_code)] // held for its Drop side-effect (TempDir deletes on drop)
    pub(crate) scratch: TempDir,
    /// Platform-specific teardown state (e.g. Win32 Job handle).
    #[cfg(windows)]
    #[allow(dead_code)] // held for its Drop side-effect (KILL_ON_JOB_CLOSE)
    pub(crate) job: crate::platform::windows::JobHandle,
}

impl SandboxHandle {
    /// Wait for the sandboxed process to exit and return its [`ExitStatus`].
    ///
    /// Stdout and stderr are inherited from the parent by default.
    /// Use [`crate::SandboxBuilder::stdout`] / [`crate::SandboxBuilder::stderr`]
    /// to redirect them before spawning.
    pub async fn wait(mut self) -> Result<ExitStatus, SandboxError> {
        self.child.wait().await.map_err(SandboxError::Wait)
    }

    /// Wait for the process, collect its stdout and stderr, and return them.
    ///
    /// Requires that the builder was configured with [`crate::SandboxBuilder::capture_output`]
    /// before spawning; otherwise stdout/stderr are inherited and this returns empty bytes.
    ///
    /// The sandbox (scratch dir + job object) is cleaned up after the child exits.
    pub async fn wait_with_output(self) -> Result<std::process::Output, SandboxError> {
        use std::mem::ManuallyDrop;
        let mut md = ManuallyDrop::new(self);

        // Extract the child so we can await it without holding the Drop guard.
        // Safety: we drop all remaining fields manually below before returning.
        let child = unsafe { std::ptr::read(&md.child) };

        // Eagerly clean up scratch dir and platform teardown state.
        unsafe {
            std::ptr::drop_in_place(&mut md.scratch);
            #[cfg(windows)]
            std::ptr::drop_in_place(&mut md.job);
        }

        child.wait_with_output().await.map_err(SandboxError::Wait)
    }

    /// Kill the sandboxed process and clean up. Equivalent to dropping the
    /// handle but returns an error if the kill or cleanup fails.
    pub async fn destroy(mut self) -> Result<(), SandboxError> {
        self.kill_child().await
    }

    /// Send SIGKILL (Unix) / TerminateProcess (Windows) to the child.
    async fn kill_child(&mut self) -> Result<(), SandboxError> {
        if let Err(e) = self.child.kill().await {
            // ESRCH means the process already exited — not an error.
            if e.kind() != std::io::ErrorKind::InvalidInput {
                return Err(SandboxError::Wait(e));
            }
        }
        Ok(())
    }
}

impl Drop for SandboxHandle {
    fn drop(&mut self) {
        // Best-effort kill; we can't propagate errors from Drop.
        // tokio's `Child::kill` is async, so we use the blocking `start_kill`
        // which sends the signal without waiting for the process to exit.
        if let Err(e) = self.child.start_kill() {
            // InvalidInput = process already exited.
            if e.kind() != std::io::ErrorKind::InvalidInput {
                warn!("sandbox drop: failed to kill child: {e}");
            }
        }
        // `self.scratch` drops here, deleting the temp directory.
        // Platform-specific teardown (e.g. closing the Win32 Job handle with
        // KILL_ON_JOB_CLOSE) also happens through field drops.
    }
}
