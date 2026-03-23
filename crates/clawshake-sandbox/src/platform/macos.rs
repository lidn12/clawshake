//! macOS sandbox implementation.
//!
//! Security layers:
//!
//! 1. **Environment rerouting** — `HOME`, `TMPDIR`, and package-manager
//!    prefixes point at an ephemeral [`TempDir`].
//!
//! 2. **Seatbelt (`sandbox_init`)** — an SBPL (Sandbox Profile Language)
//!    profile is applied to the child process via a `pre_exec` hook.  The
//!    profile denies all filesystem access by default and grants explicit
//!    read-only access to standard system directories plus read-write access
//!    to declared [`crate::Mount`] paths and the ephemeral scratch dir.
//!    Network access is controlled by the [`crate::NetworkPolicy`].
//!
//! Full filesystem namespace isolation (APFS snapshots) is planned for v2.

#![cfg(target_os = "macos")]

use std::ffi::CString;
use std::os::unix::process::CommandExt;
use std::path::Path;

use tempfile::TempDir;
use tokio::process::Command;
use tracing::debug;

use crate::config::{NetworkPolicy, SandboxConfig};
use crate::error::SandboxError;
use crate::handle::SandboxHandle;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub(crate) async fn spawn(config: &SandboxConfig) -> Result<SandboxHandle, SandboxError> {
    // ── 1. Ephemeral scratch directory ──────────────────────────────────────
    let scratch = TempDir::new().map_err(SandboxError::Io)?;
    let scratch_path = scratch.path().to_path_buf();

    for sub in &["tmp", ".config", ".cache", "Library/Caches", ".local/bin", ".cargo/bin"] {
        std::fs::create_dir_all(scratch_path.join(sub)).map_err(SandboxError::Io)?;
    }

    // ── 2. Build SBPL profile ────────────────────────────────────────────────
    let profile = build_sbpl_profile(config, &scratch_path);
    let profile_cstr = CString::new(profile)
        .map_err(|e| SandboxError::Setup(format!("SBPL profile contains null byte: {e}")))?;

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

    cmd.env("HOME",             &scratch_path);
    cmd.env("TMPDIR",           scratch_path.join("tmp"));
    cmd.env("XDG_CONFIG_HOME",  scratch_path.join(".config"));
    cmd.env("XDG_CACHE_HOME",   scratch_path.join(".cache"));
    cmd.env("npm_config_prefix", scratch_path.join(".local"));
    cmd.env("npm_config_cache",  scratch_path.join(".cache/npm"));
    cmd.env("CARGO_HOME",        scratch_path.join(".cargo"));
    cmd.env("GOPATH",            scratch_path.join("go"));
    cmd.env("PYTHONUSERBASE",    scratch_path.join(".local"));
    cmd.env("PIP_USER",          "1");

    // ── 4. pre_exec: apply Seatbelt profile in child ─────────────────────────
    //
    // Safety: `sandbox_init` is a documented macOS API safe to call post-fork.
    // `profile_cstr` is an immutable CString captured by value.
    unsafe {
        cmd.pre_exec(move || {
            let mut err_ptr: *mut libc::c_char = std::ptr::null_mut();
            let ret = sandbox_init(
                profile_cstr.as_ptr(),
                SANDBOX_NAMED_EXTERNAL_ONLY_FLAG,
                &mut err_ptr,
            );
            if ret != 0 {
                let msg = if !err_ptr.is_null() {
                    std::ffi::CStr::from_ptr(err_ptr).to_string_lossy().into_owned()
                } else {
                    "unknown sandbox_init error".to_string()
                };
                if !err_ptr.is_null() {
                    sandbox_free_error(err_ptr);
                }
                return Err(std::io::Error::new(std::io::ErrorKind::Other,
                    format!("sandbox_init: {msg}")));
            }
            Ok(())
        });
    }

    // ── 5. Spawn ─────────────────────────────────────────────────────────────
    let child = cmd.spawn().map_err(SandboxError::Spawn)?;

    debug!(
        pid  = child.id().unwrap_or(0),
        scratch = %scratch_path.display(),
        "sandbox/macos: process spawned"
    );

    Ok(SandboxHandle { child, scratch })
}

// ---------------------------------------------------------------------------
// Seatbelt (sandbox_init) FFI
// ---------------------------------------------------------------------------

// Flag: apply profile as a literal SBPL string rather than a named profile.
const SANDBOX_NAMED_EXTERNAL_ONLY_FLAG: u64 = 0;

extern "C" {
    fn sandbox_init(
        profile: *const libc::c_char,
        flags: u64,
        errorbuf: *mut *mut libc::c_char,
    ) -> libc::c_int;

    fn sandbox_free_error(errorbuf: *mut libc::c_char);
}

// ---------------------------------------------------------------------------
// SBPL profile builder
// ---------------------------------------------------------------------------

/// Build a Seatbelt SBPL profile string tailored to `config`.
///
/// Approach: **deny by default**, then allow specific path trees.
/// The profile is compiled and applied in the child by `sandbox_init`.
fn build_sbpl_profile(config: &SandboxConfig, scratch: &Path) -> String {
    let mut rules: Vec<String> = vec![
        "(version 1)".into(),
        "(deny default)".into(),
        // Signal and process management — needed for normal operation.
        "(allow signal (target self))".into(),
        "(allow process-fork)".into(),
        "(allow process-exec*)".into(),
        "(allow mach-lookup)".into(),
        "(allow mach-per-user-lookup)".into(),
        "(allow system-socket)".into(),
        // sysctl reads — many programs call sysctl for hw info.
        "(allow sysctl-read)".into(),
        // /dev entries needed by almost every program.
        r#"(allow file-read* (path "/dev/null") (path "/dev/urandom") (path "/dev/random") (path "/dev/zero"))"#.into(),
        // Standard read-only system trees.
        r#"(allow file-read* (subpath "/usr"))"#.into(),
        r#"(allow file-read* (subpath "/bin"))"#.into(),
        r#"(allow file-read* (subpath "/sbin"))"#.into(),
        r#"(allow file-read* (subpath "/lib"))"#.into(),
        r#"(allow file-read* (subpath "/System"))"#.into(),
        r#"(allow file-read* (subpath "/Library/Frameworks"))"#.into(),
        r#"(allow file-read* (subpath "/private/etc"))"#.into(),
    ];

    // Scratch dir — full read-write.
    rules.push(format!(
        r#"(allow file-read* file-write* (subpath "{}"))"#,
        scratch.to_string_lossy()
    ));

    // /tmp — full read-write.
    rules.push(r#"(allow file-read* file-write* (subpath "/tmp") (subpath "/private/tmp"))"#.into());

    // Declared mounts.
    for mount in &config.mounts {
        let perm = if mount.writable {
            "file-read* file-write*"
        } else {
            "file-read*"
        };
        rules.push(format!(
            r#"(allow {perm} (subpath "{}"))"#,
            mount.host_path.to_string_lossy()
        ));
    }

    // Network.
    match config.network {
        NetworkPolicy::Allow => {
            rules.push("(allow network*)".into());
        }
        NetworkPolicy::OutboundOnly => {
            rules.push("(allow network-outbound)".into());
        }
        NetworkPolicy::None => {
            // Network stays denied (the default).
        }
    }

    rules.join("\n")
}
