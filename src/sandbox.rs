//! Opt-in OS self-sandbox for hostile guests (`--sandbox`).
//!
//! Layers, applied by the host after it reads the entry source and the
//! preload file and before V8 initializes (V8's worker pool, the watchdog,
//! and the stdin reader are threads created later and must inherit every
//! restriction) and before guest code runs:
//!
//! 1. Landlock denies all filesystem access except the single `--dump` file
//!    and a read-only rule for /dev/urandom (V8 opens it while seeding).
//! 2. seccomp-BPF denies network, exec, process creation, and other
//!    dangerous syscalls with EPERM while allowing what the runtime needs
//!    (threads, mmap/mprotect for the JIT, futex, clocks, stdio). clone is
//!    argument-filtered, clone3 fails with ENOSYS so glibc falls back to
//!    clone, and a hand-written stacked filter kills the process on any
//!    non-x86_64 arch or x32-range syscall number.
//!
//! `PR_SET_NO_NEW_PRIVS` is set first and inherited descriptors >= 3 are
//! closed. Every setup failure refuses to run (fail closed), including a
//! Landlock ruleset that is not fully enforced (kernel below ABI 5).

use std::path::Path;

/// Why the sandbox could not be applied. Any error means: do not run.
#[derive(Debug)]
pub enum SandboxError {
    /// Not Linux x86_64: the only platform the layers are verified on.
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
            SandboxError::Unsupported => write!(f, "--sandbox is only supported on Linux x86_64"),
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
/// the file is created now without truncation and the end-of-run snapshot
/// reopens that inode under a Landlock rule anchored to the open descriptor.
/// Must be called before the first [`crate::run`]/[`crate::serve`] and
/// before any thread exists; both are checked and refuse instead of
/// restricting only one thread.
pub fn apply(dump: Option<&Path>) -> Result<(), SandboxError> {
    imp::apply(dump, None)
}

/// Which kernel layer [`apply_layer`] installs.
///
/// Hidden because the product always applies both layers: the tests need
/// each layer alone to attribute a denial to the right one (execve, for
/// example, is denied by seccomp with EPERM but also by Landlock's Execute
/// right with EACCES, so only a seccomp-only run proves the seccomp rule).
#[doc(hidden)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Landlock,
    Seccomp,
}

