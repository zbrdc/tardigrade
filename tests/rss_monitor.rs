//! Bounded process-level tests for Linux RSS sampler admission and outcomes.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod common;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux {
    use std::io::Read;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use crate::common;

    struct Result {
        code: i32,
        stdout: String,
        stderr: String,
    }

    fn script(name: &str, source: &str) -> PathBuf {
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}.js"));
        std::fs::write(&path, source).expect("write RSS monitor test script");
        path
    }

    /// Drain both pipes while polling the child under a deadline. Every
    /// process is waited or killed and waited; these cases never start
    /// descendants, so pipe reader joins also finish when the child exits.
    fn run_bounded(args: &[&str], require_sandbox: bool) -> Result {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tardigrade"));
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if require_sandbox {
            command.env("TARDIGRADE_REQUIRE_SANDBOX", "1");
        }
        let mut child = command.spawn().expect("spawn tardigrade");
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let out_reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).map(|_| bytes)
        });
        let err_reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).map(|_| bytes)
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll tardigrade") {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_reader.join();
                let _ = err_reader.join();
                panic!("tardigrade did not exit within five seconds");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let stdout = out_reader
            .join()
            .expect("stdout reader")
            .expect("read stdout");
        let stderr = err_reader
            .join()
            .expect("stderr reader")
            .expect("read stderr");
        Result {
            code: status.code().expect("tardigrade exited by signal"),
            stdout: String::from_utf8(stdout).expect("stdout UTF-8"),
            stderr: String::from_utf8(stderr).expect("stderr UTF-8"),
        }
    }

    fn sandbox_available(test: &str) -> bool {
        if common::kernel_supports_sandbox() {
            return true;
        }
        assert!(
            !common::sandbox_required(),
            "TARDIGRADE_REQUIRE_SANDBOX=1 but the kernel reports Landlock ABI {}, need {}",
            common::landlock_abi(),
            common::REQUIRED_LANDLOCK_ABI
        );
        common::skipped(test);
        false
    }

    #[test]
    fn sandbox_with_requested_rss_cap_refuses_before_guest_entry() {
        if !sandbox_available("sandbox_with_requested_rss_cap_refuses_before_guest_entry") {
            return;
        }
        let entry = script(
            "rss-sandbox-admission",
            "console.log('GUEST_EXECUTED'); setTimeout(() => {}, 300);",
        );
        let entry = entry.to_str().unwrap();
        let run = run_bounded(&["--sandbox", "--rss-cap=1", entry], true);
        assert!(
            !run.stdout.contains("GUEST_EXECUTED"),
            "guest entry ran before sampler admission: {:?}",
            run.stdout
        );
        assert_eq!(run.code, 70, "stderr: {}", run.stderr);
        assert_eq!(
            run.stderr, "tardigrade: cannot start run watchdog\n",
            "startup refusal diagnostic must stay bounded"
        );
    }

    #[test]
    fn serve_bootstrap_with_requested_rss_cap_refuses_before_entry() {
        if !sandbox_available("serve_bootstrap_with_requested_rss_cap_refuses_before_entry") {
            return;
        }
        let entry = script(
            "rss-serve-admission",
            "console.log('GUEST_EXECUTED'); setTimeout(() => {}, 300);",
        );
        let entry = entry.to_str().unwrap();
        let run = run_bounded(&["--serve", "--sandbox", "--rss-cap=1", entry], true);
        assert!(
            !run.stdout.contains("GUEST_EXECUTED") && !run.stderr.contains("GUEST_EXECUTED"),
            "serve entry ran before sampler admission: stdout={:?} stderr={:?}",
            run.stdout,
            run.stderr
        );
        assert_eq!(run.code, 70, "stderr: {}", run.stderr);
        assert_eq!(
            run.stderr, "tardigrade: cannot start run watchdog\n",
            "serve bootstrap refusal diagnostic must stay bounded"
        );
    }

    #[test]
    fn sandbox_with_disabled_rss_cap_still_runs() {
        if !sandbox_available("sandbox_with_disabled_rss_cap_still_runs") {
            return;
        }
        let entry = script("rss-sandbox-disabled", "console.log('GUEST_EXECUTED');");
        let entry = entry.to_str().unwrap();
        let run = run_bounded(&["--sandbox", "--rss-cap=0", entry], true);
        assert_eq!(run.code, 0, "stderr: {}", run.stderr);
        assert_eq!(run.stdout, "GUEST_EXECUTED\n");
        assert_eq!(run.stderr, "");
    }

    #[test]
    fn available_unsandboxed_rss_cap_keeps_sampled_oom_exit() {
        let entry = script(
            "rss-unsandboxed-available",
            "setTimeout(() => console.log('TIMER_FIRED'), 300);",
        );
        let entry = entry.to_str().unwrap();
        let run = run_bounded(&["--rss-cap=1", entry], false);
        assert_eq!(run.code, 137, "stderr: {}", run.stderr);
        assert_eq!(run.stdout, "", "RSS cap should stop before the timer");
        assert!(
            run.stderr.starts_with("tardigrade: heap limit exceeded ("),
            "stderr: {}",
            run.stderr
        );
        assert!(!run.stdout.contains("TIMER_FIRED"));
    }
}
