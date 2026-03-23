//! Linux sandbox implementation.
//!
//! Security layers applied (in order, from weakest to strongest):
//!
//! 1. **Environment rerouting** — `HOME`, `TMPDIR`, package-manager prefixes
//!    all point at an ephemeral [`TempDir`] so tool-install writes land in a
//!    directory that is deleted on cleanup.
//!
//! 2. **seccomp BPF denylist** — compiled in the parent via `seccompiler`,
//!    installed in the child via `pre_exec` before `exec(2)`.  Blocks kernel-
//!    escape and namespace-escape syscalls; optionally blocks `socket(2)` for
//!    network isolation.
//!
//! 3. **Landlock LSM path restriction** — an allowlist of accessible
//!    filesystem paths.  Built in the parent (file-descriptor based), applied
//!    in the child via a raw `landlock_restrict_self(2)` syscall inside
//!    `pre_exec`.  Gracefully degrades on kernels older than 5.13.
//!
//! Mount namespaces / overlayfs (full write isolation) are planned for v2.

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::mem::ManuallyDrop;
use std::os::unix::io::{AsFd, AsRawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;

use landlock::{ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr};
use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, SeccompRule, TargetArch};
use tempfile::TempDir;
use tokio::process::Command;
use tracing::{debug, warn};

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

    for sub in &["tmp", ".config", ".cache", ".local/bin", ".local/share", ".cargo/bin"] {
        std::fs::create_dir_all(scratch_path.join(sub)).map_err(SandboxError::Io)?;
    }

    // ── 2. Landlock ruleset (built in parent) ────────────────────────────────
    let ll_fd = build_landlock_ruleset(config, &scratch_path)?;

    // ── 3. Seccomp BPF filter (compiled in parent) ───────────────────────────
    let seccomp_prog = build_seccomp_filter(&config.network)
        .map_err(|e| SandboxError::Setup(format!("seccomp compilation: {e}")))?;

    // ── 4. Build command ─────────────────────────────────────────────────────
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

    // Reroute home / temp / package-manager dirs into the scratch space.
    cmd.env("HOME",             &scratch_path);
    cmd.env("TMPDIR",           scratch_path.join("tmp"));
    cmd.env("XDG_CONFIG_HOME",  scratch_path.join(".config"));
    cmd.env("XDG_CACHE_HOME",   scratch_path.join(".cache"));
    cmd.env("XDG_DATA_HOME",    scratch_path.join(".local/share"));
    cmd.env("npm_config_prefix", scratch_path.join(".local"));
    cmd.env("npm_config_cache",  scratch_path.join(".cache/npm"));
    cmd.env("CARGO_HOME",        scratch_path.join(".cargo"));
    cmd.env("GOPATH",            scratch_path.join("go"));
    cmd.env("PYTHONUSERBASE",    scratch_path.join(".local"));
    cmd.env("PIP_USER",          "1");

    // ── 5. pre_exec: apply Landlock + seccomp in child ───────────────────────
    //
    // Safety: This closure runs in the child process after fork() but before
    // exec().  Only async-signal-safe operations (syscalls) are performed here.
    // The captured `seccomp_prog` Vec is read-only; the raw fd `ll_fd` was
    // obtained in the parent and is duplicated into the child by fork().
    unsafe {
        cmd.pre_exec(move || {
            // Apply Landlock (soft-fail if kernel is too old).
            if ll_fd >= 0 {
                let ret = libc::syscall(
                    libc::SYS_landlock_restrict_self,
                    ll_fd as libc::c_long,
                    0i64,
                );
                if ret != 0 {
                    let err = *libc::__errno_location();
                    if err != libc::ENOSYS && err != libc::EOPNOTSUPP {
                        return Err(std::io::Error::from_raw_os_error(err));
                    }
                }
                // No longer needed in the child after restrict_self.
                libc::close(ll_fd);
            }

            // Apply seccomp filter.
            seccompiler::apply_filter(&seccomp_prog)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

            Ok(())
        });
    }

    // ── 6. Spawn ─────────────────────────────────────────────────────────────
    let child = cmd.spawn().map_err(SandboxError::Spawn)?;

    // Close the landlock fd in the parent now that fork has happened.
    if ll_fd >= 0 {
        unsafe { libc::close(ll_fd); }
    }

    debug!(
        pid  = child.id().unwrap_or(0),
        scratch = %scratch_path.display(),
        "sandbox/linux: process spawned"
    );

    Ok(SandboxHandle { child, scratch })
}

// ---------------------------------------------------------------------------
// Landlock helpers
// ---------------------------------------------------------------------------

