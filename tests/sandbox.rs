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
    use std::process::{Command, Output, Stdio};
    use std::time::{Duration, Instant};

    pub fn main() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if !dispatch_mode(&args) {
            let filter = args.first().map(String::as_str).unwrap_or("");
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
        vec![
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
        ]
    }

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

    fn probe_clone(flags: libc::c_ulong) -> String {
        syscall_outcome(unsafe { libc::syscall(libc::SYS_clone, flags, 0, 0, 0, 0) })
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
            unsafe { libc::_exit(0) }; // child of an unexpected success
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
        Case { name: "live_run_threads_all_restricted", run: live_threads_restricted },
        Case { name: "dump_not_truncated_before_the_snapshot", run: dump_not_truncated_early },
    ];

    fn run_tests(filter: &str) -> i32 {
        let (mut ran, mut failed) = (0, 0);
        for case in CASES {
            if !filter.is_empty() && !case.name.contains(filter) {
                continue;
            }
            ran += 1;
            let ok = std::panic::catch_unwind(case.run).is_ok();
            println!("test {} ... {}", case.name, if ok { "ok" } else { "FAILED" });
            failed += usize::from(!ok);
        }
        println!("sandbox test result: {} run; {} failed", ran, failed);
        failed as i32
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

    fn run_probe(args: &[String]) -> Vec<String> {
        let out = Command::new(std::env::current_exe().expect("test binary path"))
            .args(args)
            .env("PROBE_PARENT", std::process::id().to_string())
            .output()
            .expect("re-exec probe child");
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
        ] {
            errno_is(&lines, name, errno);
        }
        for name in ["read-urandom", "kill-self", "thread-spawn", "write-dump"] {
            assert_eq!(result(&lines, name), "ok", "{} must keep working", name);
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
        assert!(result(&lines, "no-landlock").starts_with("refused:"), "{:?}", lines);
    }

    fn refuses_no_seccomp() {
        let lines = run_probe(&["no-seccomp".to_string()]);
        assert!(result(&lines, "no-seccomp").starts_with("refused:"), "{:?}", lines);
    }

    /// The real binary under --sandbox: every thread it creates (V8's worker
    /// pool, watchdog, stdin reader) must carry the seccomp filter and
    /// no_new_privs.
    fn live_threads_restricted() {
        let script = write_guest("alive", "setTimeout(() => console.log('done'), 1500);");
        let mut child = spawn_v8vm(&["--sandbox", script.to_str().unwrap()]);
        let statuses = wait_for_statuses(child.id(), 8, Duration::from_secs(2));
        assert!(statuses.len() > 1, "V8 worker threads must exist");
        for status in &statuses {
            assert!(status.contains("Seccomp:\t2"), "no seccomp filter: {}", first_lines(status));
            assert!(status.contains("NoNewPrivs:\t1"), "no no_new_privs: {}", first_lines(status));
        }
        drop(child.stdin.take()); // release stdin so the guest can finish
        let out = child.wait_with_output().expect("wait v8vm");
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

    fn spawn_v8vm(args: &[&str]) -> std::process::Child {
        Command::new(env!("CARGO_BIN_EXE_v8vm"))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn v8vm")
    }

    fn v8vm_output(args: &[&str]) -> Output {
        spawn_v8vm(args).wait_with_output().expect("wait v8vm")
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
}

fn main() {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    run::main();
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    println!("sandbox tests: unsupported platform (need Linux x86_64), 0 run");
}
