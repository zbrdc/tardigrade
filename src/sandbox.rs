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
//! Landlock ruleset that is not fully enforced (kernel below ABI 6).

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
    /// `--dump` names something other than a regular file (a pipe, a tty):
    /// Landlock can anchor a write rule only on a file or directory inode.
    DumpNotRegular(std::path::PathBuf),
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
            SandboxError::DumpNotRegular(path) => {
                write!(f, "--dump must be a regular file: {:?}", path)
            }
            SandboxError::Kernel(msg) => write!(f, "kernel refused the restrictions: {}", msg),
        }
    }
}

impl std::error::Error for SandboxError {}

/// Whether [`apply`] can work here: x86_64 Linux with Landlock ABI 6
/// (Linux 6.12). Purely informational — it installs nothing — so a caller
/// can tell "this kernel cannot sandbox" from any other refusal.
pub fn supported() -> bool {
    imp::supported()
}

/// Restrict this process: Landlock (filesystem, signal and abstract-socket
/// scopes) plus seccomp (syscalls).
///
/// `dump`, if set, is the only filesystem write the run may perform later:
/// the file is created now without truncation and the end-of-run snapshot
/// reopens that inode under a Landlock rule anchored to the open descriptor.
/// Must be called before the first [`crate::run`]/[`crate::serve`] and
/// before any thread exists; both are checked and refuse instead of
/// restricting only one thread.
///
/// This also closes every file descriptor >= 3 in the whole process
/// (`close_range`): descriptors inherited from the caller's environment
/// (a socket, a log file) would be a channel out of the sandbox.
pub fn apply(dump: Option<&Path>) -> Result<(), SandboxError> {
    imp::apply(dump)
}

/// Which kernel layer [`apply_layer`] installs.
///
/// Only built with the `test-layers` feature (a non-default feature enabled
/// for this crate's own tests through a self dev-dependency): the product
/// always applies both layers, but the tests need each layer alone to
/// attribute a denial to the right one (execve, for example, is denied by
/// seccomp with EPERM but also by Landlock's Execute right with EACCES, so
/// only a seccomp-only run proves the seccomp rule).
#[cfg(feature = "test-layers")]
#[doc(hidden)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Landlock,
    Seccomp,
}

/// Apply a single sandbox layer. Test-only; see [`Layer`].
#[cfg(feature = "test-layers")]
#[doc(hidden)]
pub fn apply_layer(layer: Layer, dump: Option<&Path>) -> Result<(), SandboxError> {
    imp::apply_layer(layer, dump)
}

/// Test-only (`test-layers`): the errno the enumeration's deny filter
/// returns. EOWNERDEAD (130) is produced by no syscall in the deny list, so
/// seeing it means the list matched.
#[cfg(all(feature = "test-layers", target_os = "linux", target_arch = "x86_64"))]
#[doc(hidden)]
pub const PROBE_DENY_ERRNO: libc::c_int = libc::EOWNERDEAD;

/// Test-only (`test-layers`): the syscall numbers denied outright, so a
/// probe can iterate the list instead of duplicating it.
#[cfg(all(feature = "test-layers", target_os = "linux", target_arch = "x86_64"))]
#[doc(hidden)]
pub fn denied_syscalls() -> Vec<libc::c_long> {
    imp::denied_syscalls()
}

/// Test-only (`test-layers`): the seccomp layer alone with the deny list
/// returning [`PROBE_DENY_ERRNO`], for that iteration.
#[cfg(all(feature = "test-layers", target_os = "linux", target_arch = "x86_64"))]
#[doc(hidden)]
pub fn apply_probe() -> Result<(), SandboxError> {
    imp::apply_probe(PROBE_DENY_ERRNO)
}

/// Stand-in for every target the layers are not verified on: the same
/// signatures as the real module, but nothing is ever restricted.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
mod imp {
    use super::SandboxError;
    use std::path::Path;

    pub fn apply(_dump: Option<&Path>) -> Result<(), SandboxError> {
        Err(SandboxError::Unsupported)
    }