/// Build a Landlock ruleset and return the raw fd, or -1 if unsupported.
///
/// The returned fd is **not** owned by Rust's drop machinery — the caller
/// must close it manually in both parent and child.
fn build_landlock_ruleset(config: &SandboxConfig, scratch: &Path) -> Result<i32, SandboxError> {
    let abi = ABI::V3;

    let base = Ruleset::default().handle_access(AccessFs::from_all(abi));
    let base = match base {
        Ok(b)  => b,
        Err(e) => {
            warn!("sandbox/linux: landlock not supported by kernel ({e}); path restriction disabled");
            return Ok(-1);
        }
    };

    let created = match base.create() {
        Ok(c)  => c,
        Err(e) => {
            warn!("sandbox/linux: landlock ruleset create failed ({e}); path restriction disabled");
            return Ok(-1);
        }
    };

    let read_only = AccessFs::from_read(abi);
    let read_write = AccessFs::from_all(abi);

    // Helper closure — adds a rule, logging soft failures.
    fn add<R>(mut created: landlock::RulesetCreated, rule: R) -> landlock::RulesetCreated
    where
        R: landlock::Rule<landlock::AccessFs>,
    {
        match created.add_rule(rule) {
            Ok(c)  => c,
            Err(e) => {
                warn!("sandbox/linux: landlock add_rule failed ({e}); skipping path");
                created
            }
        }
    }

    let mut c = created;

    // Standard read-only system directories.
    for dir in &[
        "/usr", "/lib", "/lib64", "/lib32", "/libx32",
        "/bin", "/sbin", "/etc/alternatives", "/etc/ld.so.cache",
        "/proc/self", "/dev/null", "/dev/urandom", "/dev/zero",
    ] {
        if Path::new(dir).exists() {
            if let Ok(fd) = PathFd::new(dir) {
                c = add(c, PathBeneath::new(fd, read_only));
            }
        }
    }

    // Full access to /tmp and the scratch dir.
    for dir in &["/tmp", scratch] {
        if let Ok(fd) = PathFd::new(dir) {
            c = add(c, PathBeneath::new(fd, read_write));
        }
    }

    // Declared mounts.
    for mount in &config.mounts {
        if !mount.host_path.exists() {
            warn!(
                path = %mount.host_path.display(),
                "sandbox/linux: mount host_path not found; skipping"
            );
            continue;
        }
        if let Ok(fd) = PathFd::new(&mount.host_path) {
            let access = if mount.writable { read_write } else { read_only };
            c = add(c, PathBeneath::new(fd, access));
        }
    }

    // Leak the fd out of Rust's ownership so we can pass the raw integer.
    let raw = c.as_fd().as_raw_fd();
    let _ = ManuallyDrop::new(c);
    Ok(raw)
}

// ---------------------------------------------------------------------------
// Seccomp helpers
// ---------------------------------------------------------------------------

/// Compile a seccomp BPF **denylist** filter (default action = Allow).
///
/// Blocking a small set of dangerous syscalls is safer for a general-purpose
/// sandbox than maintaining an allowlist, which is brittle and breaks arbitrary
/// programs.
fn build_seccomp_filter(network: &NetworkPolicy) -> Result<BpfProgram, Box<dyn std::error::Error>> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    let deny = SeccompAction::Errno(libc::EPERM as u32);

    macro_rules! deny_all {
        ($( $sc:expr ),* $(,)?) => {
            $( rules.insert($sc, vec![SeccompRule::new(vec![], deny.clone())]); )*
        };
    }

    // Kernel takeover / privilege escalation.
    deny_all!(
        libc::SYS_kexec_load,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_perf_event_open,
        libc::SYS_add_key,
        libc::SYS_keyctl,
        libc::SYS_request_key
    );

    // kexec_file_load exists only on x86-64 and arm64.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    deny_all!(libc::SYS_kexec_file_load);

    // Filesystem / namespace escape.
    deny_all!(
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_unshare
    );

    // Network — block if policy is None.
    if *network == NetworkPolicy::None {
        deny_all!(libc::SYS_socket);
    }

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        current_target_arch(),
    )?;

    Ok(filter.try_into()?)
}

fn current_target_arch() -> TargetArch {
    #[cfg(target_arch = "x86_64")]  return TargetArch::x86_64;
    #[cfg(target_arch = "aarch64")] return TargetArch::aarch64;
    #[cfg(target_arch = "arm")]     return TargetArch::arm;
    #[allow(unreachable_code)]
    panic!("sandbox/linux: unsupported CPU architecture for seccomp")
}
