//! Tests for `--sandbox`. This target has no libtest harness (`harness =
//! false`): `sandbox::apply` refuses to run once any thread exists and libtest
//! always runs tests on a worker thread, so each probe re-executes this same
//! binary as a single-threaded child with a mode argument:
//!
//!   probe <dump> <escape_dir>   full sandbox, then probe syscalls
//!   landlock-only <dump> <dir>  Landlock layer alone
//!   seccomp-only <dump> <dir>   seccomp layer alone
//!   late-thread | late-v8       apply too late (refused)
//!   no-landlock | no-seccomp    kernel feature missing (refused)
//!
//! Probe output is "PROBE|<name>|<outcome>" with outcome `ok`, `skipped`, or
//! `errno=<n>` (the raw errno, so a test can tell Landlock's EACCES from
//! seccomp's EPERM).

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod run {
    use std::collections::BTreeMap;
    use std::convert::TryInto;
    use std::ffi::CString;
    use std::path::{Path, PathBuf};
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Output, Stdio};
    use std::time::{Duration, Instant};

    pub fn main() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if !dispatch_mode(&args) {
            // cargo forwards libtest-style flags (--nocapture,
            // --test-threads=1) to every test target; they are not filters.
            let filter = args.iter().find(|a| !a.starts_with('-')).map(String::as_str);
            std::process::exit(run_tests(filter));
        }
    }

    // ---- probe modes (run in a fresh, single-threaded child) ----

    fn dispatch_mode(args: &[String]) -> bool {
        let mode = match args.first() {
            Some(m) => m.as_str(),
            None => return false,
        };
        let path = |i: usize| PathBuf::from(&args[i]);
        match mode {
            "probe" => mode_apply(None, path(1), path(2)),
            "landlock-only" => mode_apply(Some(v8vm::sandbox::Layer::Landlock), path(1), path(2)),
            "seccomp-only" => mode_apply(Some(v8vm::sandbox::Layer::Seccomp), path(1), path(2)),
            "late-thread" => mode_late_thread(),
            "late-v8" => mode_late_v8(),
            "no-landlock" => mode_missing("no-landlock", libc::SYS_landlock_create_ruleset),
            "no-seccomp" => mode_missing("no-seccomp", libc::SYS_seccomp),
            "x32" => mode_foreign_abi(AbiCall::X32),
            "int80" => mode_foreign_abi(AbiCall::Int80),
            _ => return false,
        }
        true
    }

    fn mode_apply(layer: Option<v8vm::sandbox::Layer>, dump: PathBuf, escape_dir: PathBuf) {
        println!("PROBE|threads|{}", thread_count());
        let applied = match layer {
            None => v8vm::sandbox::apply(Some(&dump)),
            Some(layer) => v8vm::sandbox::apply_layer(layer, Some(&dump)),
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

    /// Which foreign syscall ABI the mode should invoke after apply. Both
    /// must kill the process with SIGSYS through the stacked x32 guard:
    /// seccomp acts before syscall dispatch, so this is testable even on a
    /// host whose kernel would otherwise reject the call.
    enum AbiCall {
        X32,
        Int80,
    }

    fn mode_foreign_abi(call: AbiCall) {
        let dump = scratch("abi-dump.json");
        v8vm::sandbox::apply(Some(&dump)).expect("apply before the foreign-abi call");
        match call {
            AbiCall::X32 => {
                println!("PROBE|x32|armed");
                // A syscall number in the x32 range (nr | 0x40000000).
                let rv = unsafe { libc::syscall(0x4000_0000 | libc::SYS_getpid) };
                println!("PROBE|x32-getpid-survived|{}", raw(rv as libc::c_int));
            }
            AbiCall::Int80 => {
                println!("PROBE|int80|armed");
                // int 0x80 arrives with the i386 arch value; eax=20 is
                // getpid. rbx cannot be an inline-asm operand on x86_64, so
                // the caller-saved registers are preserved by hand.
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
                println!("PROBE|int80-survived|ok");
            }
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
        report_apply(name, v8vm::sandbox::apply(None));
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

    fn probe_all(dump: &Path, escape_dir: &Path) -> Vec<(String, String)> {
        let parent = std::env::var("PROBE_PARENT").ok().and_then(|p| p.parse().ok());
        let mut probes = vec![
            ("read-etc-passwd".into(), io(std::fs::File::open("/etc/passwd").map(|_| ()))),
            ("read-urandom".into(), io(std::fs::File::open("/dev/urandom").map(|_| ()))),
            ("write-existing".into(), io(std::fs::write(escape_dir.join("escape.txt"), "x"))),
            ("write-new".into(), io(std::fs::write(escape_dir.join("new.txt"), "x"))),
            ("write-dump".into(), io(std::fs::write(dump, "{\"probe\":1}"))),
            ("read-dump".into(), io(std::fs::File::open(dump).map(|_| ()))),
            ("socket".into(), raw(unsafe {
                libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0)
            })),
            ("exec".into(), probe_execve()),
            ("fork".into(), syscall_outcome(unsafe { libc::fork() as libc::c_long })),
            (
                "clone-newuser".into(),
                probe_clone((libc::CLONE_NEWUSER | libc::SIGCHLD) as libc::c_ulong),
            ),
            ("clone3".into(), probe_clone3()),
            ("thread-spawn".into(), probe_thread()),
            ("kill-self".into(), raw(unsafe { libc::kill(libc::getpid(), 0) })),
            ("kill-parent".into(), match parent {
                None => "skipped".to_string(),
                Some(pid) => raw(unsafe { libc::kill(pid, 0) }),
            }),
            ("tgkill-parent".into(), match parent {
                None => "skipped".to_string(),
                Some(pid) => raw(unsafe { libc::tgkill(pid, pid, 0) }),
            }),
            (
                "ioctl-tiocsti".into(),
                raw(unsafe { libc::ioctl(0, libc::TIOCSTI as libc::c_ulong, b'x' as *const u8) }),
            ),
            (
                // The kernel ignores the high 32 bits of an ioctl request, so
                // the deny must compare the low dword only.
                "ioctl-tiocsti-hibit".into(),
                raw(unsafe {
                    libc::ioctl(0, (libc::TIOCSTI as libc::c_ulong) | 1 << 32, b'x' as *const u8)
                }),
            ),
            (
                "fcntl-setown-other".into(),
                match parent {
                    None => "skipped".to_string(),
                    Some(pid) => raw(unsafe {
                        libc::fcntl(pipe_w(), libc::F_SETOWN, pid)
                    }),
                },
            ),
            ("sigqueue-parent".into(), match parent {
                None => "skipped".to_string(),
                Some(pid) => {
                    // seccomp denies on the pid argument before the kernel
                    // ever validates the siginfo, so its content is moot.
                    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                    info.si_code = libc::SI_QUEUE;
                    raw(unsafe {
                        libc::syscall(libc::SYS_rt_sigqueueinfo, pid, 0, &info) as libc::c_int
                    })
                }
            }),
            ("rt-tgsigqueue-parent".into(), match parent {
                None => "skipped".to_string(),
                Some(pid) => {
                    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                    info.si_code = libc::SI_QUEUE;
                    raw(unsafe {
                        libc::syscall(libc::SYS_rt_tgsigqueueinfo, pid, pid, 0, &info) as libc::c_int
                    })
                }
            }),
            ("prlimit-parent".into(), match parent {
                None => "skipped".to_string(),
                Some(pid) => {
                    let mut rl: libc::rlimit = unsafe { std::mem::zeroed() };
                    raw(unsafe {
                        libc::syscall(libc::SYS_prlimit64, pid, libc::RLIMIT_NOFILE, 0, &mut rl)
                            as libc::c_int
                    })
                }
            }),
            (
                "prlimit-self".into(),
                raw(unsafe {
                    let mut rl: libc::rlimit = std::mem::zeroed();
                    libc::syscall(libc::SYS_prlimit64, 0, libc::RLIMIT_NOFILE, 0, &mut rl)
                        as libc::c_int
                }),
            ),
            ("setpriority-other".into(), match parent {
                None => "skipped".to_string(),
                Some(pid) => raw(unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as libc::c_uint, 5) }),
            }),
            ("sched-setaffinity-other".into(), match parent {
                None => "skipped".to_string(),
                Some(pid) => {
                    let set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
                    raw(unsafe {
                        libc::syscall(
                            libc::SYS_sched_setaffinity,
                            pid,
                            std::mem::size_of::<libc::cpu_set_t>(),
                            &set,
                        ) as libc::c_int
                    })
                }
            }),
            (
                "inotify-init".into(),
                raw(unsafe { libc::inotify_init1(0) }),
            ),
            (
                "inotify-watch".into(),
                raw(unsafe { libc::inotify_add_watch(-1, c"/home".as_ptr(), 0xfff) }),
            ),
        ];
        // clone shapes: a thread that also creates a namespace, and the
        // vfork/CLONE_VM forms used to start another program.
        let thread = (libc::CLONE_THREAD | libc::CLONE_SIGHAND | libc::CLONE_VM) as libc::c_ulong;
        for (name, flag) in NAMESPACE_PROBE_FLAGS {
            probes.push((
                format!("clone-thread+{}", name),
                probe_clone(thread | flag),
            ));
        }
        probes.push((
            "clone-vfork".into(),
            probe_clone((libc::CLONE_VM | libc::CLONE_VFORK | libc::SIGCHLD) as libc::c_ulong),
        ));
        probes.push((
            "clone-vm-nothread".into(),
            probe_clone((libc::CLONE_VM | libc::SIGCHLD) as libc::c_ulong),
        ));
        probes
    }

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

    /// A direct execve: the only way to attribute the errno to seccomp rather
    /// than to std::process' clone-based spawn path.
    fn probe_execve() -> String {
        let path = CString::new("/bin/true").unwrap();
        let argv = [path.as_ptr(), std::ptr::null()];
        let envp: [*const libc::c_char; 1] = [std::ptr::null()];
        let rv = unsafe {
            libc::syscall(libc::SYS_execve, path.as_ptr(), argv.as_ptr(), envp.as_ptr())
        };
        // execve only returns when it fails; success would replace the child.
        assert_eq!(rv, -1, "execve unexpectedly returned {}", rv);
        errno_last()
    }

    /// Raw clone with the child handled entirely in asm: returning through
    /// libc's syscall wrapper would use the parent's frame on the child's
    /// new stack (garbage), so the child exits before any Rust runs.
    fn probe_clone(flags: libc::c_ulong) -> String {
        let stack = clone_stack();
        if stack.is_null() {
            return "skipped".to_string();
        }
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
                inlateout("rax") libc::SYS_clone => rv,
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
        syscall_outcome(rv)
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

    fn raw(rv: libc::c_int) -> String {
        if rv == -1 {
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

    /// A fresh pipe write end for the F_SETOWN probe (fd 0 is not a pipe).
    fn pipe_w() -> libc::c_int {
        let mut fds = [0 as libc::c_int; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return -1;
        }
        unsafe { libc::close(fds[0]) };
        fds[1]
    }

    fn errno_last() -> String {
        format!("errno={}", std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }

    fn thread_count() -> usize {
        std::fs::read_dir("/proc/self/task").map(|tasks| tasks.count()).unwrap_or(0)
    }

    fn report(prefix: &str, probes: &[(String, String)]) {
        for (name, outcome) in probes {
            println!("PROBE|{}{}|{}", prefix, name, outcome);
        }
    }

    // ---- the test cases (run when no mode argument is given) ----

    struct Case {
        name: &'static str,
        run: fn(),
    }

    const CASES: &[Case] = &[
        Case { name: "full_sandbox_denies_exec_network_process_and_signals", run: full_sandbox_denies },
        Case { name: "full_sandbox_writes_only_the_dump_file", run: full_sandbox_dump_only },
        Case { name: "sandbox_restrictions_reach_threads_spawned_after_apply", run: threads_inherit },
        Case { name: "seccomp_layer_alone_denies_exec_network_process_creation", run: seccomp_alone },
        Case { name: "landlock_layer_alone_denies_filesystem", run: landlock_alone },
        Case { name: "sandbox_refuses_when_other_threads_already_run", run: refuses_late_thread },
        Case { name: "sandbox_refuses_when_v8_is_already_initialized", run: refuses_late_v8 },
        Case { name: "sandbox_refuses_when_landlock_is_missing", run: refuses_no_landlock },
        Case { name: "sandbox_refuses_when_seccomp_is_missing", run: refuses_no_seccomp },
        Case { name: "foreign_abi_x32_call_is_killed", run: x32_call_killed },
        Case { name: "foreign_abi_int80_call_is_killed", run: int80_call_killed },
        Case { name: "clone_thread_namespace_shapes_are_denied", run: clone_shapes_denied },
        Case { name: "live_run_threads_all_restricted", run: live_threads_restricted },
        Case { name: "dump_not_truncated_before_the_snapshot", run: dump_not_truncated_early },
        Case { name: "harness_ignores_libtest_flags_and_fails_on_empty_filters", run: harness_args },
    ];

    fn run_tests(filter: Option<&str>) -> i32 {
        let (mut ran, mut failed) = (0, 0);
        // A child spawned by the harness case must not run that case again,
        // or the process chain would never end.
        let harness_child = std::env::var_os("V8VM_SANDBOX_HARNESS_CHILD").is_some();
        for case in CASES {
            if harness_child && case.name.starts_with("harness_") {
                continue;
            }
            if let Some(f) = filter {
                if !case.name.contains(f) {
                    continue;
                }
            }
            ran += 1;
            let ok = std::panic::catch_unwind(case.run).is_ok();
            println!("test {} ... {}", case.name, if ok { "ok" } else { "FAILED" });
            failed += usize::from(!ok);
        }
        // A filter that matches nothing must not look like a pass.
        if ran == 0 {
            eprintln!("no sandbox test matched filter {:?}", filter);
            return 1;
        }
        println!("sandbox test result: {} run; {} failed", ran, failed);
        failed as i32
    }

    /// cargo forwards libtest-style flags to every test target: they must
    /// not be mistaken for filters, and a filter matching nothing must not
    /// look like a pass. The child is marked so it skips this case (it would
    /// otherwise re-run every case, including this one, forever).
    fn harness_args() {
        if std::env::var_os("V8VM_SANDBOX_HARNESS_CHILD").is_some() {
            return;
        }
        let mut flagged = Command::new(std::env::current_exe().expect("test binary path"));
        flagged
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env("V8VM_SANDBOX_HARNESS_CHILD", "1");
        let flagged = run_bounded(&mut flagged);
        assert!(flagged.status.success(), "flags must not skip the tests");
        let out = String::from_utf8_lossy(&flagged.stdout);
        let expected = format!("sandbox test result: {} run;", CASES.len() - 1);
        assert!(out.contains(&expected), "expected the other cases to run: {}", out);
        assert!(!out.contains("test harness_ignores"), "child must skip this case: {}", out);

        let mut empty = Command::new(std::env::current_exe().expect("test binary path"));
        empty.arg("no-such-test-name").env("V8VM_SANDBOX_HARNESS_CHILD", "1");
        let empty = run_bounded(&mut empty);
        assert!(!empty.status.success(), "an empty filter must fail, not pass");
        assert!(String::from_utf8_lossy(&empty.stderr).contains("no sandbox test matched"));
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
        cmd.arg(mode).env("PROBE_PARENT", std::process::id().to_string());
        cmd
    }

    fn run_probe(args: &[String]) -> Vec<String> {
        let mut cmd = self_exec(&args[0]);
        let out = run_bounded(cmd.args(&args[1..]));
        assert!(
            out.status.success(),
            "probe child {:?} failed: {}",
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

    fn errno_is(lines: &[String], name: &str, expected: libc::c_int) {
        assert_eq!(
            result(lines, name),
            format!("errno={}", expected),
            "{} must fail with {}",
            name,
            expected
        );
    }

    fn layer_mode(mode: &str, setup: &str) -> Vec<String> {
        let (dump, escape) = probe_setup(setup);
        let args = [mode.to_string(), path_string(&dump), path_string(&escape)];
        run_probe(&args)
    }

    fn path_string(path: &Path) -> String {
        path.to_str().expect("utf8 path").to_string()
    }

    fn full_sandbox_denies() {
        let lines = layer_mode("probe", "deny");
        assert_eq!(result(&lines, "threads"), "1", "apply starts from one thread");
        for (name, errno) in [
            ("read-etc-passwd", libc::EACCES),
            ("write-existing", libc::EACCES),
            ("write-new", libc::EACCES),
            ("read-dump", libc::EACCES),
            ("socket", libc::EPERM),
            ("exec", libc::EPERM),
            ("fork", libc::EPERM),
            ("clone-newuser", libc::EPERM),
            ("clone3", libc::ENOSYS),
            ("kill-parent", libc::EPERM),
            ("tgkill-parent", libc::EPERM),
            ("ioctl-tiocsti", libc::EPERM),
            ("ioctl-tiocsti-hibit", libc::EPERM),
            ("fcntl-setown-other", libc::EPERM),
            ("sigqueue-parent", libc::EPERM),
            ("rt-tgsigqueue-parent", libc::EPERM),
            ("prlimit-parent", libc::EPERM),
            ("setpriority-other", libc::EPERM),
            ("sched-setaffinity-other", libc::EPERM),
            ("inotify-init", libc::EPERM),
            ("inotify-watch", libc::EPERM),
        ] {
            errno_is(&lines, name, errno);
        }
        for name in ["read-urandom", "kill-self", "thread-spawn", "write-dump", "prlimit-self"] {
            assert_eq!(result(&lines, name), "ok", "{} must keep working", name);
        }
    }

    /// Every CLONE_THREAD + CLONE_NEW* shape, and the vfork/CLONE_VM forms,
    /// must fail with EPERM (seccomp runs before the kernel could reject or
    /// allow the flags itself).
    fn clone_shapes_denied() {
        let lines = layer_mode("probe", "clone-shapes");
        let mut names: Vec<String> = NAMESPACE_PROBE_FLAGS
            .iter()
            .map(|(name, _)| format!("clone-thread+{}", name))
            .collect();
        names.extend(["clone-vfork", "clone-vm-nothread", "clone-newuser", "fork"].map(String::from));
        for name in names {
            errno_is(&lines, &name, libc::EPERM);
        }
    }

    fn full_sandbox_dump_only() {
        let lines = layer_mode("probe", "dump-only");
        assert_eq!(result(&lines, "write-dump"), "ok", "the dump write must pass");
        errno_is(&lines, "read-dump", libc::EACCES);
        errno_is(&lines, "write-existing", libc::EACCES);
        errno_is(&lines, "write-new", libc::EACCES);
        let content = std::fs::read_to_string(scratch("dump-only-dump.json")).expect("dump file");
        assert_eq!(content, "{\"probe\":1}");
    }

    fn threads_inherit() {
        let lines = layer_mode("probe", "thr");
        for (name, errno) in [
            ("thr-read-etc-passwd", libc::EACCES),
            ("thr-write-existing", libc::EACCES),
            ("thr-write-new", libc::EACCES),
            ("thr-read-dump", libc::EACCES),
            ("thr-socket", libc::EPERM),
            ("thr-exec", libc::EPERM),
            ("thr-fork", libc::EPERM),
            ("thr-clone-newuser", libc::EPERM),
            ("thr-clone3", libc::ENOSYS),
            ("thr-kill-parent", libc::EPERM),
        ] {
            errno_is(&lines, name, errno);
        }
        for name in ["thr-write-dump", "thr-read-urandom", "thr-thread-spawn"] {
            assert_eq!(result(&lines, name), "ok", "{} must keep working", name);
        }
    }

    /// seccomp alone, so the exec/network/process denials are provably from
    /// the seccomp rules and not from Landlock's Execute/Write rights.
    fn seccomp_alone() {
        let lines = layer_mode("seccomp-only", "seccomp");
        for (name, errno) in [
            ("exec", libc::EPERM),
            ("socket", libc::EPERM),
            ("fork", libc::EPERM),
            ("clone-newuser", libc::EPERM),
            ("clone3", libc::ENOSYS),
            ("kill-parent", libc::EPERM),
            ("ioctl-tiocsti", libc::EPERM),
            ("fcntl-setown-other", libc::EPERM),
            ("sigqueue-parent", libc::EPERM),
            ("prlimit-parent", libc::EPERM),
            ("setpriority-other", libc::EPERM),
            ("inotify-init", libc::EPERM),
        ] {
            errno_is(&lines, name, errno);
        }
        // Landlock is absent in this mode: these succeed, proving the layer
        // isolation works in the other direction too.
        for name in ["read-etc-passwd", "write-new", "write-dump", "thread-spawn"] {
            assert_eq!(result(&lines, name), "ok", "{} needs only Landlock", name);
        }
    }

    fn landlock_alone() {
        let lines = layer_mode("landlock-only", "landlock");
        for (name, errno) in [
            ("read-etc-passwd", libc::EACCES),
            ("write-existing", libc::EACCES),
            ("write-new", libc::EACCES),
            ("read-dump", libc::EACCES),
            // Execute is a Landlock right too: this is exactly why the exec
            // denial has to be asserted in the seccomp-only run above.
            ("exec", libc::EACCES),
        ] {
            errno_is(&lines, name, errno);
        }
        for name in ["write-dump", "read-urandom", "thread-spawn", "clone3"] {
            assert_eq!(result(&lines, name), "ok", "{} needs no Landlock right", name);
        }
        // The Signal scope is the Landlock side of the signalling fix: even
        // with seccomp absent, neither a queued signal nor a plain kill
        // reaches another process. fcntl F_SETOWN stays allowed here
        // (seccomp owns that rule).
        errno_is(&lines, "sigqueue-parent", libc::EPERM);
        errno_is(&lines, "kill-parent", libc::EPERM);
        errno_is(&lines, "tgkill-parent", libc::EPERM);
        assert_eq!(result(&lines, "fcntl-setown-other"), "ok", "F_SETOWN is seccomp's job");
    }

    fn refuses_late_thread() {
        let lines = run_probe(&["late-thread".to_string()]);
        let r = result(&lines, "late-thread");
        assert!(r.starts_with("refused:"), "apply must refuse: {:?}", lines);
        assert!(r.contains("thread"), "message must name threads: {:?}", lines);
    }

    fn refuses_late_v8() {
        let lines = run_probe(&["late-v8".to_string()]);
        let r = result(&lines, "late-v8");
        assert!(r.starts_with("refused:"), "apply must refuse: {:?}", lines);
        assert!(r.contains("run()/serve()"), "message must name the ordering: {:?}", lines);
    }

    fn refuses_no_landlock() {
        let lines = run_probe(&["no-landlock".to_string()]);
        let r = result(&lines, "no-landlock");
        assert!(r.starts_with("refused:"), "{:?}", lines);
        assert!(r.contains("Landlock"), "refusal must name the layer: {:?}", lines);
    }

    fn refuses_no_seccomp() {
        let lines = run_probe(&["no-seccomp".to_string()]);
        let r = result(&lines, "no-seccomp");
        assert!(r.starts_with("refused:"), "{:?}", lines);
        assert!(r.contains("seccomp"), "refusal must name the layer: {:?}", lines);
    }

    /// A syscall number in the x32 range must be killed by the stacked guard
    /// with SIGSYS, not returned from.
    fn x32_call_killed() {
        let out = spawn_abi_mode("x32");
        assert!(out.0, "armed marker missing: {}", out.1);
        assert_eq!(out.1, libc::SIGSYS.to_string(), "must die with SIGSYS");
    }

    /// An int 0x80 call arrives with the i386 arch value and must be killed
    /// with SIGSYS the same way.
    fn int80_call_killed() {
        let out = spawn_abi_mode("int80");
        assert!(out.0, "armed marker missing: {}", out.1);
        assert_eq!(out.1, libc::SIGSYS.to_string(), "must die with SIGSYS");
    }

    /// (marker seen, terminating signal name) for a foreign-abi probe mode.
    fn spawn_abi_mode(mode: &str) -> (bool, String) {
        let out = run_bounded(&mut self_exec(mode));
        let stdout = String::from_utf8_lossy(&out.stdout);
        let marker = format!("PROBE|{}|armed", mode);
        let seen = stdout.lines().any(|l| l == marker);
        let survived = stdout.contains("survived");
        let signal = out
            .status
            .signal()
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("exit-{:?}", out.status.code()));
        assert!(!survived, "foreign-abi call returned instead of dying: {}", stdout);
        (seen, signal)
    }

    /// The real binary under --sandbox: every thread it creates (V8's worker
    /// pool, watchdog, stdin reader) must carry the seccomp filter and
    /// no_new_privs.
    fn live_threads_restricted() {
        const LIMIT: Duration = Duration::from_secs(60);
        let script = write_guest("alive", "setTimeout(() => console.log('done'), 1500);");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_v8vm"));
        cmd.args(["--sandbox", script.to_str().unwrap()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn v8vm");
        let deadline = Instant::now() + LIMIT;
        let statuses = wait_for_statuses(child.id(), 8, Duration::from_secs(2));
        assert!(statuses.len() > 1, "V8 worker threads must exist");
        for status in &statuses {
            assert!(status.contains("Seccomp:\t2"), "no seccomp filter: {}", first_lines(status));
            assert!(status.contains("NoNewPrivs:\t1"), "no no_new_privs: {}", first_lines(status));
            // Three filters are installed (clone3 ENOSYS, the x32 guard and
            // the deny list). Not every kernel reports the field; this one
            // (6.17) does.
            if let Some(count) = seccomp_filters(status) {
                assert_eq!(count, 3, "filter count: {}", first_lines(status));
            }
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
        assert_eq!(json, serde_json::json!({"/k": "v"}));
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
