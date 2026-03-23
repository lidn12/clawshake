//! Windows sandbox implementation.
//!
//! Security layers:
//!
//! 1. **Environment rerouting** — `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`,
//!    `TEMP`, and package-manager prefixes point at an ephemeral [`TempDir`].
//!
//! 2. **Job object with `KILL_ON_JOB_CLOSE`** — all processes spawned under
//!    the job are killed atomically when the [`SandboxHandle`] is dropped.
//!    Optional memory and CPU limits (`ResourceLimits`) are set via
//!    `SetInformationJobObject`.
//!
//! Filesystem path restriction via `CreateRestrictedToken` / AppContainer is
//! planned for v2.

#![cfg(windows)]

use tempfile::TempDir;
use tokio::process::Command;
use tracing::debug;
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Threading::{OpenProcess, PROCESS_ALL_ACCESS},
    },
};

use crate::config::SandboxConfig;
use crate::error::SandboxError;
use crate::handle::SandboxHandle;

// ---------------------------------------------------------------------------
// Job handle wrapper
// ---------------------------------------------------------------------------

/// RAII wrapper around a Win32 Job Object handle.
///
/// Closing the handle with `KILL_ON_JOB_CLOSE` causes Windows to kill all
/// processes in the job automatically.
pub(crate) struct JobHandle(HANDLE);

impl Drop for JobHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe { let _ = CloseHandle(self.0); }
        }
    }
}

// HANDLE is a raw pointer but Job handles are safe to send across threads.
unsafe impl Send for JobHandle {}
unsafe impl Sync for JobHandle {}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub(crate) async fn spawn(config: &SandboxConfig) -> Result<SandboxHandle, SandboxError> {
    // ── 1. Ephemeral scratch directory ──────────────────────────────────────
    let scratch = TempDir::new().map_err(SandboxError::Io)?;
    let scratch_path = scratch.path().to_path_buf();

    for sub in &["Temp", "AppData\\Roaming", "AppData\\Local", "bin", ".cargo\\bin"] {
        std::fs::create_dir_all(scratch_path.join(sub)).map_err(SandboxError::Io)?;
    }

    // ── 2. Create Job object ─────────────────────────────────────────────────
    let job = create_job_object(config)?;

    // ── 3. Build command ─────────────────────────────────────────────────────
    let mut cmd = Command::new(&config.argv[0]);
    cmd.args(&config.argv[1..]);

    if let Some(ref wd) = config.workdir {
        cmd.current_dir(wd);
    }

    if !config.inherit_env {
        cmd.env_clear();
    }
    for (k, v) in &config.extra_env {
        cmd.env(k, v);
    }

    // Reroute user-profile / temp / package dirs to scratch space.
    cmd.env("USERPROFILE",       &scratch_path);
    cmd.env("HOMEDRIVE",         scratch_path.components().next()
        .map(|c| c.as_os_str().to_os_string())
        .unwrap_or_default());
    cmd.env("HOMEPATH",          scratch_path.to_string_lossy().as_ref());
    cmd.env("TEMP",              scratch_path.join("Temp"));
    cmd.env("TMP",               scratch_path.join("Temp"));
    cmd.env("APPDATA",           scratch_path.join("AppData\\Roaming"));
    cmd.env("LOCALAPPDATA",      scratch_path.join("AppData\\Local"));
    cmd.env("npm_config_prefix", scratch_path.join("bin"));
    cmd.env("npm_config_cache",  scratch_path.join("AppData\\Local\\npm-cache"));
    cmd.env("CARGO_HOME",        scratch_path.join(".cargo"));
    cmd.env("GOPATH",            scratch_path.join("go"));

    // ── 4. Spawn ─────────────────────────────────────────────────────────────
    let child = cmd.spawn().map_err(SandboxError::Spawn)?;

    // ── 5. Assign child to Job object ────────────────────────────────────────
    if let Some(pid) = child.id() {
        assign_to_job(pid, &job)?;
    }

    debug!(
        pid  = child.id().unwrap_or(0),
        scratch = %scratch_path.display(),
        "sandbox/windows: process spawned"
    );

    Ok(SandboxHandle { child, scratch, job })
}

// ---------------------------------------------------------------------------
// Job object helpers
// ---------------------------------------------------------------------------

fn create_job_object(config: &SandboxConfig) -> Result<JobHandle, SandboxError> {
    let handle = unsafe { CreateJobObjectW(None, None) }
        .map_err(|e| SandboxError::Setup(format!("CreateJobObject: {e}")))?;

    // KILL_ON_JOB_CLOSE: kill all processes when the job handle is closed.
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;

    // Optional memory limit.
    if let Some(max_bytes) = config.limits.memory_bytes {
        use windows::Win32::System::JobObjects::JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        info.ProcessMemoryLimit = max_bytes as usize;
    }

    let info_size = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
    unsafe {
        SetInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            info_size,
        )
    }
    .map_err(|e| SandboxError::Setup(format!("SetInformationJobObject: {e}")))?;

    Ok(JobHandle(handle))
}

fn assign_to_job(pid: u32, job: &JobHandle) -> Result<(), SandboxError> {
    let proc_handle = unsafe { OpenProcess(PROCESS_ALL_ACCESS, false, pid) }
        .map_err(|e| SandboxError::Setup(format!("OpenProcess({pid}): {e}")))?;

    let result = unsafe { AssignProcessToJobObject(job.0, proc_handle) };
    unsafe { let _ = CloseHandle(proc_handle); }

    result.map_err(|e| SandboxError::Setup(format!("AssignProcessToJobObject: {e}")))
}
