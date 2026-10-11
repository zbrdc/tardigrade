//! Opt-in OS self-sandbox for hostile guests (`--sandbox`).
//!
//! Two kernel layers, applied by the host before V8 initializes (V8's worker
//! threads, the watchdog, and the stdin reader must all be created after the
//! restrictions so that they inherit them) and before guest code runs:
//!
//! 1. Landlock denies all filesystem access except the single `--dump` file,
//!    which is created before the restrictions and reopened at end of run.
//! 2. seccomp-BPF denies network, exec, and other dangerous syscalls with
//!    EPERM while allowing everything the runtime needs (threads via
//!    clone/clone3, mmap/mprotect for the JIT, futex, clocks, stdio...).
//!
//! `PR_SET_NO_NEW_PRIVS` is set first. Every setup failure refuses to run
//! (fail closed): the caller must exit without executing guest code.

use std::path::Path;

/// Why the sandbox could not be applied. Any error means: do not run.
#[derive(Debug)]
pub enum SandboxError {
    /// Not Linux: the sandbox layers are Linux-only.
    Unsupported,
    /// V8 is already initialized, so its worker threads would escape Landlock.
    EngineStarted,
    /// Threads already exist: Landlock would restrict only the calling thread.
    ThreadsActive(usize),
    /// The `--dump` file cannot be prepared before the restrictions apply.
    Dump(std::path::PathBuf, std::io::Error),
    /// The kernel lacks Landlock or seccomp, or a setup step failed.
    Kernel(String),
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::Unsupported => write!(f, "--sandbox is only supported on Linux"),
            SandboxError::EngineStarted => {
                write!(f, "V8 is already initialized; apply the sandbox before the first run()/serve()")
            }
            SandboxError::ThreadsActive(n) => {
                write!(f, "{} threads already exist; apply the sandbox before any thread", n)
            }
            SandboxError::Dump(path, e) => write!(f, "cannot prepare dump {:?}: {}", path, e),
            SandboxError::Kernel(msg) => write!(f, "kernel refused the restrictions: {}", msg),
        }
    }
}

impl std::error::Error for SandboxError {}