/// Apply a single sandbox layer. Test-only; see [`Layer`].
#[doc(hidden)]
pub fn apply_layer(layer: Layer, dump: Option<&Path>) -> Result<(), SandboxError> {
    imp::apply(dump, Some(layer))
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
mod imp {
    use super::{Layer, SandboxError};
    use std::path::Path;

    pub fn apply(_dump: Option<&Path>, _layer: Option<Layer>) -> Result<(), SandboxError> {
        Err(SandboxError::Unsupported)
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod imp {
    use super::{Layer, SandboxError};
    use seccompiler::{
        apply_filter_all_threads, BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp,
        SeccompCondition, SeccompFilter, SeccompRule,
    };
    use std::collections::BTreeMap;
    use std::convert::TryInto;
    use std::path::Path;

    /// Keyed by `c_long`, which is the syscall-number type (and `i64`
    /// here, the only arch this module builds for).
    type Rules = BTreeMap<libc::c_long, Vec<SeccompRule>>;

    pub fn apply(dump: Option<&Path>, layer: Option<Layer>) -> Result<(), SandboxError> {
        // Refuse late application before doing any work, then restrict from
        // the broadest layer (filesystem) to the most irreversible (seccomp).
        if crate::engine_started() {
            return Err(SandboxError::EngineStarted);
        }
        let threads = thread_count()?;
        if threads != 1 {
            return Err(SandboxError::ThreadsActive(threads));
        }
        // Close inherited descriptors before opening the dump anchor, or
        // close_range would close it again.
        close_inherited_fds()?;
        let dump_file = prepare_dump(dump)?;
        set_no_new_privs()?;
        if layer != Some(Layer::Seccomp) {
            landlock_restrict(dump_file.as_ref())?;
        }
        if layer != Some(Layer::Landlock) {
            seccomp_restrict()?;
        }
        Ok(())
    }

    /// Threads created after this call inherit both layers, so there must be
    /// no thread yet: Landlock cannot be applied to an existing thread.
    fn thread_count() -> Result<usize, SandboxError> {
        std::fs::read_dir("/proc/self/task")
            .map(|entries| entries.count())
            .map_err(|e| kernel(format!("cannot read /proc/self/task: {}", e)))
    }

    /// The dump file must exist before Landlock denies creation, and the
    /// rule is anchored to its inode, so a later path swap cannot redirect
    /// the dump write elsewhere. Created without O_TRUNC: a run that ends
    /// before the dump leaves an existing file untouched.
    ///
    /// The kernel only accepts an O_PATH descriptor (or a directory) as a
    /// PathBeneath parent — a plain write handle gets EBADFD — so the rule
    /// anchor is an O_PATH open of the same path, checked to be the same
    /// inode as the write handle so the two opens cannot be split.
    fn prepare_dump(dump: Option<&Path>) -> Result<Option<landlock::PathFd>, SandboxError> {
        dump.map(|path| {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
                .map_err(|e| SandboxError::Dump(path.to_path_buf(), e))?;
            let anchor = landlock::PathFd::new(path).map_err(kernel)?;
            match (inode_of(&file), inode_of(&anchor)) {
                (Some(a), Some(b)) if a == b => Ok(anchor),
                _ => Err(kernel("dump path changed while preparing the sandbox")),
            }
        })
        .transpose()
    }

    fn inode_of(fd: impl std::os::fd::AsFd) -> Option<(u64, u64)> {
        use std::os::fd::AsRawFd;
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::fstat(fd.as_fd().as_raw_fd(), &mut stat) };
        (rc == 0).then_some((stat.st_dev, stat.st_ino))
    }

    /// Anything the caller's environment passed in (a socket, a log file) is
    /// a channel out of the sandbox; the run needs only stdio.
    fn close_inherited_fds() -> Result<(), SandboxError> {
        let rc = unsafe { libc::close_range(3, u32::MAX, 0) };
        if rc != 0 {
            return Err(kernel(format!("close_range: {}", std::io::Error::last_os_error())));
        }
        Ok(())
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

    fn landlock_restrict(dump: Option<&landlock::PathFd>) -> Result<(), SandboxError> {
        use landlock::{
            make_bitflags, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr,
            RulesetCreatedAttr, ABI,
        };
        // ABI::V5 covers every filesystem right through IoctlDev. Anything
        // less must refuse instead of silently dropping rights.
        let mut ruleset = Ruleset::default()
            .handle_access(AccessFs::from_all(ABI::V5))
            .map_err(kernel)?
            .create()
            .map_err(kernel)?;
        // V8 opens /dev/urandom while seeding RNGs (Math.random and hashing);
        // without this rule that open fails and the seed falls back to
        // weaker sources. Read-only, and this one file only.
        let urandom = PathFd::new("/dev/urandom").map_err(kernel)?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(urandom, AccessFs::ReadFile))
            .map_err(kernel)?;
        if let Some(anchor) = dump {
            let access = make_bitflags!(AccessFs::{WriteFile | Truncate});
            ruleset = ruleset.add_rule(PathBeneath::new(anchor, access)).map_err(kernel)?;
        }
        let status = ruleset.restrict_self().map_err(kernel)?;
        enforce_status(status.ruleset)
    }

    /// Landlock must be fully enforced: a partially enforced ruleset means
    /// the kernel dropped handled rights (Landlock ABI below 5), which is a
    /// silent gap rather than a sandbox, so it is refused like a missing one.
    fn enforce_status(status: landlock::RulesetStatus) -> Result<(), SandboxError> {
        match status {
            landlock::RulesetStatus::FullyEnforced => Ok(()),
            landlock::RulesetStatus::PartiallyEnforced | landlock::RulesetStatus::NotEnforced => {
                Err(kernel("Landlock is not fully enforced (needs ABI 5, Linux 6.10)"))
            }
        }
    }

    fn seccomp_restrict() -> Result<(), SandboxError> {
        // Order matters: the deny list refuses SYS_seccomp, so it must be the
        // last filter installed. clone3 gets ENOSYS (not EPERM) because
        // glibc's pthread_create falls back to clone only on ENOSYS; the
        // fallback is then argument-checked by the deny list.
        let nosys = compile(single(libc::SYS_clone3), libc::ENOSYS)?;
        apply_filter_all_threads(&nosys).map_err(kernel)?;
        install_x32_guard()?;
        let deny = compile(filter_rules(), libc::EPERM)?;
        apply_filter_all_threads(&deny).map_err(kernel)
    }

    /// Compile a default-allow filter whose matched syscalls return `errno`.
    fn compile(rules: Rules, errno: libc::c_int) -> Result<BpfProgram, SandboxError> {
        SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::Errno(errno as u32),
            // The module is x86_64-only; the arch token type is not exported
            // from the crate root, so it is inferred here.
            std::env::consts::ARCH.try_into().map_err(|_| kernel("unsupported architecture"))?,
        )
        .map_err(kernel)?
        .try_into()
        .map_err(kernel)
    }

    fn single(nr: libc::c_long) -> Rules {
        [(nr, Vec::new())].into_iter().collect()
    }

    /// The deny list proper: each listed syscall fails with EPERM whatever
    /// its arguments. clone, ioctl and the signal syscalls need argument
    /// rules instead and are added on top.
    fn filter_rules() -> Rules {
        let mut rules: Rules = DENIED.iter().map(|nr| (*nr, Vec::new())).collect();
        rules.insert(libc::SYS_clone, clone_rules());
        rules.insert(libc::SYS_ioctl, ioctl_rules());
        let pid = unsafe { libc::getpid() } as u64;
        // Signals may only target this process (raise/abort paths); another
        // process or a whole group (pid 0 or -pgid) is denied.
        rules.insert(libc::SYS_kill, vec![condition(0, SeccompCmpOp::Ne, pid)]);
        rules.insert(libc::SYS_tgkill, vec![condition(0, SeccompCmpOp::Ne, pid)]);
        // tkill carries no thread-group id to check: deny outright.
        rules.insert(libc::SYS_tkill, Vec::new());
        rules
    }

    /// clone is allowed only for real threads of this process: CLONE_THREAD
    /// set and no namespace-creating flag. seccompiler rules are or-bound, so
    /// one rule per denied shape.
    fn clone_rules() -> Vec<SeccompRule> {
        let mut rules = vec![masked(libc::CLONE_THREAD as u64, 0)];
        for flag in NAMESPACE_FLAGS {
            rules.push(masked(flag, flag));
        }
        rules
    }

    /// Matches when `(flags & mask) == value` on clone's first argument.
    fn masked(mask: u64, value: u64) -> SeccompRule {
        rule(0, SeccompCmpArgLen::Qword, SeccompCmpOp::MaskedEq(mask), value)
    }

    /// ioctl is denied only for the terminal input injection requests; the
    /// rest (isatty, window size on already-open fds) stays available.
    fn ioctl_rules() -> Vec<SeccompRule> {
        vec![
            condition(1, SeccompCmpOp::Eq, libc::TIOCSTI),
            condition(1, SeccompCmpOp::Eq, libc::TIOCLINUX),
        ]
    }

    fn condition(arg: usize, op: SeccompCmpOp, value: u64) -> SeccompRule {
        rule(arg as u8, SeccompCmpArgLen::Dword, op, value)
    }

    fn rule(arg: u8, len: SeccompCmpArgLen, op: SeccompCmpOp, value: u64) -> SeccompRule {
        // Constructors only reject an argument index above 5.
        SeccompRule::new(vec![SeccompCondition::new(arg, len, op, value).expect("valid condition")])
            .expect("valid rule")
    }

    /// Namespace-creating clone flags (see clone(2)); each gets its own rule.
    const NAMESPACE_FLAGS: [u64; 8] = [
        libc::CLONE_NEWNS as u64,
        libc::CLONE_NEWCGROUP as u64,
        libc::CLONE_NEWUTS as u64,
        libc::CLONE_NEWIPC as u64,
        libc::CLONE_NEWUSER as u64,
        libc::CLONE_NEWPID as u64,
        libc::CLONE_NEWNET as u64,
        libc::CLONE_NEWTIME as u64,
    ];

    /// AUDIT_ARCH_X86_64 from linux/audit.h (EM_X86_64 | 64-bit | little
    /// endian). libc 0.2.190 does not export the AUDIT_ARCH_* set.
    const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
    /// Syscall numbers at or above this are the x32 ABI range on x86_64.
    const X32_BIT: u32 = 0x4000_0000;

    /// A stacked filter closing the x32 hole: seccompiler matches syscall
    /// numbers exactly, so a call made through the x32 ABI (nr | X32_BIT)
    /// would skip every deny rule, and an int 0x80 call would arrive with a
    /// different arch. Both are kills: nothing in a v8vm run uses another
    /// syscall ABI.
    fn x32_guard() -> Vec<libc::sock_filter> {
        const LD_W: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
        const JEQ: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
        const JGE: u16 = (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16;
        const RET: u16 = (libc::BPF_RET | libc::BPF_K) as u16;
        // seccomp_data: nr at offset 0, arch at offset 4. Jump offsets are
        // relative to the following instruction; index 5 is the KILL return.
        vec![
            libc::sock_filter { code: LD_W, jt: 0, jf: 0, k: 4 },
            libc::sock_filter { code: JEQ, jt: 0, jf: 3, k: AUDIT_ARCH_X86_64 },
            libc::sock_filter { code: LD_W, jt: 0, jf: 0, k: 0 },
            libc::sock_filter { code: JGE, jt: 1, jf: 0, k: X32_BIT },
            libc::sock_filter { code: RET, jt: 0, jf: 0, k: libc::SECCOMP_RET_ALLOW },
            libc::sock_filter { code: RET, jt: 0, jf: 0, k: libc::SECCOMP_RET_KILL_PROCESS },
        ]
    }

    fn install_x32_guard() -> Result<(), SandboxError> {
        let program = x32_guard();
        let prog = libc::sock_fprog {
            len: program.len() as libc::c_ushort,
            filter: program.as_ptr() as *mut libc::sock_filter,
        };
        let rc = unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                libc::SECCOMP_FILTER_FLAG_TSYNC,
                &prog,
            )
        };
        if rc != 0 {
            return Err(kernel(format!(
                "seccomp(x32 guard): {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    // Syscalls denied with EPERM regardless of arguments, grouped by what
    // they would let escaped guest code do. clone/clone3 stay usable for
    // threads through the argument rules above.
    const DENIED: &[libc::c_long] = &[
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
        // exec and process creation (x86_64 legacy entry points)
        libc::SYS_execve,
        libc::SYS_execveat,
        libc::SYS_fork,
        libc::SYS_vfork,
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
        libc::SYS_setgroups,
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
        libc::SYS_memfd_secret,
        libc::SYS_personality,
        libc::SYS_modify_ldt,
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
        // System V and POSIX IPC shared with other processes
        libc::SYS_shmget,
        libc::SYS_shmat,
        libc::SYS_shmctl,
        libc::SYS_msgget,
        libc::SYS_msgsnd,
        libc::SYS_msgrcv,
        libc::SYS_msgctl,
        libc::SYS_semget,
        libc::SYS_semop,
        libc::SYS_semtimedop,
        libc::SYS_semctl,
        libc::SYS_mq_open,
        // metadata mutation and reads (Landlock covers open/create/remove,
        // not setattr or xattr; stat/access/readlink stay observable)
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
        libc::SYS_getxattr,
        libc::SYS_lgetxattr,
        libc::SYS_fgetxattr,
        libc::SYS_listxattr,
        libc::SYS_llistxattr,
        libc::SYS_flistxattr,
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
        // raw port io (x86_64 legacy)
        libc::SYS_ioperm,
        libc::SYS_iopl,
        libc::SYS_uselib,
    ];

    #[cfg(test)]
    mod tests {
        use super::*;
        use landlock::RulesetStatus;

        #[test]
        fn partially_or_not_enforced_rulesets_are_refused() {
            assert!(enforce_status(RulesetStatus::FullyEnforced).is_ok());
            assert!(enforce_status(RulesetStatus::PartiallyEnforced).is_err());
            assert!(enforce_status(RulesetStatus::NotEnforced).is_err());
        }

        #[test]
        fn clone_rules_deny_non_threads_and_namespace_creators() {
            let expected: Vec<SeccompRule> = std::iter::once(masked(libc::CLONE_THREAD as u64, 0))
                .chain(NAMESPACE_FLAGS.iter().copied().map(|f| masked(f, f)))
                .collect();
            assert_eq!(clone_rules(), expected);
            assert_eq!(clone_rules().len(), 1 + NAMESPACE_FLAGS.len());
        }

        #[test]
        fn ioctl_rules_deny_only_terminal_injection() {
            let expected = vec![
                condition(1, SeccompCmpOp::Eq, libc::TIOCSTI),
                condition(1, SeccompCmpOp::Eq, libc::TIOCLINUX),
            ];
            assert_eq!(ioctl_rules(), expected);
        }

        #[test]
        fn signal_rules_target_only_this_process() {
            let pid = unsafe { libc::getpid() } as u64;
            let rules = filter_rules();
            let deny = |nr: libc::c_long| rules.get(&nr).expect("rule present");
            assert_eq!(deny(libc::SYS_kill), &vec![condition(0, SeccompCmpOp::Ne, pid)]);
            assert_eq!(deny(libc::SYS_tgkill), &vec![condition(0, SeccompCmpOp::Ne, pid)]);
            assert_eq!(deny(libc::SYS_tkill), &Vec::new());
        }

        #[test]
        fn exec_and_network_are_denied_unconditionally() {
            let rules = filter_rules();
            for nr in [libc::SYS_execve, libc::SYS_execveat, libc::SYS_socket, libc::SYS_connect]
            {
                assert_eq!(rules.get(&nr), Some(&Vec::new()), "raw deny for {}", nr);
            }
            // the argument-filtered syscalls must be present too
            assert_eq!(rules.get(&libc::SYS_ioctl), Some(&ioctl_rules()));
            assert_eq!(rules.get(&libc::SYS_clone), Some(&clone_rules()));
        }

        #[test]
        fn x32_guard_kills_foreign_arch_and_x32_numbers() {
            let p = x32_guard();
            let kill = p.len() - 1;
            assert_eq!(p.len(), 6);
            // loads arch (seccomp_data offset 4), kills when it differs
            assert_eq!((p[0].code, p[0].k), ((libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, 4));
            assert_eq!((p[1].jf, p[1].k), (3, AUDIT_ARCH_X86_64));
            assert_eq!(2 + p[1].jf as usize, kill);
            // loads nr (offset 0), kills on the x32 range
            assert_eq!((p[2].code, p[2].k), ((libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, 0));
            assert_eq!((p[3].jt, p[3].k), (1, X32_BIT));
            assert_eq!(4 + p[3].jt as usize, kill);
            assert_eq!(p[4].k, libc::SECCOMP_RET_ALLOW);
            assert_eq!(p[5].k, libc::SECCOMP_RET_KILL_PROCESS);
        }
    }
}