    pub fn supported() -> bool {
        false
    }

    #[cfg(feature = "test-layers")]
    pub fn apply_layer(_layer: super::Layer, _dump: Option<&Path>) -> Result<(), SandboxError> {
        Err(SandboxError::Unsupported)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn apply_refuses_with_the_platform_message() {
            let err = apply(None).expect_err("no layer is available here");
            assert!(matches!(err, SandboxError::Unsupported));
            assert_eq!(err.to_string(), "--sandbox is only supported on Linux x86_64");
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod imp {
    use super::SandboxError;
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

    /// landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)
    /// returns the kernel's Landlock ABI, or -1 when Landlock is off.
    fn landlock_abi() -> libc::c_long {
        const VERSION: libc::c_ulong = 1;
        unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<libc::c_void>(),
                0 as libc::size_t,
                VERSION,
            )
        }
    }

    pub fn supported() -> bool {
        // ABI 6 carries both the filesystem rights and the scopes the
        // ruleset asks for; anything less is refused by enforce_status.
        landlock_abi() >= 6
    }

    /// Both layers, filesystem first and seccomp last: seccomp is the most
    /// irreversible, and its deny list refuses the seccomp syscall itself.
    pub fn apply(dump: Option<&Path>) -> Result<(), SandboxError> {
        let anchor = prepare(dump)?;
        landlock_restrict(anchor.as_ref())?;
        seccomp_restrict()
    }

    /// One layer alone, sharing every precondition with [`apply`].
    #[cfg(feature = "test-layers")]
    pub fn apply_layer(layer: super::Layer, dump: Option<&Path>) -> Result<(), SandboxError> {
        let anchor = prepare(dump)?;
        match layer {
            super::Layer::Landlock => landlock_restrict(anchor.as_ref()),
            super::Layer::Seccomp => seccomp_restrict(),
        }
    }

    /// The deny list as numbers, for the test-only enumeration.
    #[cfg(feature = "test-layers")]
    pub fn denied_syscalls() -> Vec<libc::c_long> {
        DENIED.to_vec()
    }

    /// The seccomp layer with a caller-chosen deny errno, for the same
    /// enumeration: preconditions as [`apply`], no Landlock, so nothing else
    /// can colour the result.
    #[cfg(feature = "test-layers")]
    pub fn apply_probe(deny_errno: libc::c_int) -> Result<(), SandboxError> {
        prepare(None)?;
        seccomp_restrict_with(deny_errno)
    }

    /// Everything that must hold before any restriction is installed:
    /// refuse late application, then ready the descriptors. Returns the dump
    /// anchor for the Landlock rule.
    fn prepare(dump: Option<&Path>) -> Result<Option<landlock::PathFd>, SandboxError> {
        if crate::host::engine_started() {
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
        Ok(dump_file)
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
            // Without this, a pipe or tty (--dump=/dev/stdout) reaches the
            // Landlock rule below and fails there with an opaque EBADFD.
            let regular = file.metadata().map(|m| m.is_file());
            if !regular.map_err(|e| SandboxError::Dump(path.to_path_buf(), e))? {
                return Err(SandboxError::DumpNotRegular(path.to_path_buf()));
            }
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

    /// Scopes requested from the kernel: no signals to processes outside the
    /// sandbox and no connecting to abstract unix sockets outside it. Both
    /// arrive with Landlock ABI 6, so requesting them also raises the minimum
    /// kernel this sandbox accepts.
    fn scopes() -> landlock::BitFlags<landlock::Scope> {
        use landlock::Access;
        landlock::Scope::from_all(landlock::ABI::V6)
    }

    /// Request and selector values libc does not define for x86_64 Linux.
    /// F_SETSIG chooses the signal delivered for async fd events, and
    /// F_SETOWN_EX aims them at a thread or process (fcntl(2)).
    const F_SETSIG: libc::c_int = 10;
    const F_SETOWN_EX: libc::c_int = 15;
    /// ioctl spellings of F_SETOWN (linux/sockios.h, asm-generic/ioctls.h).
    const FIOSETOWN: u64 = 0x8901;
    const SIOCSPGRP: u64 = 0x8902;
    /// ioprio_set `which` selecting a single process (linux/ioprio.h).
    const IOPRIO_WHO_PROCESS: u64 = 1;

    fn landlock_restrict(dump: Option<&landlock::PathFd>) -> Result<(), SandboxError> {
        use landlock::{
            make_bitflags, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr,
            RulesetCreatedAttr, ABI,
        };
        // ABI::V5 covers every filesystem right through IoctlDev, and the
        // scopes above need ABI::V6. Anything less must refuse instead of
        // silently dropping rights.
        let mut ruleset = Ruleset::default()
            .handle_access(AccessFs::from_all(ABI::V5))
            .map_err(land)?
            .scope(scopes())
            .map_err(land)?
            .create()
            .map_err(land)?;
        // V8 opens /dev/urandom while seeding RNGs (Math.random and hashing);
        // without this rule that open fails and the seed falls back to
        // weaker sources. Read-only, and this one file only.
        let urandom = PathFd::new("/dev/urandom").map_err(land)?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(urandom, AccessFs::ReadFile))
            .map_err(land)?;
        if let Some(anchor) = dump {
            let access = make_bitflags!(AccessFs::{WriteFile | Truncate});
            ruleset = ruleset.add_rule(PathBeneath::new(anchor, access)).map_err(land)?;
        }
        let status = ruleset.restrict_self().map_err(land)?;
        enforce_status(status.ruleset)
    }

    /// Tags a Landlock-layer failure so refusals name the refusing layer.
    fn land(msg: impl std::fmt::Display) -> SandboxError {
        SandboxError::Kernel(format!("Landlock: {}", msg))
    }

    /// Tags a seccomp-layer failure so refusals name the refusing layer.
    fn comp(msg: impl std::fmt::Display) -> SandboxError {
        SandboxError::Kernel(format!("seccomp: {}", msg))
    }

    /// Landlock must be fully enforced: a partially enforced ruleset means
    /// the kernel dropped handled rights or scopes (Landlock ABI below 6),
    /// which is a silent gap rather than a sandbox, so it is refused like a
    /// missing one.
    fn enforce_status(status: landlock::RulesetStatus) -> Result<(), SandboxError> {
        match status {
            landlock::RulesetStatus::FullyEnforced => Ok(()),
            landlock::RulesetStatus::PartiallyEnforced | landlock::RulesetStatus::NotEnforced => {
                Err(land("not fully enforced (needs ABI 6, Linux 6.12)"))
            }
        }
    }

    fn seccomp_restrict() -> Result<(), SandboxError> {
        seccomp_restrict_with(libc::EPERM)
    }

    /// The deny list's errno is a parameter so the test-only enumeration can
    /// ask for one no kernel call produces here, making every denial
    /// attributable to the list itself.
    fn seccomp_restrict_with(deny_errno: libc::c_int) -> Result<(), SandboxError> {
        // Order matters: the deny list refuses SYS_seccomp, so it must be the
        // last filter installed. clone3 gets ENOSYS (not EPERM) because
        // glibc's pthread_create falls back to clone only on ENOSYS; the
        // fallback is then argument-checked by the deny list.
        let nosys = compile(single(libc::SYS_clone3), libc::ENOSYS)?;
        apply_filter_all_threads(&nosys).map_err(comp)?;
        install_x32_guard()?;
        let deny = compile(filter_rules(), deny_errno)?;
        apply_filter_all_threads(&deny).map_err(comp)
    }

    /// Compile a default-allow filter whose matched syscalls return `errno`.
    fn compile(rules: Rules, errno: libc::c_int) -> Result<BpfProgram, SandboxError> {
        SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::Errno(errno as u32),
            // The module is x86_64-only; the arch token type is not exported
            // from the crate root, so it is inferred here.
            std::env::consts::ARCH.try_into().map_err(|_| comp("unsupported architecture"))?,
        )
        .map_err(comp)?
        .try_into()
        .map_err(comp)
    }

    fn single(nr: libc::c_long) -> Rules {
        [(nr, Vec::new())].into_iter().collect()
    }

    /// The deny list proper: each listed syscall fails with EPERM whatever
    /// its arguments. clone, ioctl and the signal syscalls need argument
    /// rules instead and are added on top.
    fn filter_rules() -> Rules {
        let mut rules: Rules = DENIED.iter().map(|nr| (*nr, Vec::new())).collect();
        let pid = unsafe { libc::getpid() } as u64;
        rules.insert(libc::SYS_clone, clone_rules());
        rules.insert(libc::SYS_ioctl, ioctl_rules());
        rules.insert(libc::SYS_fcntl, fcntl_rules());
        // Signals may only target this process (raise/abort paths); another
        // process, a whole group (pid 0 or -pgid), or a queued siginfo to
        // anyone else is denied. Landlock's Signal scope is the second wall.
        for nr in [
            libc::SYS_kill,
            libc::SYS_tgkill,
            libc::SYS_rt_sigqueueinfo,
            libc::SYS_rt_tgsigqueueinfo,
        ] {
            rules.insert(nr, vec![condition(0, SeccompCmpOp::Ne, pid)]);
        }
        // tkill carries no thread-group id to check: deny outright.
        rules.insert(libc::SYS_tkill, Vec::new());
        // Resource limits, scheduling and priority may only touch this
        // process: 0 (the caller) or its own pid, as glibc uses them.
        for nr in [
            libc::SYS_prlimit64,
            libc::SYS_sched_setaffinity,
            libc::SYS_sched_setscheduler,
            libc::SYS_sched_setattr,
            libc::SYS_sched_setparam,
        ] {
            rules.insert(nr, vec![foreign(0, pid)]);
        }
        rules.insert(libc::SYS_setpriority, priority_rules(libc::PRIO_PROCESS as u64, pid));
        rules.insert(libc::SYS_ioprio_set, priority_rules(IOPRIO_WHO_PROCESS, pid));
        rules
    }

    /// Denies an argument that is neither 0 (calling process) nor this
    /// process: both `Ne` conditions must hold, so self-targeted calls pass.
    fn foreign(arg: usize, pid: u64) -> SeccompRule {
        let differs_from = |value| {
            SeccompCondition::new(arg as u8, SeccompCmpArgLen::Dword, SeccompCmpOp::Ne, value)
                .expect("valid condition")
        };
        SeccompRule::new(vec![differs_from(0), differs_from(pid)]).expect("valid rule")
    }

    /// `which`/`who` calls (setpriority, ioprio_set): only the
    /// process-scoped `which` value, and only for the caller (0) or this
    /// process; a user- or group-wide target is denied.
    fn priority_rules(which_process: u64, pid: u64) -> Vec<SeccompRule> {
        vec![condition(0, SeccompCmpOp::Ne, which_process), foreign(1, pid)]
    }

    /// fcntl: aiming async-signal delivery at another process (F_SETOWN,
    /// F_SETOWN_EX) or choosing the signal (F_SETSIG) is denied; every other
    /// request (FD_CLOEXEC, locks, O_NONBLOCK) is untouched.
    fn fcntl_rules() -> Vec<SeccompRule> {
        vec![
            condition(1, SeccompCmpOp::Eq, libc::F_SETOWN as u64),
            condition(1, SeccompCmpOp::Eq, F_SETSIG as u64),
            condition(1, SeccompCmpOp::Eq, F_SETOWN_EX as u64),
        ]
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

    /// ioctl is denied only for the terminal input injection requests and
    /// the ioctl forms of F_SETOWN; the rest (isatty, window size on
    /// already-open fds) stays available.
    fn ioctl_rules() -> Vec<SeccompRule> {
        vec![
            condition(1, SeccompCmpOp::Eq, libc::TIOCSTI),
            condition(1, SeccompCmpOp::Eq, libc::TIOCLINUX),
            condition(1, SeccompCmpOp::Eq, FIOSETOWN),
            condition(1, SeccompCmpOp::Eq, SIOCSPGRP),
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
    /// fchmodat2, the first syscall newer than the deny list's newest entry:
    /// everything from here on is refused with ENOSYS so that a future
    /// kernel's new syscalls cannot bypass the documented denials (the
    /// round-4 review set modes and xattrs through fchmodat2, setxattrat and
    /// friends). Nothing in the run path calls anything this high (verified
    /// with strace over the examples and the suite).
    const NEW_SYSCALL_FLOOR: u32 = 452;

    /// A stacked filter for what syscall-number matching cannot express:
    /// seccompiler matches numbers exactly, so a call made through the x32
    /// ABI (nr | X32_BIT) would skip every deny rule, an int 0x80 call would
    /// arrive with a different arch, and a syscall newer than the deny list
    /// would not be in it at all. Foreign ABIs are kills; new numbers get
    /// ENOSYS, which is also what an unknown syscall returns.
    fn x32_guard() -> Vec<libc::sock_filter> {
        const LD_W: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
        const JEQ: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
        const JGE: u16 = (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16;
        const RET: u16 = (libc::BPF_RET | libc::BPF_K) as u16;
        const NOSYS: u32 = libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32;
        // seccomp_data: nr at offset 0, arch at offset 4. Jump offsets are
        // relative to the following instruction; [6] returns ENOSYS and [7]
        // is the KILL return.
        vec![
            libc::sock_filter { code: LD_W, jt: 0, jf: 0, k: 4 },
            libc::sock_filter { code: JEQ, jt: 0, jf: 5, k: AUDIT_ARCH_X86_64 },
            libc::sock_filter { code: LD_W, jt: 0, jf: 0, k: 0 },
            libc::sock_filter { code: JGE, jt: 3, jf: 0, k: X32_BIT },
            libc::sock_filter { code: JGE, jt: 1, jf: 0, k: NEW_SYSCALL_FLOOR },
            libc::sock_filter { code: RET, jt: 0, jf: 0, k: libc::SECCOMP_RET_ALLOW },
            libc::sock_filter { code: RET, jt: 0, jf: 0, k: NOSYS },
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
            return Err(comp(format!("x32 guard: {}", std::io::Error::last_os_error())));
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
        libc::SYS_mq_unlink,
        libc::SYS_mq_timedsend,
        libc::SYS_mq_timedreceive,
        libc::SYS_mq_notify,
        libc::SYS_mq_getsetattr,
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
        // file-event observation (live names leak through these)
        libc::SYS_inotify_init,
        libc::SYS_inotify_init1,
        libc::SYS_inotify_add_watch,
        libc::SYS_fanotify_init,
        libc::SYS_fanotify_mark,
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

    // The deny rules are exercised at runtime by tests/sandbox.rs; only
    // logic with no runtime counterpart is checked here.
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
        fn x32_guard_kills_foreign_arch_x32_and_new_numbers() {
            let p = x32_guard();
            let kill = p.len() - 1;
            let nosys = p.len() - 2;
            assert_eq!(p.len(), 8);
            // loads arch (seccomp_data offset 4), kills when it differs
            assert_eq!((p[0].code, p[0].k), ((libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, 4));
            assert_eq!((p[1].jf, p[1].k), (5, AUDIT_ARCH_X86_64));
            assert_eq!(2 + p[1].jf as usize, kill);
            // loads nr (offset 0): the x32 range kills, newer numbers ENOSYS
            assert_eq!((p[2].code, p[2].k), ((libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, 0));
            assert_eq!((p[3].jt, p[3].k), (3, X32_BIT));
            assert_eq!(4 + p[3].jt as usize, kill);
            assert_eq!((p[4].jt, p[4].k), (1, NEW_SYSCALL_FLOOR));
            assert_eq!(5 + p[4].jt as usize, nosys);
            // below the floor is allowed, then the two returns
            assert_eq!(p[5].k, libc::SECCOMP_RET_ALLOW);
            assert_eq!(p[6].k, libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32);
            assert_eq!(p[7].k, libc::SECCOMP_RET_KILL_PROCESS);
        }
    }
}
