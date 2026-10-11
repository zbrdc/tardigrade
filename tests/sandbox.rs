//! Tests for `--sandbox`. This target has no libtest harness (`harness =
//! false`): `sandbox::apply` refuses to run once any thread exists and libtest
//! always runs tests on a worker thread, so each probe re-executes this same
//! binary as a single-threaded child.
//!
//! Test selection follows libtest where it matters. Positional arguments are
//! substring filters (`--exact` makes them whole names), `--skip <name>`
//! removes cases, `--list` prints the case names and exits 0, and every other
//! `-` flag (`--nocapture`, `--test-threads=1`) is ignored. A filter that
//! matches nothing is not an error and exits 0 with a "0 run" line: cargo
//! hands `cargo test <name>` to every test target, so the name of a test in
//! another target must not fail this one. Only an unfiltered run in which
//! nothing ran fails.
//!
//! Probe modes are children of this binary started as
//! `--probe-mode=<mode> [args]`. The prefix keeps them from colliding with
//! test-name filters (a bare `x32` filter once selected the mode that kills
//! the process):
//!
//!   probe <dump> <escape_dir>   full sandbox, then probe syscalls
//!   landlock-only <dump> <dir>  Landlock layer alone
//!   seccomp-only <dump> <dir>   seccomp layer alone
//!   late-thread | late-v8       apply too late (refused)
//!   no-landlock | no-seccomp    kernel feature missing (refused)
//!   x32 | int80 | int80-bare    foreign-ABI syscall (killed / host check)
//!   old-kernel <case>...        run cases on a kernel without Landlock
//!
//! Probe output is "PROBE|<name>|<outcome>" with outcome `ok`, `skipped`, or
//! `errno=<n>` (the raw errno, so a test can tell Landlock's EACCES from
//! seccomp's EPERM).
//!
//! Kernel support: cases that need the full Landlock ABI 6 print "SKIPPED
//! (kernel lacks Landlock ABI 6): <case>" on stderr and pass when the kernel
//! lacks it. The fail-closed cases fake the missing feature themselves and
//! run everywhere. V8VM_REQUIRE_SANDBOX=1 turns every such skip into a
//! failure.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod common;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod run {
    use crate::common;
    use std::collections::BTreeMap;
    use std::convert::TryInto;
    use std::ffi::CString;
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output, Stdio};
    use std::time::{Duration, Instant};
    use v8vm::sandbox::{apply_layer, apply_probe, denied_syscalls, Layer, PROBE_DENY_ERRNO};

    const PROBE_PREFIX: &str = "--probe-mode=";
    /// Set on the children the harness case spawns, so they do not run the
    /// harness case again (the process chain would never end).
    const HARNESS_CHILD: &str = "V8VM_SANDBOX_HARNESS_CHILD";

    pub fn main() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        match args.first().and_then(|a| a.strip_prefix(PROBE_PREFIX)) {
            Some(mode) => run_probe_mode(mode, &args[1..]),
            None => std::process::exit(run_tests(&Options::parse(&args))),
        }
    }

    // ---- test selection ----

    /// Flags that take a separate value in libtest; the value is not a filter.
    const VALUE_FLAGS: [&str; 5] = ["--test-threads", "--format", "--color", "--logfile", "--shuffle-seed"];

    #[derive(Default)]
    struct Options {
        list: bool,
        exact: bool,
        filters: Vec<String>,
        skips: Vec<String>,
    }

    impl Options {
        fn parse(args: &[String]) -> Options {
            let mut opts = Options::default();
            let mut rest = args.iter();
            while let Some(arg) = rest.next() {
                match arg.as_str() {
                    "--list" => opts.list = true,
                    "--exact" => opts.exact = true,
                    "--skip" => opts.skips.extend(rest.next().cloned()),
                    flag if VALUE_FLAGS.contains(&flag) => {
                        rest.next();
                    }
                    flag if flag.starts_with("--skip=") => opts.skips.push(flag[7..].to_string()),
                    flag if flag.starts_with('-') => {}
                    filter => opts.filters.push(filter.to_string()),
                }
            }
            opts
        }

        fn selects(&self, name: &str) -> bool {
            let hit = |pattern: &String| {
                if self.exact {
                    name == pattern
                } else {
                    name.contains(pattern.as_str())
                }
            };
            (self.filters.is_empty() || self.filters.iter().any(hit)) && !self.skips.iter().any(hit)
        }
    }

    // ---- probe modes (run in a fresh, single-threaded child) ----

    fn run_probe_mode(mode: &str, rest: &[String]) {
        let path = |i: usize| PathBuf::from(&rest[i]);
        match mode {
            "probe" => mode_apply(None, path(0), path(1)),
            "landlock-only" => mode_apply(Some(Layer::Landlock), path(0), path(1)),
            "seccomp-only" => mode_apply(Some(Layer::Seccomp), path(0), path(1)),
            "late-thread" => mode_late_thread(),
            "late-v8" => mode_late_v8(),
            "no-landlock" => mode_missing("no-landlock", libc::SYS_landlock_create_ruleset),
            "no-seccomp" => mode_missing("no-seccomp", libc::SYS_seccomp),
            "deny-enum" => mode_deny_enum(),
            "x32" => mode_x32(),
            "int80" => mode_int80(),
            "int80-bare" => mode_int80_bare(),
            "old-kernel" => std::process::exit(mode_old_kernel(rest)),
            other => {
                eprintln!("unknown probe mode {:?}", other);
                std::process::exit(2);
            }
        }
    }

    fn mode_apply(layer: Option<Layer>, dump: PathBuf, escape_dir: PathBuf) {
        println!("PROBE|threads|{}", thread_count());
        let applied = match layer {
            None => v8vm::sandbox::apply(Some(&dump)),
            Some(layer) => apply_layer(layer, Some(&dump)),
        };
        if let Err(e) = applied {
            println!("PROBE|apply|refused:{}", e);
            return;
        }
        report("", &probe_all(&dump, &escape_dir));
        // Threads created after apply must inherit both layers.
        let owned = (dump.clone(), escape_dir.clone());
        let in_thread =
            std::thread::spawn(move || probe_all(&owned.0, &owned.1)).join().expect("probe thread");
        report("thr-", &in_thread);
    }

    /// The whole deny list through a filter whose denials return
    /// PROBE_DENY_ERRNO, an errno no kernel call produces here: every entry
    /// must report exactly that, so a missing or inert rule is named. The
    /// argument-rule shapes (namespace clones, TIOCLINUX) go through the
    /// same filter.
    fn mode_deny_enum() {
        if let Err(e) = apply_probe() {
            println!("PROBE|deny-enum|refused:{}", e);
            return;
        }
        println!("PROBE|deny-enum|ok");
        let want = format!("errno={}", PROBE_DENY_ERRNO);
        let mut count = 0;
        for (nr, name) in denied_table() {
            // fork and vfork would create a child if the rule were missing;
            // the raw path exits any child in asm before it can run.
            let outcome = match name {
                "fork" | "vfork" => probe_raw_process(nr),
                _ => sys(nr, [-1, -1, 0, 0, 0]),
            };
            count += usize::from(outcome == want);
            println!("PROBE|deny|{}|{}", name, outcome);
        }
        // Argument rules carry the same errno in this filter.
        let thread = (libc::CLONE_THREAD | libc::CLONE_SIGHAND | libc::CLONE_VM) as libc::c_ulong;
        for (name, flags) in NAMESPACE_PROBE_FLAGS {
            println!("PROBE|deny|clone-thread+{}|{}", name, probe_clone(thread | flags));
        }
        println!("PROBE|deny|clone-vm-nothread|{}", probe_clone((libc::CLONE_VM | libc::SIGCHLD) as libc::c_ulong));
        println!("PROBE|deny|ioctl-tioclinux|{}", on_pipe(|fd| raw(unsafe {
            libc::ioctl(fd, libc::TIOCLINUX as libc::c_ulong, b'x' as *const u8)
        })));
        println!("PROBE|deny|checked|{}", count);
    }

    /// (number, name) for every syscall denied outright, in the order the
    /// deny list holds them; the enumeration test also checks this against
    /// the list the library exports, so the two cannot drift apart.
    fn denied_table() -> Vec<(libc::c_long, &'static str)> {
        const NAMES: &[&str] = &[
        "socket",
        "socketpair",
        "connect",
        "bind",
        "listen",
        "accept",
        "accept4",
        "sendto",
        "recvfrom",
        "sendmsg",
        "recvmsg",
        "sendmmsg",
        "recvmmsg",
        "shutdown",
        "getsockname",
        "getpeername",
        "setsockopt",
        "getsockopt",
        "execve",
        "execveat",
        "fork",
        "vfork",
        "ptrace",
        "process_vm_readv",
        "process_vm_writev",
        "kcmp",
        "pidfd_open",
        "pidfd_getfd",
        "pidfd_send_signal",
        "setuid",
        "setgid",
        "setreuid",
        "setregid",
        "setresuid",
        "setresgid",
        "setfsuid",
        "setfsgid",
        "setgroups",
        "capset",
        "unshare",
        "setns",
        "mount",
        "umount2",
        "pivot_root",
        "chroot",
        "fsopen",
        "fsconfig",
        "fsmount",
        "move_mount",
        "open_tree",
        "mount_setattr",
        "fspick",
        "init_module",
        "finit_module",
        "delete_module",
        "kexec_load",
        "kexec_file_load",
        "reboot",
        "swapon",
        "swapoff",
        "acct",
        "sethostname",
        "setdomainname",
        "syslog",
        "bpf",
        "perf_event_open",
        "userfaultfd",
        "io_uring_setup",
        "io_uring_enter",
        "io_uring_register",
        "seccomp",
        "memfd_create",
        "memfd_secret",
        "personality",
        "modify_ldt",
        "keyctl",
        "add_key",
        "request_key",
        "open_by_handle_at",
        "name_to_handle_at",
        "clock_settime",
        "clock_adjtime",
        "adjtimex",
        "settimeofday",
        "shmget",
        "shmat",
        "shmctl",
        "msgget",
        "msgsnd",
        "msgrcv",
        "msgctl",
        "semget",
        "semop",
        "semtimedop",
        "semctl",
        "mq_open",
        "mq_unlink",
        "mq_timedsend",
        "mq_timedreceive",
        "mq_notify",
        "mq_getsetattr",
        "chmod",
        "fchmod",
        "fchmodat",
        "chown",
        "fchown",
        "lchown",
        "fchownat",
        "setxattr",
        "lsetxattr",
        "fsetxattr",
        "getxattr",
        "lgetxattr",
        "fgetxattr",
        "listxattr",
        "llistxattr",
        "flistxattr",
        "removexattr",
        "lremovexattr",
        "fremovexattr",
        "utime",
        "utimes",
        "futimesat",
        "utimensat",
        "inotify_init",
        "inotify_init1",
        "inotify_add_watch",
        "fanotify_init",
        "fanotify_mark",
        "sendfile",
        "splice",
        "vmsplice",
        "tee",
        "copy_file_range",
        "ioperm",
        "iopl",
        "uselib",
        ];
        // libc has no name-to-number lookup; the constants are matched by
        // the same tokens the deny list uses.
        fn nr_of(name: &str) -> libc::c_long {
            match name {
                "socket" => libc::SYS_socket,
                "socketpair" => libc::SYS_socketpair,
                "connect" => libc::SYS_connect,
                "bind" => libc::SYS_bind,
                "listen" => libc::SYS_listen,
                "accept" => libc::SYS_accept,
                "accept4" => libc::SYS_accept4,
                "sendto" => libc::SYS_sendto,
                "recvfrom" => libc::SYS_recvfrom,
                "sendmsg" => libc::SYS_sendmsg,
                "recvmsg" => libc::SYS_recvmsg,
                "sendmmsg" => libc::SYS_sendmmsg,
                "recvmmsg" => libc::SYS_recvmmsg,
                "shutdown" => libc::SYS_shutdown,
                "getsockname" => libc::SYS_getsockname,
                "getpeername" => libc::SYS_getpeername,
                "setsockopt" => libc::SYS_setsockopt,
                "getsockopt" => libc::SYS_getsockopt,
                "execve" => libc::SYS_execve,
                "execveat" => libc::SYS_execveat,
                "fork" => libc::SYS_fork,
                "vfork" => libc::SYS_vfork,
                "ptrace" => libc::SYS_ptrace,
                "process_vm_readv" => libc::SYS_process_vm_readv,
                "process_vm_writev" => libc::SYS_process_vm_writev,
                "kcmp" => libc::SYS_kcmp,
                "pidfd_open" => libc::SYS_pidfd_open,
                "pidfd_getfd" => libc::SYS_pidfd_getfd,
                "pidfd_send_signal" => libc::SYS_pidfd_send_signal,
                "setuid" => libc::SYS_setuid,
                "setgid" => libc::SYS_setgid,
                "setreuid" => libc::SYS_setreuid,
                "setregid" => libc::SYS_setregid,
                "setresuid" => libc::SYS_setresuid,
                "setresgid" => libc::SYS_setresgid,
                "setfsuid" => libc::SYS_setfsuid,
                "setfsgid" => libc::SYS_setfsgid,
                "setgroups" => libc::SYS_setgroups,
                "capset" => libc::SYS_capset,
                "unshare" => libc::SYS_unshare,
                "setns" => libc::SYS_setns,
                "mount" => libc::SYS_mount,
                "umount2" => libc::SYS_umount2,
                "pivot_root" => libc::SYS_pivot_root,
                "chroot" => libc::SYS_chroot,
                "fsopen" => libc::SYS_fsopen,
                "fsconfig" => libc::SYS_fsconfig,
                "fsmount" => libc::SYS_fsmount,
                "move_mount" => libc::SYS_move_mount,
                "open_tree" => libc::SYS_open_tree,
                "mount_setattr" => libc::SYS_mount_setattr,
                "fspick" => libc::SYS_fspick,
                "init_module" => libc::SYS_init_module,
                "finit_module" => libc::SYS_finit_module,
                "delete_module" => libc::SYS_delete_module,
                "kexec_load" => libc::SYS_kexec_load,
                "kexec_file_load" => libc::SYS_kexec_file_load,
                "reboot" => libc::SYS_reboot,
                "swapon" => libc::SYS_swapon,
                "swapoff" => libc::SYS_swapoff,
                "acct" => libc::SYS_acct,
                "sethostname" => libc::SYS_sethostname,
                "setdomainname" => libc::SYS_setdomainname,
                "syslog" => libc::SYS_syslog,
                "bpf" => libc::SYS_bpf,
                "perf_event_open" => libc::SYS_perf_event_open,
                "userfaultfd" => libc::SYS_userfaultfd,
                "io_uring_setup" => libc::SYS_io_uring_setup,
                "io_uring_enter" => libc::SYS_io_uring_enter,
                "io_uring_register" => libc::SYS_io_uring_register,
                "seccomp" => libc::SYS_seccomp,
                "memfd_create" => libc::SYS_memfd_create,
                "memfd_secret" => libc::SYS_memfd_secret,
                "personality" => libc::SYS_personality,
                "modify_ldt" => libc::SYS_modify_ldt,
                "keyctl" => libc::SYS_keyctl,
                "add_key" => libc::SYS_add_key,
                "request_key" => libc::SYS_request_key,
                "open_by_handle_at" => libc::SYS_open_by_handle_at,
                "name_to_handle_at" => libc::SYS_name_to_handle_at,
                "clock_settime" => libc::SYS_clock_settime,
                "clock_adjtime" => libc::SYS_clock_adjtime,
                "adjtimex" => libc::SYS_adjtimex,
                "settimeofday" => libc::SYS_settimeofday,
                "shmget" => libc::SYS_shmget,
                "shmat" => libc::SYS_shmat,
                "shmctl" => libc::SYS_shmctl,
                "msgget" => libc::SYS_msgget,
                "msgsnd" => libc::SYS_msgsnd,
                "msgrcv" => libc::SYS_msgrcv,
                "msgctl" => libc::SYS_msgctl,
                "semget" => libc::SYS_semget,
                "semop" => libc::SYS_semop,
                "semtimedop" => libc::SYS_semtimedop,
                "semctl" => libc::SYS_semctl,
                "mq_open" => libc::SYS_mq_open,
                "mq_unlink" => libc::SYS_mq_unlink,
                "mq_timedsend" => libc::SYS_mq_timedsend,
                "mq_timedreceive" => libc::SYS_mq_timedreceive,
                "mq_notify" => libc::SYS_mq_notify,
                "mq_getsetattr" => libc::SYS_mq_getsetattr,
                "chmod" => libc::SYS_chmod,
                "fchmod" => libc::SYS_fchmod,
                "fchmodat" => libc::SYS_fchmodat,
                "chown" => libc::SYS_chown,
                "fchown" => libc::SYS_fchown,
                "lchown" => libc::SYS_lchown,
                "fchownat" => libc::SYS_fchownat,
                "setxattr" => libc::SYS_setxattr,
                "lsetxattr" => libc::SYS_lsetxattr,
                "fsetxattr" => libc::SYS_fsetxattr,
                "getxattr" => libc::SYS_getxattr,
                "lgetxattr" => libc::SYS_lgetxattr,
                "fgetxattr" => libc::SYS_fgetxattr,
                "listxattr" => libc::SYS_listxattr,
                "llistxattr" => libc::SYS_llistxattr,
                "flistxattr" => libc::SYS_flistxattr,
                "removexattr" => libc::SYS_removexattr,
                "lremovexattr" => libc::SYS_lremovexattr,
                "fremovexattr" => libc::SYS_fremovexattr,
                "utime" => libc::SYS_utime,
                "utimes" => libc::SYS_utimes,
                "futimesat" => libc::SYS_futimesat,
                "utimensat" => libc::SYS_utimensat,
                "inotify_init" => libc::SYS_inotify_init,
                "inotify_init1" => libc::SYS_inotify_init1,
                "inotify_add_watch" => libc::SYS_inotify_add_watch,
                "fanotify_init" => libc::SYS_fanotify_init,
                "fanotify_mark" => libc::SYS_fanotify_mark,
                "sendfile" => libc::SYS_sendfile,
                "splice" => libc::SYS_splice,
                "vmsplice" => libc::SYS_vmsplice,
                "tee" => libc::SYS_tee,
                "copy_file_range" => libc::SYS_copy_file_range,
                "ioperm" => libc::SYS_ioperm,
                "iopl" => libc::SYS_iopl,
                "uselib" => libc::SYS_uselib,
                _ => unreachable!("test table entry without a libc constant"),
            }
        }
        NAMES.iter().map(|n| (nr_of(n), *n)).collect()
    }

    /// A syscall number in the x32 range (nr | 0x40000000) must be killed by
    /// the stacked guard with SIGSYS. Seccomp acts before syscall dispatch,
    /// so this is testable even on a host whose kernel would reject the call.
    fn mode_x32() {
        apply_seccomp_only("abi-x32-dump.json");
        println!("PROBE|x32|armed");
        let rv = unsafe { libc::syscall(0x4000_0000 | libc::SYS_getpid) };
        println!("PROBE|x32-getpid-survived|{}", raw(rv));
    }

    /// `int 0x80` arrives with the i386 arch value and must be killed with
    /// SIGSYS the same way, where the kernel delivers it to seccomp at all.
    fn mode_int80() {
        apply_seccomp_only("abi-int80-dump.json");
        println!("PROBE|int80|armed");
        int80_getpid();
        println!("PROBE|int80-survived|ok");
    }

    /// The same call with no sandbox: tells the test whether this host's
    /// kernel accepts `int 0x80` (ia32 emulation) in the first place.
    fn mode_int80_bare() {
        int80_getpid();
        println!("PROBE|int80-bare|survived");
    }

    /// Only the x32 guard is under test, so the seccomp layer alone is
    /// enough and the mode does not depend on Landlock support.
    fn apply_seccomp_only(dump_name: &str) {
        apply_layer(Layer::Seccomp, Some(&scratch(dump_name))).expect("apply before the foreign-abi call");
    }

    /// int 0x80 with eax=20 (i386 getpid). rbx cannot be an inline-asm
    /// operand on x86_64, so the caller-saved registers are preserved by hand.
    fn int80_getpid() {
        unsafe {
            std::arch::asm!(
                "push rbx",
                "push rcx",
                "push rdx",
                "push rsi",
                "push rdi",
                "int $$0x80",
                "pop rdi",
                "pop rsi",
                "pop rdx",
                "pop rcx",
                "pop rbx",
                in("eax") 20u32,
                lateout("eax") _,
            );
        }
    }

    fn mode_late_thread() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let keeper = std::thread::spawn(move || {
            let _ = rx.recv();
        });
        report_apply("late-thread", v8vm::sandbox::apply(None));
        drop(tx);
        let _ = keeper.join();
    }

    fn mode_late_v8() {
        let _ = v8vm::run("", "noop.js", &v8vm::Config::new());
        report_apply("late-v8", v8vm::sandbox::apply(None));
    }

    /// Simulates a kernel without a layer: the layer's setup syscall fails
    /// with ENOSYS before apply runs, which must refuse, not run.
    fn mode_missing(name: &str, nr: libc::c_long) {
        install_errno_filter(nr, libc::ENOSYS);
        let applied = if name == "no-seccomp" && !common::kernel_supports_sandbox() {
            // A full apply would stop at Landlock first on this kernel and
            // hide the seccomp refusal under test.
            apply_layer(Layer::Seccomp, None)
        } else {
            v8vm::sandbox::apply(None)
        };
        report_apply(name, applied);
    }

    /// Runs the named cases on a simulated kernel without Landlock: every
    /// child it spawns inherits the ENOSYS filter too.
    fn mode_old_kernel(case_names: &[String]) -> i32 {
        install_errno_filter(libc::SYS_landlock_create_ruleset, libc::ENOSYS);
        let mut args = vec!["--exact".to_string()];
        args.extend(case_names.iter().cloned());
        run_tests(&Options::parse(&args))
    }

    fn report_apply(name: &str, applied: Result<(), v8vm::sandbox::SandboxError>) {
        match applied {
            Err(e) => println!("PROBE|{}|refused:{}", name, e),
            Ok(()) => println!("PROBE|{}|ok", name),
        }
    }

    fn install_errno_filter(nr: libc::c_long, errno: libc::c_int) {
        // c_long is the syscall-number type and i64 on this arch.
        let rules: BTreeMap<libc::c_long, Vec<seccompiler::SeccompRule>> =
            [(nr, Vec::new())].into_iter().collect();
        let filter: seccompiler::BpfProgram = seccompiler::SeccompFilter::new(
            rules,
            seccompiler::SeccompAction::Allow,
            seccompiler::SeccompAction::Errno(errno as u32),
            std::env::consts::ARCH.try_into().unwrap(),
        )
        .unwrap()
        .try_into()
        .unwrap();
        seccompiler::apply_filter_all_threads(&filter).unwrap();
    }

    // ---- probes ----

    type Probe = (String, String);
    /// A named call with no target, and one aimed at a pid.
    type SelfProbe = (&'static str, fn() -> String);
    type PidProbe = (&'static str, fn(i32) -> String);

    fn probe(name: &str, outcome: String) -> Probe {
        (name.to_string(), outcome)
    }

    fn probe_all(dump: &Path, escape_dir: &Path) -> Vec<Probe> {
        let parent = std::env::var("PROBE_PARENT").ok().and_then(|p| p.parse().ok());
        let own_pid = unsafe { libc::getpid() };
        let mut probes = fs_probes(dump, escape_dir);
        for (name, call) in SELF_PROBES {
            probes.push(probe(name, call()));
        }
        for (name, call) in PARENT_PROBES {
            probes.push(probe(name, with_parent(parent, *call)));
        }
        // Process-scoped calls: allowed on the caller (as 0 and as its own
        // pid), denied on another process.
        for (name, call) in TARGET_PROBES {
            probes.push(probe(&format!("{}-self-0", name), call(0)));
            probes.push(probe(&format!("{}-self-pid", name), call(own_pid)));
            probes.push(probe(&format!("{}-parent", name), with_parent(parent, *call)));
        }
        probes.extend(clone_probes());
        probes
    }

    /// Probes aimed at the test process that spawned this child. Without a
    /// parent pid they report `skipped`, which no expectation accepts.
    fn with_parent(parent: Option<i32>, aim: fn(i32) -> String) -> String {
        parent.map_or_else(|| "skipped".to_string(), aim)
    }

    /// Probes for the syscalls newer than the deny list: the range rule in
    /// the stacked guard must refuse them with ENOSYS whatever their
    /// arguments. They aim at the dump file, which the run owns, so an
    /// unexpectedly successful call damages nothing that matters.
    fn new_syscall_probes(dump: &Path) -> Vec<Probe> {
        let path = CString::new(path_string(dump)).expect("dump path");
        let name = addr(c"user.v8vm".as_ptr());
        // struct xattr_args { u64 value; u32 size; u32 flags; }
        let mut args = [0u8; 16];
        args[0..8].copy_from_slice(&1u64.to_ne_bytes());
        let file_attr = [0u8; 32]; // struct file_attr, all zero
        vec![
            probe("fchmodat2", sys(NR_FCHMODAT2, [libc::AT_FDCWD as _, addr(path.as_ptr()), 0o600 as _, 0, 0])),
            probe("setxattrat", sys(NR_SETXATTRAT, [libc::AT_FDCWD as _, addr(path.as_ptr()), 0, name, addr(args.as_ptr())])),
            probe("getxattrat", sys(NR_GETXATTRAT, [libc::AT_FDCWD as _, addr(path.as_ptr()), 0, name, addr(args.as_ptr())])),
            probe("listxattrat", sys(NR_LISTXATTRAT, [libc::AT_FDCWD as _, addr(path.as_ptr()), 0, addr(args.as_ptr()), 0])),
            probe("removexattrat", sys(NR_REMOVEXATTRAT, [libc::AT_FDCWD as _, addr(path.as_ptr()), 0, name, 0])),
            probe("file-getattr", sys(NR_FILE_GETATTR, [libc::AT_FDCWD as _, addr(path.as_ptr()), addr(file_attr.as_ptr()), file_attr.len() as _, 0])),
            probe("file-setattr", sys(NR_FILE_SETATTR, [libc::AT_FDCWD as _, addr(path.as_ptr()), addr(file_attr.as_ptr()), file_attr.len() as _, 0])),
        ]
    }

    /// Probes for the newest syscalls below the floor: they must reach the
    /// kernel (and fail on their invalid arguments) instead of being turned
    /// into ENOSYS, which pins the lower edge of the range rule.
    fn below_floor_probes() -> Vec<Probe> {
        vec![
            // cachestat(fd, range, cstat, flags): fd -1 is EBADF.
            probe("cachestat", sys(NR_CACHESTAT, [-1, 0, 0, 0, 0])),
            // futex_waitv(waiters, nr_futexes, flags, timeout, clockid): zero
            // futexes is EINVAL.
            probe("futex-waitv", sys(NR_FUTEX_WAITV, [0, 0, 0, 0, 0])),
        ]
    }

    /// Syscall numbers libc 0.2.190 does not define (post-6.12 additions;
    /// asm/unistd_64.h on this host): futex_waitv 449, cachestat 451, fchmodat2 452, setxattrat 463,
    /// getxattrat 464, listxattrat 465, removexattrat 466, file_getattr 468,
    /// file_setattr 469.
    const NR_FUTEX_WAITV: libc::c_long = 449;
    const NR_CACHESTAT: libc::c_long = 451;
    const NR_FCHMODAT2: libc::c_long = 452;
    const NR_SETXATTRAT: libc::c_long = 463;
    const NR_GETXATTRAT: libc::c_long = 464;
    const NR_LISTXATTRAT: libc::c_long = 465;
    const NR_REMOVEXATTRAT: libc::c_long = 466;
    const NR_FILE_GETATTR: libc::c_long = 468;
    const NR_FILE_SETATTR: libc::c_long = 469;

    fn fs_probes(dump: &Path, escape_dir: &Path) -> Vec<Probe> {
        let mut probes = new_syscall_probes(dump);
        probes.extend(below_floor_probes());
        probes.extend(vec![
            probe("read-etc-passwd", io(std::fs::File::open("/etc/passwd").map(|_| ()))),
            probe("read-urandom", io(std::fs::File::open("/dev/urandom").map(|_| ()))),
            probe("write-existing", io(std::fs::write(escape_dir.join("escape.txt"), "x"))),
            probe("write-new", io(std::fs::write(escape_dir.join("new.txt"), "x"))),
            probe("write-dump", io(std::fs::write(dump, "{\"probe\":1}"))),
            probe("read-dump", io(std::fs::File::open(dump).map(|_| ()))),
        ]);
        probes
    }

    /// Probes that need no other process. Argument-less denials are exercised
    /// with arguments that name nothing real, so a missing rule shows up as
    /// a different errno (EBADF, ESRCH, ENOENT) rather than as damage.
    const SELF_PROBES: &[SelfProbe] = &[
        ("socket", || raw(unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) })),
        ("exec", probe_execve),
        ("fork", || syscall_outcome(unsafe { libc::fork() as libc::c_long })),
        ("sys-fork", || probe_raw_process(libc::SYS_fork)),
        ("sys-vfork", || probe_raw_process(libc::SYS_vfork)),
        ("clone-newuser", || probe_clone((libc::CLONE_NEWUSER | libc::SIGCHLD) as libc::c_ulong)),
        ("clone3", probe_clone3),
        ("thread-spawn", probe_thread),
        ("kill-self", || raw(unsafe { libc::kill(libc::getpid(), 0) })),
        ("ioctl-tiocsti", || raw(unsafe { libc::ioctl(0, libc::TIOCSTI as libc::c_ulong, b'x' as *const u8) })),
        // The kernel ignores the high 32 bits of an ioctl request, so the
        // deny must compare the low dword only.
        ("ioctl-tiocsti-hibit", || {
            raw(unsafe { libc::ioctl(0, (libc::TIOCSTI as libc::c_ulong) | 1 << 32, b'x' as *const u8) })
        }),
        ("fcntl-setsig", || on_pipe(|fd| raw(unsafe { libc::fcntl(fd, F_SETSIG, libc::SIGUSR1) }))),
        ("inotify-init", || raw(unsafe { libc::inotify_init1(0) })),
        ("inotify-watch", || raw(unsafe { libc::inotify_add_watch(-1, c"/home".as_ptr(), 0xfff) })),
        ("fanotify-init", || sys(libc::SYS_fanotify_init, [(libc::FAN_CLASS_NOTIF | libc::FAN_REPORT_FID) as _, 0, 0, 0, 0])),
        ("fanotify-mark", || {
            let path = addr(c"/".as_ptr());
            sys(libc::SYS_fanotify_mark, [-1, libc::FAN_MARK_ADD as _, 0, libc::AT_FDCWD as _, path])
        }),
        ("mq-open", || sys(libc::SYS_mq_open, [addr(c"/v8vm-probe-none".as_ptr()), libc::O_RDONLY as _, 0, 0, 0])),
        ("mq-timedsend", || sys(libc::SYS_mq_timedsend, [-1, addr(c"x".as_ptr()), 1, 0, 0])),
        ("mq-timedreceive", || {
            let mut buf = [0u8; 8192];
            sys(libc::SYS_mq_timedreceive, [-1, addr(buf.as_mut_ptr()), buf.len() as _, 0, 0])
        }),
        ("mq-notify", || sys(libc::SYS_mq_notify, [-1, 0, 0, 0, 0])),
        ("mq-getsetattr", || sys(libc::SYS_mq_getsetattr, [-1, 0, 0, 0, 0])),
        // Group- and user-wide targets that match no process. The group
        // probes use the caller's own pid as the group id, which passes the
        // "who" check, so only the "which" check can deny them (and no
        // group has that id, so a missing rule changes nothing).
        ("setpriority-pgrp", || {
            let pgid = unsafe { libc::getpid() } as libc::id_t;
            raw(unsafe { libc::setpriority(libc::PRIO_PGRP, pgid, current_nice()) })
        }),
        ("ioprio-set-pgrp", || {
            sys(libc::SYS_ioprio_set, [IOPRIO_WHO_PGRP, unsafe { libc::getpid() } as _, 0, 0, 0])
        }),
        ("setpriority-user", || {
            raw(unsafe { libc::setpriority(libc::PRIO_USER, UNUSED_ID, current_nice()) })
        }),
        ("ioprio-set-user", || sys(libc::SYS_ioprio_set, [IOPRIO_WHO_USER, UNUSED_ID as _, 0, 0, 0])),
    ];

    /// An id (pgid or uid) that no process has.
    const UNUSED_ID: libc::id_t = 4_000_000_000;
    /// linux/ioprio.h: `which` values for ioprio_set.
    const IOPRIO_WHO_PROCESS: libc::c_long = 1;
    const IOPRIO_WHO_PGRP: libc::c_long = 2;
    const IOPRIO_WHO_USER: libc::c_long = 3;
    /// fcntl(2) values libc does not define for x86_64 Linux.
    const F_SETSIG: libc::c_int = 10;
    const F_SETOWN_EX: libc::c_int = 15;
    const F_OWNER_PID: libc::c_int = 1;
    /// ioctl spellings of F_SETOWN (linux/sockios.h, asm-generic/ioctls.h).
    const FIOSETOWN: libc::c_ulong = 0x8901;
    const SIOCSPGRP: libc::c_ulong = 0x8902;

    /// Probes aimed at the parent test process, which seccomp must refuse and
    /// which the Landlock scopes refuse too (SCOPED_PARENT).
    const PARENT_PROBES: &[PidProbe] = &[
        ("kill-parent", |pid| raw(unsafe { libc::kill(pid, 0) })),
        ("tgkill-parent", |pid| raw(unsafe { libc::tgkill(pid, pid, 0) })),
        ("tkill-parent", |pid| sys(libc::SYS_tkill, [pid as _, 0, 0, 0, 0])),
        // Seccomp denies on the pid argument before the kernel validates the
        // siginfo, so its content is moot.
        ("sigqueue-parent", |pid| {
            let info = queue_info();
            sys(libc::SYS_rt_sigqueueinfo, [pid as _, 0, addr(&info), 0, 0])
        }),
        ("rt-tgsigqueue-parent", |pid| {
            let info = queue_info();
            sys(libc::SYS_rt_tgsigqueueinfo, [pid as _, pid as _, 0, addr(&info), 0])
        }),
        ("fcntl-setown-parent", |pid| on_pipe(|fd| raw(unsafe { libc::fcntl(fd, libc::F_SETOWN, pid) }))),
        ("fcntl-setown-ex-parent", |pid| {
            let owner = [F_OWNER_PID, pid]; // struct f_owner_ex { int type; pid_t pid; }
            on_pipe(|fd| raw(unsafe { libc::fcntl(fd, F_SETOWN_EX, owner.as_ptr()) }))
        }),
        ("ioctl-fiosetown-parent", |pid| on_pipe(|fd| raw(unsafe { libc::ioctl(fd, FIOSETOWN, &pid) }))),
        ("ioctl-siocspgrp-parent", |pid| on_pipe(|fd| raw(unsafe { libc::ioctl(fd, SIOCSPGRP, &pid) }))),
        ("mq-unlink-parent", |pid| {
            // The raw syscall takes the name without its leading slash, and
            // glibc's mq_unlink would turn EPERM into EACCES.
            let name = CString::new(fixture_queue_name(pid)).expect("queue name");
            sys(libc::SYS_mq_unlink, [addr(name.as_ptr().wrapping_add(1)), 0, 0, 0, 0])
        }),
        ("connect-abstract-parent", |pid| {
            let sock = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
            if sock < 0 {
                return errno_last();
            }
            let (address, len) = abstract_addr(pid);
            let rv = unsafe { libc::connect(sock, &address as *const _ as *const libc::sockaddr, len) };
            let outcome = raw(rv);
            unsafe { libc::close(sock) };
            outcome
        }),
    ];

    /// Process-scoped calls: they must work on the caller and be refused for
    /// another process. Each takes the target pid.
    const TARGET_PROBES: &[PidProbe] = &[
        ("prlimit", prlimit),
        ("sched-setscheduler", sched_setscheduler),
        ("sched-setattr", sched_setattr),
        ("sched-setparam", sched_setparam),
        ("sched-setaffinity", sched_setaffinity),
        ("setpriority", |pid| {
            raw(unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, current_nice()) })
        }),
        ("ioprio-set", |pid| sys(libc::SYS_ioprio_set, [IOPRIO_WHO_PROCESS, pid as _, 0, 0, 0])),
    ];

    /// A raw syscall with up to five integer arguments (pointers as
    /// addresses); the kernel ignores the ones a call does not use.
    fn sys(nr: libc::c_long, args: [libc::c_long; 5]) -> String {
        raw(unsafe { libc::syscall(nr, args[0], args[1], args[2], args[3], args[4]) })
    }

    fn addr<T>(pointer: *const T) -> libc::c_long {
        pointer as libc::c_long
    }

    fn queue_info() -> libc::siginfo_t {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        info.si_code = libc::SI_QUEUE;
        info
    }

    /// The current nice value, so a priority call that is allowed never
    /// lowers it (raising priority needs privilege) and never touches the
    /// harness when a missing rule lets it reach the parent.
    fn current_nice() -> libc::c_int {
        unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) }
    }

    fn prlimit(pid: i32) -> String {
        let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
        sys(libc::SYS_prlimit64, [pid as _, libc::RLIMIT_NOFILE as _, 0, addr(std::ptr::addr_of_mut!(limit)), 0])
    }

    fn sched_setscheduler(pid: i32) -> String {
        let param = libc::sched_param { sched_priority: 0 };
        sys(libc::SYS_sched_setscheduler, [pid as _, libc::SCHED_OTHER as _, addr(&param), 0, 0])
    }

    fn sched_setparam(pid: i32) -> String {
        let param = libc::sched_param { sched_priority: 0 };
        sys(libc::SYS_sched_setparam, [pid as _, addr(&param), 0, 0, 0])
    }

    /// struct sched_attr at its original (version 0) size.
    #[repr(C)]
    struct SchedAttr {
        size: u32,
        policy: u32,
        flags: u64,
        nice: i32,
        priority: u32,
        runtime: u64,
        deadline: u64,
        period: u64,
    }

    fn sched_setattr(pid: i32) -> String {
        let attr = SchedAttr {
            size: std::mem::size_of::<SchedAttr>() as u32,
            policy: libc::SCHED_OTHER as u32,
            flags: 0,
            nice: current_nice(),
            priority: 0,
            runtime: 0,
            deadline: 0,
            period: 0,
        };
        sys(libc::SYS_sched_setattr, [pid as _, addr(&attr), 0, 0, 0])
    }

    /// Re-applies the caller's own affinity mask, a no-op where allowed.
    fn sched_setaffinity(pid: i32) -> String {
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::cpu_set_t>();
        unsafe { libc::sched_getaffinity(0, size, &mut set) };
        sys(libc::SYS_sched_setaffinity, [pid as _, size as _, addr(&set), 0, 0])
    }

    /// Runs `aim` on a fresh pipe write end (fd 0 is not a pipe), then closes it.
    fn on_pipe(aim: impl FnOnce(libc::c_int) -> String) -> String {
        let mut fds = [0 as libc::c_int; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return errno_last();
        }
        let outcome = aim(fds[1]);
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        outcome
    }

    // ---- fixtures the probes aim at in the parent ----

    fn fixture_queue_name(owner: i32) -> String {
        format!("/v8vm-probe-{}", owner)
    }

    /// Abstract unix socket address named after its owner's pid.
    fn abstract_addr(owner: i32) -> (libc::sockaddr_un, libc::socklen_t) {
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let name = format!("v8vm-probe-{}", owner);
        for (slot, byte) in address.sun_path[1..].iter_mut().zip(name.bytes()) {
            *slot = byte as libc::c_char;
        }
        let len = std::mem::size_of::<libc::sa_family_t>() + 1 + name.len();
        (address, len as libc::socklen_t)
    }

    /// What the probes aim at, owned by the test process: a POSIX message
    /// queue (mq_unlink must not destroy it) and a listening abstract unix
    /// socket (the Landlock scope must stop connections to it). Both are
    /// named after this pid and removed on drop.
    struct Fixtures {
        listener: libc::c_int,
        queue: CString,
    }

    impl Fixtures {
        fn create() -> Fixtures {
            let owner = std::process::id() as i32;
            let listener = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
            let (address, len) = abstract_addr(owner);
            let bound = unsafe { libc::bind(listener, &address as *const _ as *const libc::sockaddr, len) };
            let listening = unsafe { libc::listen(listener, 4) };
            assert!(listener >= 0 && bound == 0 && listening == 0, "abstract socket fixture: {}", errno_last());
            let queue = CString::new(fixture_queue_name(owner)).expect("queue name");
            // Best effort: where queues cannot be created, mq_unlink still
            // differs observably (ENOENT instead of the sandbox's EPERM).
            let mode = 0o600 as libc::mode_t;
            let mq = unsafe { libc::mq_open(queue.as_ptr(), libc::O_CREAT | libc::O_RDWR, mode, std::ptr::null::<libc::mq_attr>()) };
            if mq != -1 {
                unsafe { libc::mq_close(mq) };
            }
            Fixtures { listener, queue }
        }
    }

    impl Drop for Fixtures {
        fn drop(&mut self) {
            unsafe {
                libc::close(self.listener);
                libc::mq_unlink(self.queue.as_ptr());
            }
        }
    }

    // ---- process-creation probes ----

    /// (name, CLONE_NEW* flag) pairs probed on top of a thread clone.
    const NAMESPACE_PROBE_FLAGS: [(&str, libc::c_ulong); 8] = [
        ("newuser", libc::CLONE_NEWUSER as libc::c_ulong),
        ("newnet", libc::CLONE_NEWNET as libc::c_ulong),
        ("newpid", libc::CLONE_NEWPID as libc::c_ulong),
        ("newns", libc::CLONE_NEWNS as libc::c_ulong),
        ("newuts", libc::CLONE_NEWUTS as libc::c_ulong),
        ("newipc", libc::CLONE_NEWIPC as libc::c_ulong),
        ("newcgroup", libc::CLONE_NEWCGROUP as libc::c_ulong),
        ("newtime", libc::CLONE_NEWTIME as libc::c_ulong),
    ];

    /// Clone shapes: a thread that also creates a namespace, and the
    /// vfork/CLONE_VM forms used to start another program.
    fn clone_probes() -> Vec<Probe> {
        let thread = (libc::CLONE_THREAD | libc::CLONE_SIGHAND | libc::CLONE_VM) as libc::c_ulong;
        let mut probes: Vec<Probe> = NAMESPACE_PROBE_FLAGS
            .iter()
            .map(|(name, flag)| probe(&format!("clone-thread+{}", name), probe_clone(thread | flag)))
            .collect();
        let vfork = (libc::CLONE_VM | libc::CLONE_VFORK | libc::SIGCHLD) as libc::c_ulong;
        probes.push(probe("clone-vfork", probe_clone(vfork)));
        probes.push(probe("clone-vm-nothread", probe_clone((libc::CLONE_VM | libc::SIGCHLD) as libc::c_ulong)));
        probes
    }

    fn clone_probe_names() -> Vec<String> {
        let mut names: Vec<String> =
            NAMESPACE_PROBE_FLAGS.iter().map(|(name, _)| format!("clone-thread+{}", name)).collect();
        names.extend(["clone-vfork", "clone-vm-nothread"].map(String::from));
        names
    }

    /// A direct execve: the only way to attribute the errno to seccomp rather
    /// than to std::process' clone-based spawn path.
    fn probe_execve() -> String {
        let path = CString::new("/bin/true").unwrap();
        let argv = [path.as_ptr(), std::ptr::null()];
        let envp: [*const libc::c_char; 1] = [std::ptr::null()];
        let rv = unsafe { libc::syscall(libc::SYS_execve, path.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
        // execve only returns when it fails; success would replace the child.
        assert_eq!(rv, -1, "execve unexpectedly returned {}", rv);
        errno_last()
    }

    /// A raw process-creating syscall with the child handled entirely in
    /// asm: returning through libc's syscall wrapper would use the parent's
    /// frame on the child's new stack (garbage), so a child that is ever
    /// created exits through SYS_exit before any Rust runs. `nr` is clone
    /// (which uses `flags` and `stack`), fork or vfork (which ignore both).
    fn raw_process_syscall(nr: libc::c_long, flags: libc::c_ulong, stack: *mut libc::c_void) -> String {
        let rv: i64;
        unsafe {
            std::arch::asm!(
                "syscall",
                "cmp rax, 0",
                "jne 2f",
                "mov rax, 60", // SYS_exit: leave without touching the stack
                "xor edi, edi",
                "syscall",
                "2:",
                inlateout("rax") nr => rv,
                in("rdi") flags,
                in("rsi") stack,
                in("rdx") 0_i64,
                in("r10") 0_i64,
                in("r8") 0_i64,
                lateout("rcx") _,
                lateout("r11") _,
                options(nostack)
            );
        }
        asm_outcome(rv)
    }

    fn probe_raw_process(nr: libc::c_long) -> String {
        raw_process_syscall(nr, 0, std::ptr::null_mut())
    }

    fn probe_clone(flags: libc::c_ulong) -> String {
        let stack = clone_stack();
        if stack.is_null() {
            return "skipped".to_string();
        }
        raw_process_syscall(libc::SYS_clone, flags, stack)
    }

    /// Raw syscalls return -errno in rax; anything else is a created child
    /// (already gone, see above) to reap.
    fn asm_outcome(rv: i64) -> String {
        if (-4095..0).contains(&rv) {
            return format!("errno={}", -rv);
        }
        unsafe { libc::waitpid(rv as libc::c_int, std::ptr::null_mut(), 0) };
        "ok".to_string()
    }

    /// A mapped stack, as CLONE_VM shapes require; the child never runs
    /// because every probed shape is denied.
    fn clone_stack() -> *mut libc::c_void {
        let page = 64 * 1024;
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                page,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            std::ptr::null_mut()
        } else {
            (base as usize + page - 64) as *mut libc::c_void
        }
    }

    fn probe_clone3() -> String {
        let args = [0u8; 64]; // struct clone_args, all zero
        syscall_outcome(unsafe { libc::syscall(libc::SYS_clone3, args.as_ptr(), args.len()) })
    }

    fn probe_thread() -> String {
        match std::thread::spawn(|| "ok".to_string()).join() {
            Ok(outcome) => outcome,
            Err(_) => "panicked".to_string(),
        }
    }

    /// Outcome of libc's fork/clone3 wrappers (-1 plus errno on failure).
    fn syscall_outcome(rv: libc::c_long) -> String {
        if rv == -1 {
            return errno_last();
        }
        if rv == 0 {
            // Child of an unexpected success. SYS_exit, not _exit: a
            // CLONE_THREAD child must not exit_group the whole probe.
            unsafe { libc::syscall(libc::SYS_exit, 0) };
        }
        unsafe { libc::waitpid(rv as libc::c_int, std::ptr::null_mut(), 0) };
        "ok".to_string()
    }

    fn raw<T: Into<i64>>(rv: T) -> String {
        if rv.into() == -1 {
            errno_last()
        } else {
            "ok".to_string()
        }
    }

    fn io(result: std::io::Result<()>) -> String {
        match result {
            Ok(()) => "ok".to_string(),
            Err(e) => match e.raw_os_error() {
                Some(n) => format!("errno={}", n),
                None => format!("err={}", e),
            },
        }
    }

    fn errno_last() -> String {
        format!("errno={}", std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }

    fn thread_count() -> usize {
        std::fs::read_dir("/proc/self/task").map(|tasks| tasks.count()).unwrap_or(0)
    }

    fn report(prefix: &str, probes: &[Probe]) {
        for (name, outcome) in probes {
            println!("PROBE|{}{}|{}", prefix, name, outcome);
        }
    }

    // ---- what each mode must report ----

    /// Parent-aimed probes that Landlock's Signal and abstract-socket scopes
    /// stop without any seccomp rule.
    const SCOPED_PARENT: &[&str] = &[
        "kill-parent",
        "tgkill-parent",
        "tkill-parent",
        "sigqueue-parent",
        "rt-tgsigqueue-parent",
        "connect-abstract-parent",
    ];

    /// Probes with no scope behind them: only a seccomp rule stops these.
    const SECCOMP_ONLY: &[&str] = &[
        "socket",
        "exec",
        "fork",
        "sys-fork",
        "sys-vfork",
        "clone-newuser",
        "ioctl-tiocsti",
        "ioctl-tiocsti-hibit",
        "ioctl-fiosetown-parent",
        "ioctl-siocspgrp-parent",
        "fcntl-setsig",
        "fcntl-setown-parent",
        "fcntl-setown-ex-parent",
        "inotify-init",
        "inotify-watch",
        "fanotify-init",
        "fanotify-mark",
        "mq-open",
        "mq-timedsend",
        "mq-timedreceive",
        "mq-notify",
        "mq-getsetattr",
        "mq-unlink-parent",
        "setpriority-pgrp",
        "setpriority-user",
        "ioprio-set-pgrp",
        "ioprio-set-user",
    ];

    /// Probes refused by the range rule in the stacked guard: newer-than-
    /// list syscalls get ENOSYS, exactly like an unknown syscall.
    const NEW_SYSCALLS: [&str; 7] = [
        "fchmodat2",
        "setxattrat",
        "getxattrat",
        "listxattrat",
        "removexattrat",
        "file-getattr",
        "file-setattr",
    ];

    /// Probes just below the floor of the range rule.
    const BELOW_FLOOR: [&str; 2] = ["cachestat", "futex-waitv"];

    /// Every probe that fails with EPERM once the seccomp layer is in.
    fn seccomp_denied() -> Vec<String> {
        let mut names: Vec<String> = SECCOMP_ONLY.iter().chain(SCOPED_PARENT).map(|n| n.to_string()).collect();
        names.extend(TARGET_PROBES.iter().map(|(name, _)| format!("{}-parent", name)));
        names.extend(clone_probe_names());
        names
    }

    /// Calls that must keep working under every layer.
    fn always_ok() -> Vec<String> {
        let mut names: Vec<String> =
            ["read-urandom", "kill-self", "thread-spawn", "write-dump"].map(String::from).to_vec();
        for (name, _) in TARGET_PROBES {
            names.push(format!("{}-self-0", name));
            names.push(format!("{}-self-pid", name));
        }
        names
    }

    const FS_DENIED: [&str; 4] = ["read-etc-passwd", "write-existing", "write-new", "read-dump"];

    type Expected = Vec<(String, String)>;

    fn expect(names: impl IntoIterator<Item = impl AsRef<str>>, outcome: &str) -> Expected {
        names.into_iter().map(|n| (n.as_ref().to_string(), outcome.to_string())).collect()
    }

    fn errno_str(errno: libc::c_int) -> String {
        format!("errno={}", errno)
    }

    /// Both layers: filesystem denied by Landlock, the rest by seccomp.
    fn full_expectations() -> Expected {
        let mut all = expect(FS_DENIED, &errno_str(libc::EACCES));
        all.extend(expect(seccomp_denied(), &errno_str(libc::EPERM)));
        all.extend(expect(["clone3"].into_iter().chain(NEW_SYSCALLS), &errno_str(libc::ENOSYS)));
        all.extend(expect(always_ok(), "ok"));
        all
    }

    /// The below-floor calls must fail on their own invalid arguments, with
    /// any errno but ENOSYS (which is what the range rule would return if its
    /// floor sat too low). The kernel supports them: this runs only where
    /// Landlock ABI 6 (Linux 6.12) does.
    fn check_below_floor(lines: &[String], prefix: &str) {
        let enosys = errno_str(libc::ENOSYS);
        for name in BELOW_FLOOR {
            let outcome = result(lines, &format!("{}{}", prefix, name));
            assert!(
                outcome.starts_with("errno=") && outcome != enosys,
                "{}{} must reach the kernel and fail on its arguments, got {}",
                prefix,
                name,
                outcome
            );
        }
    }

    fn check(lines: &[String], prefix: &str, expected: &Expected) {
        for (name, outcome) in expected {
            let key = format!("{}{}", prefix, name);
            assert_eq!(result(lines, &key), outcome, "{} outcome", key);
        }
    }

    // ---- the test cases (run when no probe mode is given) ----

    struct Case {
        name: &'static str,
        run: fn(),
        /// Needs the real kernel layers (Landlock ABI 6). Cases that fake a
        /// missing feature, or use only seccomp, run on any kernel.
        needs_kernel: bool,
    }

    const CASES: &[Case] = &[
        Case { name: "full_sandbox_denies_exec_network_process_and_signals", run: full_sandbox_denies, needs_kernel: true },
        Case { name: "full_sandbox_writes_only_the_dump_file", run: full_sandbox_dump_only, needs_kernel: true },
        Case { name: "sandbox_restrictions_reach_threads_spawned_after_apply", run: threads_inherit, needs_kernel: true },
        Case { name: "seccomp_layer_alone_denies_exec_network_process_creation", run: seccomp_alone, needs_kernel: false },
        Case { name: "landlock_layer_alone_denies_filesystem_and_scopes", run: landlock_alone, needs_kernel: true },
        Case { name: "sandbox_refuses_when_other_threads_already_run", run: refuses_late_thread, needs_kernel: false },
        Case { name: "sandbox_refuses_when_v8_is_already_initialized", run: refuses_late_v8, needs_kernel: false },
        Case { name: "sandbox_refuses_when_landlock_is_missing", run: refuses_no_landlock, needs_kernel: false },
        Case { name: "sandbox_refuses_when_seccomp_is_missing", run: refuses_no_seccomp, needs_kernel: false },
        Case { name: "foreign_abi_x32_call_is_killed", run: x32_call_killed, needs_kernel: false },
        Case { name: "foreign_abi_int80_call_is_killed", run: int80_call_killed, needs_kernel: false },
        Case { name: "live_run_threads_all_restricted", run: live_threads_restricted, needs_kernel: true },
        Case { name: "dump_not_truncated_before_the_snapshot", run: dump_not_truncated_early, needs_kernel: true },
        Case { name: "every_denied_syscall_is_denied", run: denied_enumeration, needs_kernel: false },
        Case { name: "harness_selection_flags_list_skip_and_exact", run: harness_selection, needs_kernel: false },
        Case { name: "harness_skips_cases_on_a_kernel_without_landlock", run: harness_old_kernel, needs_kernel: false },
    ];

    enum Outcome {
        Passed,
        Failed,
        Skipped,
    }

    fn run_tests(opts: &Options) -> i32 {
        let guarded = std::env::var_os(HARNESS_CHILD).is_some();
        let selected: Vec<&Case> = CASES
            .iter()
            .filter(|case| !(guarded && case.name.starts_with("harness_")))
            .filter(|case| opts.selects(case.name))
            .collect();
        if opts.list {
            for case in selected {
                println!("{}: test", case.name);
            }
            return 0;
        }
        let (mut ran, mut failed, mut skipped) = (0, 0, 0);
        for case in selected {
            match run_case(case) {
                Outcome::Passed => ran += 1,
                Outcome::Failed => {
                    ran += 1;
                    failed += 1;
                }
                Outcome::Skipped => skipped += 1,
            }
        }
        println!("sandbox test result: {} run; {} failed; {} skipped", ran, failed, skipped);
        // A filter that matches nothing is fine (see the module comment); an
        // unfiltered run that ran nothing must not look like a pass.
        if ran + skipped == 0 && opts.filters.is_empty() {
            eprintln!("no sandbox tests ran");
            return 1;
        }
        if failed > 0 {
            101
        } else {
            0
        }
    }

    fn run_case(case: &Case) -> Outcome {
        if case.needs_kernel && !common::kernel_supports_sandbox() {
            if common::sandbox_required() {
                println!(
                    "test {} ... FAILED (kernel lacks Landlock ABI {} and V8VM_REQUIRE_SANDBOX=1)",
                    case.name,
                    common::REQUIRED_LANDLOCK_ABI
                );
                return Outcome::Failed;
            }
            common::skipped(case.name);
            println!("test {} ... skipped", case.name);
            return Outcome::Skipped;
        }
        let passed = std::panic::catch_unwind(case.run).is_ok();
        println!("test {} ... {}", case.name, if passed { "ok" } else { "FAILED" });
        if passed {
            Outcome::Passed
        } else {
            Outcome::Failed
        }
    }

    /// Describes how the test table and the library's deny list differ: the
    /// entries only the list has (named by the table when it knows them, else
    /// by number), the ones only the table has, or a pure reordering.
    fn table_drift(table: &[(libc::c_long, &str)], list: &[libc::c_long]) -> String {
        let name = |nr: libc::c_long| {
            table.iter().find(|(n, _)| *n == nr).map_or_else(|| nr.to_string(), |(_, name)| name.to_string())
        };
        let only_in_list: Vec<String> =
            list.iter().filter(|nr| !table.iter().any(|(n, _)| n == *nr)).map(|nr| name(*nr)).collect();
        let only_in_table: Vec<String> =
            table.iter().filter(|(nr, _)| !list.contains(nr)).map(|(nr, _)| name(*nr)).collect();
        if only_in_list.is_empty() && only_in_table.is_empty() {
            return "the test table and the deny list hold the same syscalls in a different order".to_string();
        }
        format!(
            "the test table and the deny list disagree: in the list but not the table: {:?}; in the table but not the list: {:?}",
            only_in_list, only_in_table
        )
    }

    /// The whole deny list, one syscall at a time, against a filter whose
    /// denials return an errno nothing else produces: every entry must
    /// report exactly that errno, and the table in this file must match the
    /// list the library exports.
    fn denied_enumeration() {
        let expected_nrs = denied_syscalls();
        let table = denied_table();
        let table_nrs: Vec<libc::c_long> = table.iter().map(|(nr, _)| *nr).collect();
        assert!(table_nrs == expected_nrs, "{}", table_drift(&table, &expected_nrs));
        let lines = run_probe("deny-enum", &[]);
        assert_eq!(result(&lines, "deny-enum"), "ok", "apply must succeed");
        let want = format!("errno={}", PROBE_DENY_ERRNO);
        let mut keys: Vec<String> = table.iter().map(|(_, name)| format!("deny|{}", name)).collect();
        keys.extend(NAMESPACE_PROBE_FLAGS.iter().map(|(name, _)| format!("deny|clone-thread+{}", name)));
        keys.extend(["deny|clone-vm-nothread", "deny|ioctl-tioclinux"].map(String::from));
        let wrong: Vec<String> = keys
            .iter()
            .filter_map(|key| match lines.iter().find_map(|l| l.strip_prefix(&format!("PROBE|{}|", key))) {
                None => Some(format!("{}: no result", key)),
                Some(o) if o != want => Some(format!("{}: {}", key, o)),
                _ => None,
            })
            .collect();
        assert!(wrong.is_empty(), "deny list mismatches: {:?}", wrong);
        // every listed syscall was actually probed
        assert_eq!(result(&lines, "deny|checked"), &table.len().to_string());
    }

    /// The harness's own argument handling, driven through cheap or listing
    /// invocations of this binary. Each child is marked so it skips the
    /// harness cases (they would otherwise respawn themselves forever).
    fn harness_selection() {
        let case_names: Vec<&str> = CASES
            .iter()
            .map(|c| c.name)
            .filter(|name| !name.starts_with("harness_"))
            .collect();

        // libtest flags are ignored, not taken for filters.
        let out = harness_run(&["--list", "--nocapture", "--test-threads=1", "--test-threads", "1"]);
        assert!(out.status.success(), "flags must not break --list");
        let listed = String::from_utf8_lossy(&out.stdout).into_owned();
        for name in &case_names {
            assert!(listed.contains(&format!("{}: test", name)), "--list misses {}: {}", name, listed);
        }

        // Probe-mode names are plain filters now, not modes: `x32` lists the
        // case and must not run (or die in) the x32 probe.
        let out = harness_run(&["--list", "x32"]);
        assert!(out.status.success(), "x32 filter: {:?}", out.status);
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "foreign_abi_x32_call_is_killed: test");

        // --skip removes cases, in both spellings.
        let out = harness_run(&["--list", "--skip", "refuses", "--skip=foreign_abi"]);
        let listed = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(!listed.contains("refuses") && !listed.contains("foreign_abi"), "skip ignored: {}", listed);
        assert!(listed.contains("full_sandbox_denies"), "skip removed too much: {}", listed);

        // --exact needs the whole name.
        let whole = "sandbox_refuses_when_other_threads_already_run";
        assert!(String::from_utf8_lossy(&harness_run(&["--list", "--exact", "refuses_when_other"]).stdout).is_empty());
        let out = harness_run(&["--list", "--exact", whole]);
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), format!("{}: test", whole));

        // A filter that matches nothing passes with "0 run" (cargo passes
        // another target's test name to this one); an unfiltered run in
        // which nothing ran does not.
        let out = harness_run(&["no-such-test-name"]);
        assert!(out.status.success(), "unmatched filter must pass: {:?}", out.status);
        assert!(String::from_utf8_lossy(&out.stdout).contains("0 run"));
        let out = harness_run(&["--skip", "_"]);
        assert!(!out.status.success(), "an unfiltered run of nothing must fail");
        assert!(String::from_utf8_lossy(&out.stderr).contains("no sandbox tests ran"));
    }

    fn harness_run(args: &[&str]) -> Output {
        run_bounded(harness_command().args(args))
    }

    fn harness_command() -> Command {
        let mut cmd = Command::new(std::env::current_exe().expect("test binary path"));
        cmd.env(HARNESS_CHILD, "1");
        cmd
    }

    /// On a kernel without Landlock, cases that need it print SKIPPED and
    /// pass, the fail-closed case still runs, and V8VM_REQUIRE_SANDBOX=1
    /// turns the skip into a failure.
    fn harness_old_kernel() {
        let needs_kernel = "full_sandbox_denies_exec_network_process_and_signals";
        let fail_closed = "sandbox_refuses_when_seccomp_is_missing";
        let mut cmd = harness_command();
        cmd.arg(format!("{}old-kernel", PROBE_PREFIX)).args([needs_kernel, fail_closed]);

        let lenient = run_bounded(cmd.env_remove("V8VM_REQUIRE_SANDBOX"));
        let stdout = String::from_utf8_lossy(&lenient.stdout);
        let stderr = String::from_utf8_lossy(&lenient.stderr);
        assert!(lenient.status.success(), "skip must pass: {} {}", stdout, stderr);
        let notice = format!("SKIPPED (kernel lacks Landlock ABI 6): {}", needs_kernel);
        assert!(stderr.contains(&notice), "missing skip notice: {}", stderr);
        assert!(stdout.contains("1 run; 0 failed; 1 skipped"), "fail-closed case must run: {}", stdout);

        let strict = run_bounded(cmd.env("V8VM_REQUIRE_SANDBOX", "1"));
        assert!(!strict.status.success(), "V8VM_REQUIRE_SANDBOX=1 must fail the skip");
        assert!(String::from_utf8_lossy(&strict.stdout).contains("FAILED (kernel lacks Landlock"));
    }

    fn scratch(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
    }

    fn write_guest(name: &str, source: &str) -> PathBuf {
        let path = scratch(&format!("{}.js", name));
        std::fs::write(&path, source).expect("write guest");
        path
    }

    /// Per-test scratch paths; the escape file pre-exists so write-existing
    /// exercises open+truncate, exactly what the dump write does.
    fn probe_setup(name: &str) -> (PathBuf, PathBuf) {
        let dump = scratch(&format!("{}-dump.json", name));
        let escape = scratch(&format!("{}-escape", name));
        std::fs::create_dir_all(&escape).expect("escape dir");
        std::fs::write(escape.join("escape.txt"), "x").expect("escape file");
        (dump, escape)
    }

    /// Runs a command to completion, killing it after a minute: a runaway
    /// child (the failure mode these tests exist to catch) must fail here
    /// instead of multiplying.
    fn run_bounded(cmd: &mut Command) -> Output {
        const LIMIT: Duration = Duration::from_secs(60);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn bounded child");
        let deadline = Instant::now() + LIMIT;
        while child.try_wait().expect("poll bounded child").is_none() {
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("child still running after {:?}", LIMIT);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        child.wait_with_output().expect("collect bounded child output")
    }

    fn self_exec(mode: &str) -> Command {
        let mut cmd = Command::new(std::env::current_exe().expect("test binary path"));
        cmd.arg(format!("{}{}", PROBE_PREFIX, mode)).env("PROBE_PARENT", std::process::id().to_string());
        cmd
    }

    fn run_probe(mode: &str, args: &[String]) -> Vec<String> {
        let out = run_bounded(self_exec(mode).args(args));
        assert!(
            out.status.success(),
            "probe child {} {:?} failed: {}",
            mode,
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .expect("utf8 probe output")
            .lines()
            .filter(|line| line.starts_with("PROBE|"))
            .map(String::from)
            .collect()
    }

    fn result<'a>(lines: &'a [String], name: &str) -> &'a str {
        lines
            .iter()
            .find_map(|line| line.strip_prefix(&format!("PROBE|{}|", name)))
            .unwrap_or_else(|| panic!("no result for {} in {:?}", name, lines))
    }

    /// Runs a layer mode against fresh fixtures in this process.
    fn layer_mode(mode: &str, setup: &str) -> Vec<String> {
        let _fixtures = Fixtures::create();
        let (dump, escape) = probe_setup(setup);
        run_probe(mode, &[path_string(&dump), path_string(&escape)])
    }

    fn path_string(path: &Path) -> String {
        path.to_str().expect("utf8 path").to_string()
    }

    fn full_sandbox_denies() {
        let lines = layer_mode("probe", "deny");
        assert_eq!(result(&lines, "threads"), "1", "apply starts from one thread");
        check(&lines, "", &full_expectations());
        check_below_floor(&lines, "");
    }

    fn full_sandbox_dump_only() {
        let lines = layer_mode("probe", "dump-only");
        assert_eq!(result(&lines, "write-dump"), "ok", "the dump write must pass");
        check(&lines, "", &expect(["read-dump", "write-existing", "write-new"], &errno_str(libc::EACCES)));
        let content = std::fs::read_to_string(scratch("dump-only-dump.json")).expect("dump file");
        assert_eq!(content, "{\"probe\":1}");
    }

    /// Everything a thread spawned after apply does must be denied exactly
    /// like the main thread's own calls.
    fn threads_inherit() {
        let lines = layer_mode("probe", "thr");
        check(&lines, "thr-", &full_expectations());
        check_below_floor(&lines, "thr-");
    }

    /// seccomp alone, so every denial here is provably from a seccomp rule
    /// and not from Landlock's rights or scopes.
    fn seccomp_alone() {
        let lines = layer_mode("seccomp-only", "seccomp");
        let mut expected = expect(seccomp_denied(), &errno_str(libc::EPERM));
        expected.extend(expect(["clone3"].into_iter().chain(NEW_SYSCALLS), &errno_str(libc::ENOSYS)));
        expected.extend(expect(always_ok(), "ok"));
        // Landlock is absent in this mode: these succeed, proving the layer
        // isolation works in the other direction too.
        expected.extend(expect(FS_DENIED.iter().chain(&["write-dump"]), "ok"));
        check(&lines, "", &expected);
    }

    fn landlock_alone() {
        let lines = layer_mode("landlock-only", "landlock");
        // Execute is a Landlock right too: this is exactly why the exec
        // denial has to be asserted in the seccomp-only run above.
        let mut expected = expect(FS_DENIED.iter().chain(&["exec"]), &errno_str(libc::EACCES));
        // The Signal and abstract-socket scopes stop what seccomp would
        // otherwise be the only wall for.
        expected.extend(expect(SCOPED_PARENT, &errno_str(libc::EPERM)));
        // Everything else is seccomp's job: with it absent these succeed.
        expected.extend(expect(["fcntl-setown-parent", "clone3", "fchmodat2"], "ok"));
        expected.extend(expect(always_ok(), "ok"));
        check(&lines, "", &expected);
    }

    /// Runs a refusal mode and checks the refusal names `needle`.
    fn assert_refused(mode: &str, needle: &str) {
        let lines = run_probe(mode, &[]);
        let outcome = result(&lines, mode);
        assert!(outcome.starts_with("refused:"), "apply must refuse: {:?}", lines);
        assert!(outcome.contains(needle), "refusal must name {:?}: {:?}", needle, lines);
    }

    fn refuses_late_thread() {
        assert_refused("late-thread", "thread");
    }

    fn refuses_late_v8() {
        assert_refused("late-v8", "run()/serve()");
    }

    fn refuses_no_landlock() {
        assert_refused("no-landlock", "Landlock");
    }

    fn refuses_no_seccomp() {
        assert_refused("no-seccomp", "seccomp");
    }

    fn x32_call_killed() {
        assert_killed_by("x32", libc::SIGSYS);
    }

    /// Where the kernel delivers `int 0x80` to seccomp it dies with SIGSYS.
    /// With ia32 emulation off (CONFIG_IA32_EMULATION_DEFAULT_DISABLED or
    /// ia32_emulation=off on the command line) the kernel kills it with
    /// SIGSEGV first, before seccomp can see it; the unsandboxed
    /// `int80-bare` mode tells the two hosts apart.
    fn int80_call_killed() {
        let bare = run_bounded(&mut self_exec("int80-bare"));
        let expected = match bare.status.signal() {
            None => libc::SIGSYS,
            Some(libc::SIGSEGV) => {
                println!("int80: ia32 emulation is disabled on this host; accepting SIGSEGV instead of SIGSYS");
                libc::SIGSEGV
            }
            Some(other) => panic!("bare int 0x80 died with unexpected signal {}", other),
        };
        assert_killed_by("int80", expected);
    }

    /// A foreign-ABI mode must announce itself, never return from the call,
    /// and die with `signal`.
    fn assert_killed_by(mode: &str, signal: i32) {
        let out = run_bounded(&mut self_exec(mode));
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.lines().any(|l| l == format!("PROBE|{}|armed", mode)), "armed marker missing: {}", stdout);
        assert!(!stdout.contains("survived"), "foreign-abi call returned instead of dying: {}", stdout);
        assert_eq!(out.status.signal(), Some(signal), "{} must die with signal {}: {:?}", mode, signal, out.status);
    }

    /// The real binary under --sandbox: every thread it creates (V8's worker
    /// pool, watchdog, stdin reader) must carry the seccomp filter and
    /// no_new_privs.
    fn live_threads_restricted() {
        const LIMIT: Duration = Duration::from_secs(60);
        let script = write_guest("alive", "vm.stdin.read().then(() => console.log('done'));");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_v8vm"));
        cmd.args(["--sandbox", script.to_str().unwrap()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn v8vm");
        let deadline = Instant::now() + LIMIT;
        // 5 is a floor, not an exact count: the main, host, guest and watchdog
        // threads plus the stdin reader that vm.stdin.read() starts. V8 workers
        // come on top, and their number depends on the CPU count.
        let statuses = wait_for_statuses(child.id(), 5, Duration::from_secs(20));
        assert!(statuses.len() > 1, "V8 worker threads must exist");
        assert!(statuses.len() >= 5, "expected the v8vm main, host, guest and watchdog threads plus V8 workers, saw {}", statuses.len());
        for status in &statuses {
            assert!(status.contains("Seccomp:\t2"), "no seccomp filter: {}", first_lines(status));
            assert!(status.contains("NoNewPrivs:\t1"), "no no_new_privs: {}", first_lines(status));
            // Three filters are installed: clone3 ENOSYS, the x32 guard and
            // the deny list. The field exists since Linux 6.8 and the
            // sandbox needs 6.12, so its absence is a failure.
            let count = seccomp_filters(status).expect("Seccomp_filters field in /proc/<pid>/task/<tid>/status");
            assert_eq!(count, 3, "filter count: {}", first_lines(status));
        }
        drop(child.stdin.take()); // release stdin so the guest can finish
        while child.try_wait().expect("poll v8vm").is_none() {
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("v8vm still running after {:?}", LIMIT);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let out = child.wait_with_output().expect("collect v8vm output");
        assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// The dump file is created without O_TRUNC: a run that dies before the
    /// snapshot leaves an existing file untouched, and a run that reaches it
    /// replaces the content.
    fn dump_not_truncated_early() {
        let dump = scratch("early-dump.json");
        let preload = scratch("bad-preload.json");
        std::fs::write(&dump, "OLD").expect("seed dump");
        std::fs::write(&preload, "{ not json").expect("bad preload");
        let script = write_guest("early", "console.log('ran');");
        let out = v8vm_output(&[
            "--sandbox",
            &format!("--preload={}", preload.display()),
            &format!("--dump={}", dump.display()),
            script.to_str().unwrap(),
        ]);
        assert_eq!(out.status.code(), Some(2));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "", "guest must not run");
        assert_eq!(std::fs::read_to_string(&dump).unwrap(), "OLD");

        let ok = write_guest("early-ok", "vm.fs.write('/k', 'v');");
        let out = v8vm_output(&["--sandbox", &format!("--dump={}", dump.display()), ok.to_str().unwrap()]);
        assert_eq!(out.status.code(), Some(0));
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&dump).unwrap()).expect("dump json");
        assert_eq!(
            json,
            serde_json::json!({"dirs": ["/"], "files": {"/k": {"text": "v"}}, "version": 2})
        );
    }

    fn v8vm_output(args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_v8vm"));
        run_bounded(cmd.args(args))
    }

    fn wait_for_statuses(pid: u32, want: usize, within: Duration) -> Vec<String> {
        let deadline = Instant::now() + within;
        loop {
            let statuses = read_thread_statuses(pid);
            if statuses.len() >= want || Instant::now() > deadline {
                return statuses;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn read_thread_statuses(pid: u32) -> Vec<String> {
        let mut statuses = Vec::new();
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{}/task", pid)) else {
            return statuses;
        };
        for task in tasks.flatten() {
            if let Ok(status) = std::fs::read_to_string(task.path().join("status")) {
                statuses.push(status);
            }
        }
        statuses
    }

    fn first_lines(status: &str) -> String {
        status.lines().take(2).collect::<Vec<_>>().join(" / ")
    }

    fn seccomp_filters(status: &str) -> Option<u32> {
        status
            .lines()
            .find_map(|l| l.strip_prefix("Seccomp_filters:"))
            .and_then(|v| v.trim().parse().ok())
    }
}

fn main() {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    run::main();
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    println!("sandbox tests: unsupported platform (need Linux x86_64), 0 run");
}