/// Restrict this process: Landlock (filesystem) plus seccomp (syscalls).
///
/// `dump`, if set, is the only filesystem write the run may perform later:
/// the file is created (or truncated) now and the end-of-run snapshot reopens
/// it under a Landlock rule attached to that single file. Must be called
/// before the first [`crate::run`]/[`crate::serve`] and before any thread
/// exists; both are checked and refuse instead of partially restricting.
pub fn apply(dump: Option<&Path>) -> Result<(), SandboxError> {
    imp::apply(dump)
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::SandboxError;
    use std::path::Path;

    pub fn apply(_dump: Option<&Path>) -> Result<(), SandboxError> {
        Err(SandboxError::Unsupported)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::SandboxError;
    use std::path::Path;

    pub fn apply(dump: Option<&Path>) -> Result<(), SandboxError> {
        // Order matters: refuse late application before doing any work, then
        // restrict from the broadest layer (filesystem) to the most
        // irreversible (seccomp), so each step runs outside the previous one.
        if crate::engine_started() {
            return Err(SandboxError::EngineStarted);
        }
        let threads = thread_count()?;
        if threads != 1 {
            return Err(SandboxError::ThreadsActive(threads));
        }
        prepare_dump(dump)?;
        set_no_new_privs()?;
        landlock_restrict(dump)?;
        seccomp_restrict()?;
        Ok(())
    }

    /// Threads created after this call inherit both layers, so there must be
    /// no thread yet: Landlock cannot be applied to an existing thread.
    fn thread_count() -> Result<usize, SandboxError> {
        std::fs::read_dir("/proc/self/task")
            .map(|entries| entries.count())
            .map_err(|e| kernel(format!("cannot read /proc/self/task: {}", e)))
    }

    /// The dump file must exist before Landlock denies creation: the rule is
    /// attached to this one file, which is tighter than allowing its whole
    /// directory.
    fn prepare_dump(dump: Option<&Path>) -> Result<(), SandboxError> {
        match dump {
            Some(path) => std::fs::File::create(path)
                .map(|_| ())
                .map_err(|e| SandboxError::Dump(path.to_path_buf(), e)),
            None => Ok(()),
        }
    }

    fn set_no_new_privs() -> Result<(), SandboxError> {
        let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
        if rc != 0 {
            return Err(kernel(format!(
                "prctl(PR_SET_NO_NEW_PRIVS): {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    fn kernel(msg: impl std::fmt::Display) -> SandboxError {
        SandboxError::Kernel(msg.to_string())
    }

    fn landlock_restrict(dump: Option<&Path>) -> Result<(), SandboxError> {
        use landlock::{
            make_bitflags, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr,
            RulesetCreatedAttr, RulesetStatus, ABI,
        };
        // ABI::V5 covers every filesystem right through IoctlDev; rights of
        // newer ABIs are additionally covered by the seccomp deny list.
        let mut ruleset = Ruleset::default()
            .handle_access(AccessFs::from_all(ABI::V5))
            .map_err(kernel)?
            .create()
            .map_err(kernel)?;
        if let Some(path) = dump {
            let fd = PathFd::new(path).map_err(kernel)?;
            let access = make_bitflags!(AccessFs::{WriteFile | Truncate});
            ruleset = ruleset.add_rule(PathBeneath::new(fd, access)).map_err(kernel)?;
        }
        let status = ruleset.restrict_self().map_err(kernel)?;
        if status.ruleset == RulesetStatus::NotEnforced {
            return Err(kernel("this kernel does not enforce Landlock"));
        }
        Ok(())
    }

    fn seccomp_restrict() -> Result<(), SandboxError> {
        use seccompiler::{apply_filter_all_threads, BpfProgram, SeccompAction, SeccompFilter};
        use std::convert::TryInto;
        // Deny list (default allow, denied calls fail with EPERM): V8's needs
        // are broad and host-specific, so an allow list proven on one kernel
        // breaks on the next. Nothing in v8vm's run path calls these after
        // the sandbox is applied. TSYNC covers any thread that slipped past
        // the single-thread check.
        let rules = DENIED
            .iter()
            .chain(DENIED_X86)
            .map(|n| (*n, Vec::new()))
            .collect();
        let filter: BpfProgram = SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::Errno(libc::EPERM as u32),
            std::env::consts::ARCH
                .try_into()
                .map_err(|_| kernel("unsupported architecture for seccomp"))?,
        )
        .map_err(kernel)?
        .try_into()
        .map_err(kernel)?;
        apply_filter_all_threads(&filter).map_err(kernel)
    }

    // Syscalls denied with EPERM, grouped by what they would let escaped
    // guest code do. clone/clone3 stay allowed because V8's worker threads,
    // the watchdog, and the stdin reader are created after this filter is
    // installed (a known gap: shellcode can clone a process-like thread).
    const DENIED: &[i64] = &[
        // network
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_connect,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept,
        libc::SYS_accept4,
        libc::SYS_sendto,
        libc::SYS_recvfrom,
        libc::SYS_sendmsg,
        libc::SYS_recvmsg,
        libc::SYS_sendmmsg,
        libc::SYS_recvmmsg,
        libc::SYS_shutdown,
        libc::SYS_getsockname,
        libc::SYS_getpeername,
        libc::SYS_setsockopt,
        libc::SYS_getsockopt,
        // exec
        libc::SYS_execve,
        libc::SYS_execveat,
        // cross-process access
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_kcmp,
        libc::SYS_pidfd_open,
        libc::SYS_pidfd_getfd,
        libc::SYS_pidfd_send_signal,
        // privilege and identity
        libc::SYS_setuid,
        libc::SYS_setgid,
        libc::SYS_setreuid,
        libc::SYS_setregid,
        libc::SYS_setresuid,
        libc::SYS_setresgid,
        libc::SYS_setfsuid,
        libc::SYS_setfsgid,
        libc::SYS_capset,
        // namespaces and mounts
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_fsopen,
        libc::SYS_fsconfig,
        libc::SYS_fsmount,
        libc::SYS_move_mount,
        libc::SYS_open_tree,
        libc::SYS_mount_setattr,
        libc::SYS_fspick,
        // kernel attack surface
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_kexec_load,
        libc::SYS_kexec_file_load,
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_acct,
        libc::SYS_sethostname,
        libc::SYS_setdomainname,
        libc::SYS_syslog,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_seccomp,
        libc::SYS_memfd_create,
        // keyring
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        // file-handle escapes
        libc::SYS_open_by_handle_at,
        libc::SYS_name_to_handle_at,
        // clock and time setting
        libc::SYS_clock_settime,
        libc::SYS_clock_adjtime,
        libc::SYS_adjtimex,
        libc::SYS_settimeofday,
        // metadata mutation (Landlock covers open/create/remove, not setattr)
        libc::SYS_chmod,
        libc::SYS_fchmod,
        libc::SYS_fchmodat,
        libc::SYS_chown,
        libc::SYS_fchown,
        libc::SYS_lchown,
        libc::SYS_fchownat,
        libc::SYS_setxattr,
        libc::SYS_lsetxattr,
        libc::SYS_fsetxattr,
        libc::SYS_removexattr,
        libc::SYS_lremovexattr,
        libc::SYS_fremovexattr,
        libc::SYS_utime,
        libc::SYS_utimes,
        libc::SYS_futimesat,
        libc::SYS_utimensat,
        // fd-to-fd plumbing
        libc::SYS_sendfile,
        libc::SYS_splice,
        libc::SYS_vmsplice,
        libc::SYS_tee,
        libc::SYS_copy_file_range,
    ];

    // Legacy syscalls with no equivalent on the generic syscall ABI.
    // create_module/query_module/get_kernel_syms are not listed: modern libc
    // dropped them and the kernel removed the syscalls long ago.
    #[cfg(target_arch = "x86_64")]
    const DENIED_X86: &[i64] = &[
        libc::SYS_fork,
        libc::SYS_vfork,
        libc::SYS_ioperm,
        libc::SYS_iopl,
        libc::SYS_uselib,
    ];

    #[cfg(not(target_arch = "x86_64"))]
    const DENIED_X86: &[i64] = &[];
}
