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
use std::path::Path;

use landlock::{
    Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI,
};
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

    for sub in &[
        "tmp",
        ".config",
        ".cache",
        ".local/bin",
        ".local/share",
        ".cargo/bin",
    ] {
        std::fs::create_dir_all(scratch_path.join(sub)).map_err(SandboxError::Io)?;
    }

    // ── 2. Landlock ruleset (built in parent) ────────────────────────────────
    // Returns None if the kernel does not support Landlock.
    let ll_ruleset = build_landlock_ruleset(config, &scratch_path)?;

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
    cmd.env("HOME", &scratch_path);
    cmd.env("TMPDIR", scratch_path.join("tmp"));
    cmd.env("XDG_CONFIG_HOME", scratch_path.join(".config"));
    cmd.env("XDG_CACHE_HOME", scratch_path.join(".cache"));
    cmd.env("XDG_DATA_HOME", scratch_path.join(".local/share"));
    cmd.env("npm_config_prefix", scratch_path.join(".local"));
    cmd.env("npm_config_cache", scratch_path.join(".cache/npm"));
    cmd.env("CARGO_HOME", scratch_path.join(".cargo"));
    cmd.env("GOPATH", scratch_path.join("go"));
    cmd.env("PYTHONUSERBASE", scratch_path.join(".local"));
    cmd.env("PIP_USER", "1");

    // ── 5. pre_exec: apply Landlock + seccomp in child ───────────────────────
    //
    // Safety: This closure runs in the child process after fork() but before
    // exec().  landlock::RulesetCreated::restrict_self() and
    // seccompiler::apply_filter() only perform syscalls (prctl,
    // landlock_restrict_self, prctl/seccomp) and are async-signal-safe.
    // The RulesetCreated is moved into the closure; after fork the child owns
    // a copy of the underlying fd and applies the ruleset via restrict_self.
    unsafe {
        cmd.pre_exec(move || {
            // Apply Landlock (soft-fail if kernel is too old).
            if let Some(ruleset) = ll_ruleset {
                // restrict_self() calls prctl(PR_SET_NO_NEW_PRIVS) + syscall.
                // Errors are silently ignored — we degrade rather than block.
                let _ = ruleset.restrict_self();
            }

            // Apply seccomp filter.
            seccompiler::apply_filter(&seccomp_prog)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

            Ok(())
        });
    }

    // ── 6. Spawn ─────────────────────────────────────────────────────────────
    let child = cmd.spawn().map_err(SandboxError::Spawn)?;

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

/// Build a Landlock ruleset and return it, or `None` if Landlock is unsupported.
fn build_landlock_ruleset(
    config: &SandboxConfig,
    scratch: &Path,
) -> Result<Option<landlock::RulesetCreated>, SandboxError> {
    let abi = ABI::V3;

    let base = Ruleset::default().handle_access(AccessFs::from_all(abi));
    let base = match base {
        Ok(b) => b,
        Err(e) => {
            warn!(
                "sandbox/linux: landlock not supported by kernel ({e}); path restriction disabled"
            );
            return Ok(None);
        }
    };

    let created = match base.create() {
        Ok(c) => c,
        Err(e) => {
            warn!("sandbox/linux: landlock ruleset create failed ({e}); path restriction disabled");
            return Ok(None);
        }
    };

    let read_only = AccessFs::from_read(abi);
    let read_write = AccessFs::from_all(abi);

    // In landlock 0.4.x, add_rule() takes ownership of `self` and does not
    // return the ruleset on error.  We thread it through a fallible closure
    // using `?`; any failure soft-disables path restriction entirely.
    let r: Result<landlock::RulesetCreated, Box<dyn std::error::Error>> = (|| {
        let mut c = created;

        // Standard read-only system directories.
        for dir in &[
            "/usr",
            "/lib",
            "/lib64",
            "/lib32",
            "/libx32",
            "/bin",
            "/sbin",
            "/etc/alternatives",
            "/etc/ld.so.cache",
            "/proc/self",
            "/dev/null",
            "/dev/urandom",
            "/dev/zero",
        ] {
            if Path::new(dir).exists() {
                if let Ok(fd) = PathFd::new(dir) {
                    c = c
                        .add_rule(PathBeneath::new(fd, read_only))
                        .map_err(|e| format!("add_rule({dir}): {e:?}"))?;
                }
            }
        }

        // Full access to /tmp and the scratch dir.
        for dir in [Path::new("/tmp"), scratch] {
            if let Ok(fd) = PathFd::new(dir) {
                c = c
                    .add_rule(PathBeneath::new(fd, read_write))
                    .map_err(|e| format!("add_rule(scratch): {e:?}"))?;
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
                let access = if mount.writable {
                    read_write
                } else {
                    read_only
                };
                c = c
                    .add_rule(PathBeneath::new(fd, access))
                    .map_err(|e| format!("add_rule(mount): {e:?}"))?;
            }
        }

        Ok(c)
    })();

    let c = match r {
        Ok(c) => c,
        Err(e) => {
            warn!("sandbox/linux: landlock ruleset build failed ({e}); path restriction disabled");
            return Ok(None);
        }
    };

    Ok(Some(c))
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

    // In seccompiler 0.4.0, per-rule actions were removed.  To block a syscall
    // unconditionally we insert an empty rule vec (no conditions → no match →
    // `mismatch_action` fires), then pass `deny` as the `mismatch_action` to
    // SeccompFilter::new.  Syscalls not in the map still get `default_action`
    // (Allow).
    macro_rules! deny_all {
        ($( $sc:expr ),* $(,)?) => {
            $( rules.insert($sc, vec![]); )*
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

    // seccompiler 0.4.0: SeccompFilter::new takes 4 args:
    //   (rules, default_action, mismatch_action, target_arch)
    // default_action → Allow (syscalls NOT in map)
    // mismatch_action → deny (syscalls IN map but empty rule vec = no match)
    let filter = SeccompFilter::new(rules, SeccompAction::Allow, deny, current_target_arch())?;

    Ok(filter.try_into()?)
}

fn current_target_arch() -> TargetArch {
    #[cfg(target_arch = "x86_64")]
    return TargetArch::x86_64;
    #[cfg(target_arch = "aarch64")]
    return TargetArch::aarch64;
    #[cfg(target_arch = "arm")]
    return TargetArch::arm;
    #[allow(unreachable_code)]
    panic!("sandbox/linux: unsupported CPU architecture for seccomp")
}
