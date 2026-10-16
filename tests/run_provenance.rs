//! End-to-end tests for the opt-in CLI preparation manifest.
//!
//! Causal matrix (each case is also used for its named temporary CLI mutation):
//! - opt_in_preserves_cli_behavior_and_snapshot_v2: disabled publication.
//! - manifest_flag_boundaries_and_double_dash: disabled flag recognition;
//!   empty and "-" parse validation; positional manifest-looking guest arg.
//! - serve_record_is_initial_and_uses_effective_serve_defaults: wrong scope
//!   and wrong effective serve default metadata.
//! - sandbox_manifest_and_distinct_dump_succeed: publication after sandbox.
//! - output_failure_and_dump_aliases_precede_guest_execution: ignored output
//!   error and omitted actual dump path.
//! - stdin_network_manifest_redacts_cli_secrets: raw CLI-secret disclosure.
//! - manifest_and_guest_reuse_the_same_preload_bytes: clearing the captured
//!   preload only after publishing the manifest.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const CHILD_DEADLINE: Duration = Duration::from_secs(25);
const IO_DEADLINE: Duration = Duration::from_secs(8);
const MAX_CAPTURE: usize = 1024 * 1024;
const MAX_LINE: usize = 64 * 1024;

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        let path = root.join(format!(
            "run-provenance-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create fresh NVMe test fixture");
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Capture {
    bytes: Vec<u8>,
    truncated: bool,
}

fn drain_bounded<R: Read>(mut reader: R) -> io::Result<Capture> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 8192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let room = MAX_CAPTURE.saturating_sub(bytes.len());
        let keep = room.min(count);
        bytes.extend_from_slice(&buffer[..keep]);
        truncated |= keep != count;
    }
    Ok(Capture { bytes, truncated })
}

struct ChildGuard(Child);

impl ChildGuard {
    fn wait(&mut self, deadline: Duration) -> ExitStatus {
        let until = Instant::now() + deadline;
        loop {
            match self.0.try_wait().expect("poll CLI child") {
                Some(status) => return status,
                None if Instant::now() < until => thread::sleep(Duration::from_millis(10)),
                None => {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                    panic!("CLI child exceeded its {deadline:?} deadline");
                }
            }
        }
    }

    fn terminate_and_reap(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.terminate_and_reap();
    }
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run_cli(cwd: &Path, args: &[String], stdin: &[u8]) -> Run {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tardigrade"));
    command
        .args(args)
        .current_dir(cwd)
        .env_remove("TARDIGRADE_DEBUG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ChildGuard(command.spawn().expect("spawn actual tardigrade CLI"));
    let stdout = child.0.stdout.take().expect("piped stdout");
    let stderr = child.0.stderr.take().expect("piped stderr");
    let (stdout_tx, stdout_rx) = mpsc::channel();
    let (stderr_tx, stderr_rx) = mpsc::channel();
    let stdout_reader = thread::spawn(move || {
        let _ = stdout_tx.send(drain_bounded(stdout));
    });
    let stderr_reader = thread::spawn(move || {
        let _ = stderr_tx.send(drain_bounded(stderr));
    });

    let feeder = if stdin.is_empty() {
        drop(child.0.stdin.take());
        None
    } else {
        let mut pipe = child.0.stdin.take().expect("piped stdin");
        let input = stdin.to_vec();
        Some(thread::spawn(move || pipe.write_all(&input)))
    };
    let status = child.wait(CHILD_DEADLINE);
    if let Some(feeder) = feeder {
        feeder
            .join()
            .expect("stdin feeder panicked")
            .expect("feed CLI stdin");
    }
    let stdout = stdout_rx
        .recv_timeout(IO_DEADLINE)
        .expect("stdout pipe drain deadline")
        .expect("drain stdout");
    let stderr = stderr_rx
        .recv_timeout(IO_DEADLINE)
        .expect("stderr pipe drain deadline")
        .expect("drain stderr");
    stdout_reader.join().expect("stdout reader panicked");
    stderr_reader.join().expect("stderr reader panicked");
    assert!(
        !stdout.truncated,
        "stdout exceeded the bounded test capture"
    );
    assert!(
        !stderr.truncated,
        "stderr exceeded the bounded test capture"
    );
    Run {
        code: status.code().expect("CLI exit code, not signal"),
        stdout: String::from_utf8(stdout.bytes).expect("CLI stdout UTF-8"),
        stderr: String::from_utf8(stderr.bytes).expect("CLI stderr UTF-8"),
    }
}

struct LiveCliChild {
    child: ChildGuard,
    stdin: Option<std::process::ChildStdin>,
    stdout_rx: mpsc::Receiver<io::Result<Capture>>,
    stderr_rx: mpsc::Receiver<io::Result<Capture>>,
    stdout_reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
}

impl LiveCliChild {
    fn start(cwd: &Path, args: &[String]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tardigrade"));
        command
            .args(args)
            .current_dir(cwd)
            .env_remove("TARDIGRADE_DEBUG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = ChildGuard(command.spawn().expect("spawn live CLI child"));
        let stdin = child.0.stdin.take();
        let stdout = child.0.stdout.take().expect("piped live stdout");
        let stderr = child.0.stderr.take().expect("piped live stderr");
        let (stdout_tx, stdout_rx) = mpsc::channel();
        let (stderr_tx, stderr_rx) = mpsc::channel();
        let stdout_reader = thread::spawn(move || {
            let _ = stdout_tx.send(drain_bounded(stdout));
        });
        let stderr_reader = thread::spawn(move || {
            let _ = stderr_tx.send(drain_bounded(stderr));
        });
        Self {
            child,
            stdin,
            stdout_rx,
            stderr_rx,
            stdout_reader: Some(stdout_reader),
            stderr_reader: Some(stderr_reader),
        }
    }

    fn pid(&self) -> u32 {
        self.child.0.id()
    }

    fn finish(&mut self) -> Run {
        self.stdin.take();
        let status = self.child.wait(CHILD_DEADLINE);
        let stdout = self
            .stdout_rx
            .recv_timeout(IO_DEADLINE)
            .expect("live stdout pipe drain deadline")
            .expect("drain live stdout");
        let stderr = self
            .stderr_rx
            .recv_timeout(IO_DEADLINE)
            .expect("live stderr pipe drain deadline")
            .expect("drain live stderr");
        if let Some(reader) = self.stdout_reader.take() {
            reader.join().expect("live stdout reader");
        }
        if let Some(reader) = self.stderr_reader.take() {
            reader.join().expect("live stderr reader");
        }
        assert!(!stdout.truncated, "live stdout capture exceeded its bound");
        assert!(!stderr.truncated, "live stderr capture exceeded its bound");
        Run {
            code: status.code().expect("live CLI exit code"),
            stdout: String::from_utf8(stdout.bytes).expect("live stdout UTF-8"),
            stderr: String::from_utf8(stderr.bytes).expect("live stderr UTF-8"),
        }
    }
}

impl Drop for LiveCliChild {
    fn drop(&mut self) {
        self.stdin.take();
        self.child.terminate_and_reap();
        if let Some(reader) = self.stdout_reader.take() {
            let _ = reader.join();
        }
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn live_thread_statuses(pid: u32) -> Vec<String> {
    let mut statuses = Vec::new();
    let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) else {
        return statuses;
    };
    for task in tasks.flatten() {
        if let Ok(status) = fs::read_to_string(task.path().join("status")) {
            statuses.push(status);
        }
    }
    statuses
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn assert_sandbox_threads(child: &mut LiveCliChild) {
    let deadline = Instant::now() + Duration::from_secs(15);
    let statuses = loop {
        assert!(
            child
                .child
                .0
                .try_wait()
                .expect("poll sandboxed child")
                .is_none(),
            "sandboxed guest exited before kernel assertions"
        );
        let statuses = live_thread_statuses(child.pid());
        if statuses.len() >= 5 {
            break statuses;
        }
        assert!(
            Instant::now() < deadline,
            "expected at least five sandboxed runtime threads, saw {}",
            statuses.len()
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert!(statuses.len() > 1, "V8 worker threads must exist");
    for status in statuses {
        assert!(
            status.contains("Seccomp:\t2"),
            "thread has no seccomp filter: {status}"
        );
        assert!(
            status.contains("NoNewPrivs:\t1"),
            "thread has no no_new_privs: {status}"
        );
        let filters = status
            .lines()
            .find_map(|line| line.strip_prefix("Seccomp_filters:"))
            .and_then(|value| value.trim().parse::<u32>().ok())
            .expect("Seccomp_filters field in each /proc task status");
        assert_eq!(
            filters, 3,
            "expected clone3, x32 guard, and deny-list filters: {status}"
        );
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

fn write_entry(dir: &Path, name: &str, source: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, source).expect("write test guest entry");
    path
}

fn manifest(path: &Path) -> (Vec<u8>, serde_json::Value) {
    let bytes = fs::read(path).expect("read published manifest");
    assert!(bytes.ends_with(b"\n"), "manifest has one trailing newline");
    assert!(
        !bytes[..bytes.len() - 1].contains(&b'\n'),
        "one JSON record only"
    );
    let value = serde_json::from_slice(&bytes[..bytes.len() - 1]).expect("manifest JSON");
    (bytes, value)
}

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn assert_normal_cli_result(run: &Run, code: i32, stdout: &str) {
    assert_eq!(run.code, code, "stderr: {}", run.stderr);
    assert_eq!(run.stdout, stdout, "stderr: {}", run.stderr);
}

#[test]
fn opt_in_preserves_cli_behavior_and_snapshot_v2() {
    let scratch = Scratch::new("same-run");
    let entry = write_entry(
        &scratch.0,
        "ordinary.js",
        "vm.fs.write('/answer', 'forty-two'); console.log('ordinary-output');",
    );
    let ordinary_dump = scratch.0.join("ordinary-snapshot.json");
    let opted_dump = scratch.0.join("opted-snapshot.json");
    let requested_but_absent = scratch.0.join("not-requested.json");
    let manifest_path = scratch.0.join("prepared.json");
    let common = [
        "--timeout=4321".to_owned(),
        "--memory=48".to_owned(),
        "--rss-cap=24".to_owned(),
        "--max-output=2".to_owned(),
        "--fs-quota=3".to_owned(),
        "--seed=77".to_owned(),
        "--clock=virtual".to_owned(),
        "--epoch=1700000000123".to_owned(),
    ];
    let mut ordinary_args = common.to_vec();
    ordinary_args.push(format!("--dump={}", ordinary_dump.display()));
    ordinary_args.push(entry.display().to_string());
    let ordinary = run_cli(&scratch.0, &ordinary_args, b"");
    let mut ordinary_artifacts = fs::read_dir(&scratch.0)
        .expect("list ordinary-mode fixture")
        .map(|entry| {
            entry
                .expect("ordinary-mode fixture entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    ordinary_artifacts.sort();
    assert_eq!(
        ordinary_artifacts,
        ["ordinary-snapshot.json", "ordinary.js"],
        "without the option, no manifest artifact or preparation I/O output is created"
    );

    let mut opted_args = common.to_vec();
    opted_args.push(format!("--dump={}", opted_dump.display()));
    opted_args.push(format!("--run-manifest={}", manifest_path.display()));
    opted_args.push(entry.display().to_string());
    let opted = run_cli(&scratch.0, &opted_args, b"");

    assert_eq!(ordinary.code, opted.code);
    assert_eq!(ordinary.stdout, opted.stdout);
    assert_eq!(ordinary.stderr, opted.stderr);
    assert_normal_cli_result(&opted, 0, "ordinary-output\n");
    assert!(
        !requested_but_absent.exists(),
        "ordinary mode publishes nothing"
    );
    let ordinary_snapshot = fs::read(&ordinary_dump).expect("ordinary snapshot");
    let opted_snapshot = fs::read(&opted_dump).expect("opted snapshot");
    assert_eq!(ordinary_snapshot, opted_snapshot);
    let snapshot: serde_json::Value =
        serde_json::from_slice(&opted_snapshot).expect("snapshot JSON");
    assert_eq!(snapshot["version"], 2, "existing RAM-fs snapshot v2");
    assert_eq!(
        snapshot["files"]["/answer"]["text"], "forty-two",
        "guest state remains in the ordinary snapshot"
    );

    let (bytes, record) = manifest(&manifest_path);
    assert_eq!(record["phase"], "prepared");
    assert_eq!(record["scope"], "cli-run");
    assert_eq!(record["inputs"]["entry"]["mode"], "script");
    assert_eq!(
        record["inputs"]["entry"]["sha256"],
        sha256_hex(b"vm.fs.write('/answer', 'forty-two'); console.log('ordinary-output');")
    );
    assert_eq!(
        record["inputs"]["preload"],
        serde_json::json!({"present": false, "sha256": null})
    );
    assert_eq!(record["determinism"]["seed"], 77);
    assert_eq!(record["determinism"]["clock"]["kind"], "virtual");
    assert_eq!(
        record["determinism"]["clock"]["epoch_ms"],
        1_700_000_000_123i64
    );
    assert_eq!(record["effective_limits"]["run_timeout_ms"], 4321);
    assert_eq!(record["effective_limits"]["command_timeout_ms"], 0);
    assert_eq!(record["effective_limits"]["heap_mb"], 48);
    assert_eq!(record["effective_limits"]["rss_cap_mb"], 24);
    assert_eq!(
        record["effective_limits"]["max_output_bytes"],
        2 * 1024 * 1024
    );
    assert_eq!(
        record["effective_limits"]["fs_quota_bytes"],
        3 * 1024 * 1024
    );
    assert_eq!(
        record["effective_limits"]["network"],
        serde_json::Value::Null
    );
    assert_eq!(
        record["capability_policy"],
        serde_json::json!({"granted": false, "identity": null, "identity_status": "not_applicable"})
    );
    assert_eq!(bytes.last(), Some(&b'\n'));
}

#[test]
fn manifest_flag_boundaries_and_double_dash() {
    let scratch = Scratch::new("parse-boundaries");
    let entry = write_entry(
        &scratch.0,
        "parse.js",
        "console.log('GUEST_RAN', JSON.stringify(vm.args));",
    );
    let entry = entry.display().to_string();

    let usage = run_cli(&scratch.0, &[], b"");
    assert_eq!(usage.code, 2);
    assert!(usage.stderr.contains("--run-manifest=<file>"));

    for bad in ["--run-manifest=", "--run-manifest=-"] {
        let run = run_cli(&scratch.0, &strings(&[bad, &entry]), b"");
        assert_eq!(run.stdout, "", "parse failures cannot execute guest");
        if bad == "--run-manifest=-" {
            assert!(
                !scratch.0.join("-").exists(),
                "literal - must not create a file in the child cwd"
            );
        }
        assert_eq!(run.code, 2, "{bad}: {}", run.stderr);
        assert!(run.stderr.contains("--run-manifest"), "{:?}", run.stderr);
        assert!(!run.stderr.contains("cannot prepare run manifest"));
    }

    let positional = "manifest-looking.json";
    let run = run_cli(
        &scratch.0,
        &strings(&["--", &entry, "--run-manifest=manifest-looking.json"]),
        b"",
    );
    assert!(
        !scratch.0.join(positional).exists(),
        "post-`--` option-looking value remains a guest argument"
    );
    assert_normal_cli_result(
        &run,
        0,
        "GUEST_RAN [\"--run-manifest=manifest-looking.json\"]\n",
    );
}

struct ServeChild {
    child: ChildGuard,
    stdin: Option<std::process::ChildStdin>,
    lines: mpsc::Receiver<io::Result<String>>,
    stdout_reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
    stderr_rx: mpsc::Receiver<io::Result<Capture>>,
}

fn bounded_line_reader<R: Read + Send + 'static>(
    reader: R,
    tx: mpsc::SyncSender<io::Result<String>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = io::BufReader::new(reader);
        loop {
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                match reader.read(&mut byte) {
                    Ok(0) => {
                        if !line.is_empty() {
                            let _ = tx.try_send(Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "unterminated stdout line",
                            )));
                        }
                        return;
                    }
                    Ok(_) => {
                        if line.len() < MAX_LINE {
                            line.push(byte[0]);
                        }
                        if byte[0] == b'\n' {
                            let result = String::from_utf8(line)
                                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
                            let _ = tx.try_send(result);
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = tx.try_send(Err(error));
                        return;
                    }
                }
            }
        }
    })
}

impl ServeChild {
    fn start(cwd: &Path, args: &[String]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tardigrade"));
        command
            .args(args)
            .current_dir(cwd)
            .env_remove("TARDIGRADE_DEBUG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = ChildGuard(command.spawn().expect("spawn serve CLI child"));
        let stdin = child.0.stdin.take();
        let stdout = child.0.stdout.take().expect("serve stdout");
        let stderr = child.0.stderr.take().expect("serve stderr");
        let (line_tx, lines) = mpsc::sync_channel(16);
        let (stderr_tx, stderr_rx) = mpsc::channel();
        let stdout_reader = bounded_line_reader(stdout, line_tx);
        let stderr_reader = thread::spawn(move || {
            let _ = stderr_tx.send(drain_bounded(stderr));
        });
        Self {
            child,
            stdin,
            lines,
            stdout_reader: Some(stdout_reader),
            stderr_reader: Some(stderr_reader),
            stderr_rx,
        }
    }

    fn command(&mut self, expression: &str) -> String {
        let input = self.stdin.as_mut().expect("serve stdin remains open");
        input
            .write_all(expression.as_bytes())
            .and_then(|_| input.write_all(b"\n"))
            .and_then(|_| input.flush())
            .expect("send bounded serve command");
        match self.lines.recv_timeout(CHILD_DEADLINE) {
            Ok(Ok(line)) => line,
            Ok(Err(error)) => panic!("read serve stdout: {error}"),
            Err(error) => panic!("serve reply deadline: {error}"),
        }
    }

    fn close_and_wait(&mut self) {
        self.stdin.take();
        let status = self.child.wait(CHILD_DEADLINE);
        assert_eq!(status.code(), Some(0), "serve child should exit cleanly");
        match self.stderr_rx.recv_timeout(IO_DEADLINE) {
            Ok(Ok(capture)) => {
                assert!(!capture.truncated, "serve stderr capture was bounded");
                let stderr = String::from_utf8(capture.bytes).expect("serve stderr UTF-8");
                assert!(stderr.is_empty(), "unexpected serve stderr: {stderr}");
            }
            Ok(Err(error)) => panic!("drain serve stderr: {error}"),
            Err(error) => panic!("serve stderr drain deadline: {error}"),
        }
        if let Some(reader) = self.stdout_reader.take() {
            reader.join().expect("serve stdout reader");
        }
        if let Some(reader) = self.stderr_reader.take() {
            reader.join().expect("serve stderr reader");
        }
        match self.lines.try_recv() {
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => {}
            Ok(Ok(line)) => panic!("unexpected extra serve output: {line:?}"),
            Ok(Err(error)) => panic!("serve stdout error: {error}"),
        }
    }
}

impl Drop for ServeChild {
    fn drop(&mut self) {
        self.stdin.take();
        self.child.terminate_and_reap();
        if let Some(reader) = self.stdout_reader.take() {
            let _ = reader.join();
        }
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
    }
}

fn wait_for_manifest(path: &Path) -> (Vec<u8>, serde_json::Value) {
    let deadline = Instant::now() + CHILD_DEADLINE;
    loop {
        if let Ok(bytes) = fs::read(path) {
            assert!(bytes.len() <= 256 * 1024, "manifest exceeds test bound");
            if bytes.ends_with(b"\n") && !bytes[..bytes.len() - 1].contains(&b'\n') {
                if let Ok(value) = serde_json::from_slice(&bytes[..bytes.len() - 1]) {
                    return (bytes, value);
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "manifest was not completely published before the deadline"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn wait_for_manifest_or_exit(
    path: &Path,
    child: &mut LiveCliChild,
) -> (Vec<u8>, serde_json::Value) {
    let deadline = Instant::now() + CHILD_DEADLINE;
    loop {
        if let Ok(bytes) = fs::read(path) {
            assert!(bytes.len() <= 256 * 1024, "manifest exceeds test bound");
            if bytes.ends_with(b"\n") && !bytes[..bytes.len() - 1].contains(&b'\n') {
                if let Ok(value) = serde_json::from_slice(&bytes[..bytes.len() - 1]) {
                    return (bytes, value);
                }
            }
        }
        assert!(
            child
                .child
                .0
                .try_wait()
                .expect("poll CLI while awaiting manifest")
                .is_none(),
            "CLI exited before the complete manifest was published"
        );
        assert!(
            Instant::now() < deadline,
            "manifest was not completely published before the deadline"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn serve_record_is_initial_and_uses_effective_serve_defaults() {
    let scratch = Scratch::new("serve");
    let entry = write_entry(&scratch.0, "serve.js", "globalThis.count = 0;");
    let manifest_path = scratch.0.join("serve.json");
    let args = strings(&[
        "--serve",
        &format!("--run-manifest={}", manifest_path.display()),
        entry.to_str().unwrap(),
    ]);
    let mut child = ServeChild::start(&scratch.0, &args);

    let (initial_bytes, record) = wait_for_manifest(&manifest_path);
    assert_eq!(record["scope"], "cli-serve-session");
    assert_eq!(record["effective_limits"]["run_timeout_ms"], 0);
    assert_eq!(record["effective_limits"]["command_timeout_ms"], 30_000);
    assert_eq!(child.command("globalThis.count += 1"), "1\n");
    assert_eq!(child.command("globalThis.count += 1"), "2\n");
    child.close_and_wait();
    let (final_bytes, _) = manifest(&manifest_path);
    assert_eq!(
        final_bytes, initial_bytes,
        "serve commands and resulting state are not appended to the bootstrap record"
    );
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn sandbox_manifest_and_distinct_dump_succeed() {
    let scratch = Scratch::new("sandbox");
    let entry = write_entry(
        &scratch.0,
        "sandbox.js",
        "vm.fs.write('/inside', 'sandboxed'); vm.stdin.read().then(() => console.log('GUEST_RAN'));",
    );
    let manifest_path = scratch.0.join("prepared.json");
    let dump_path = scratch.0.join("snapshot-v2.json");
    let check = run_cli(&scratch.0, &strings(&["--sandbox-check"]), b"");
    assert_eq!(
        check.code, 0,
        "mandatory sandbox kernel check must run without a skip: {}",
        check.stderr
    );
    assert_eq!(check.stdout, "supported\n");

    let args = strings(&[
        "--sandbox",
        &format!("--run-manifest={}", manifest_path.display()),
        &format!("--dump={}", dump_path.display()),
        entry.to_str().unwrap(),
    ]);
    let mut child = LiveCliChild::start(&scratch.0, &args);
    let (_, record) = wait_for_manifest_or_exit(&manifest_path, &mut child);
    assert_eq!(record["isolation"]["sandbox_requested"], true);
    assert_sandbox_threads(&mut child);
    let child_input = child.stdin.as_mut().expect("sandbox child's stdin");
    child_input.write_all(b"release guest\n").unwrap();
    child_input.flush().unwrap();
    let run = child.finish();
    assert_normal_cli_result(&run, 0, "GUEST_RAN\n");
    assert_ne!(manifest_path, dump_path);
    let snapshot: serde_json::Value =
        serde_json::from_slice(&fs::read(&dump_path).expect("sandbox dump")).unwrap();
    assert_eq!(snapshot["version"], 2);
    assert_eq!(snapshot["files"]["/inside"]["text"], "sandboxed");
}

#[test]
fn output_failure_and_dump_aliases_precede_guest_execution() {
    let scratch = Scratch::new("output-failures");
    let entry = write_entry(
        &scratch.0,
        "must-not-run.js",
        "console.log('GUEST_RAN'); vm.fs.write('/side-effect', 'must-not-run');",
    );
    let existing_manifest = scratch.0.join("existing-manifest.json");
    let unrelated_dump = scratch.0.join("unrelated-dump.json");
    fs::write(&existing_manifest, b"manifest sentinel").unwrap();
    fs::write(&unrelated_dump, b"dump sentinel").unwrap();
    let failed = run_cli(
        &scratch.0,
        &strings(&[
            &format!("--run-manifest={}", existing_manifest.display()),
            &format!("--dump={}", unrelated_dump.display()),
            entry.to_str().unwrap(),
        ]),
        b"",
    );
    assert_eq!(failed.stdout, "", "guest ran despite output refusal");
    assert_eq!(failed.code, 2, "stderr: {}", failed.stderr);
    assert_eq!(
        failed.stderr, "tardigrade: cannot prepare run manifest\n",
        "output error diagnostic is bounded and static"
    );
    assert_eq!(fs::read(&existing_manifest).unwrap(), b"manifest sentinel");
    assert_eq!(fs::read(&unrelated_dump).unwrap(), b"dump sentinel");

    let exact = scratch.0.join("same.json");
    let exact_failure = run_cli(
        &scratch.0,
        &strings(&[
            &format!("--run-manifest={}", exact.display()),
            &format!("--dump={}", exact.display()),
            entry.to_str().unwrap(),
        ]),
        b"",
    );
    assert_eq!(exact_failure.stdout, "");
    assert_eq!(exact_failure.code, 2, "stderr: {}", exact_failure.stderr);
    assert_eq!(
        exact_failure.stderr,
        "tardigrade: cannot prepare run manifest\n"
    );
    assert!(
        fs::symlink_metadata(&exact).is_err(),
        "owned newly created alias must be cleaned"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let symlink_dir = scratch.0.join("dangling");
        fs::create_dir(&symlink_dir).unwrap();
        let manifest = symlink_dir.join("target.json");
        let dump_alias = symlink_dir.join("dump-link.json");
        symlink(&manifest, &dump_alias).unwrap();
        let dangling_failure = run_cli(
            &symlink_dir,
            &strings(&[
                "--run-manifest=target.json",
                "--dump=dump-link.json",
                entry.to_str().unwrap(),
            ]),
            b"",
        );
        assert_eq!(dangling_failure.stdout, "");
        assert_eq!(dangling_failure.code, 2, "{}", dangling_failure.stderr);
        assert_eq!(
            dangling_failure.stderr,
            "tardigrade: cannot prepare run manifest\n"
        );
        assert!(fs::symlink_metadata(&dump_alias)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(
            fs::symlink_metadata(&manifest).is_err(),
            "only the newly created manifest artifact is removed"
        );
    }
}

#[test]
fn stdin_network_manifest_redacts_cli_secrets() {
    use std::net::TcpListener;

    let scratch = Scratch::new("redaction");
    let manifest_path = scratch.0.join("prepared.json");
    let source = "// SOURCE_SECRET_75ac\nconsole.log('stdin-source-ran');\n";
    let argument_secret = "ARGUMENT_SECRET_31bd";
    let capability_secret = "capability_secret_9e52";
    let url_secret = "URL_SECRET_20c1";
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind no-request loopback probe");
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let grant =
        format!("--allow-call={capability_secret}=http://127.0.0.1:{port}/{url_secret},POST");
    let run = run_cli(
        &scratch.0,
        &strings(&[
            &format!("--run-manifest={}", manifest_path.display()),
            "--net-max-response=321",
            "--net-total-bytes=654",
            "--net-timeout=987",
            "--net-allow-private",
            "--seed=17",
            "--clock=virtual",
            "--epoch=123456789",
            &grant,
            "-",
            "--",
            argument_secret,
        ]),
        source.as_bytes(),
    );
    assert_normal_cli_result(&run, 0, "stdin-source-ran\n");
    assert!(
        matches!(
            listener.accept(),
            Err(ref error) if error.kind() == io::ErrorKind::WouldBlock
        ),
        "redaction test must not issue any HTTP request"
    );

    let (bytes, record) = manifest(&manifest_path);
    let text = std::str::from_utf8(&bytes).unwrap();
    assert_eq!(
        record["inputs"]["entry"]["sha256"],
        sha256_hex(source.as_bytes()),
        "digest is of the exact UTF-8 source consumed from stdin"
    );
    assert_eq!(record["inputs"]["entry"]["mode"], "script");
    assert_eq!(record["determinism"]["seed"], 17);
    assert_eq!(record["determinism"]["clock"]["epoch_ms"], 123456789);
    assert_eq!(
        record["effective_limits"]["network"],
        serde_json::json!({
            "max_response_bytes": 321,
            "total_bytes": 654,
            "timeout_ms": 987,
            "allow_private": true
        })
    );
    assert_eq!(
        record["capability_policy"],
        serde_json::json!({"granted": true, "identity": null, "identity_status": "redacted"})
    );
    assert!(record["capability_policy"]["identity"].is_null());
    for secret in [
        "SOURCE_SECRET_75ac",
        argument_secret,
        capability_secret,
        url_secret,
        &format!("http://127.0.0.1:{port}/{url_secret}"),
        &scratch.0.display().to_string(),
        "wire_fd",
    ] {
        assert!(!text.contains(secret), "manifest disclosed {secret:?}");
    }
}

#[cfg(unix)]
fn create_fifo(path: &Path) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes()).expect("FIFO path has no NUL");
    let result = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
    assert_eq!(result, 0, "create FIFO: {}", io::Error::last_os_error());
}

#[cfg(unix)]
fn open_fifo_writer_until(path: &Path, deadline: Instant, cancel: &AtomicBool) -> Option<File> {
    use std::os::unix::fs::OpenOptionsExt;

    loop {
        if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
            return None;
        }
        match OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => return Some(file),
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::ENXIO | libc::EINTR | libc::EAGAIN)
                ) =>
            {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("open FIFO writer: {error}"),
        }
    }
}

#[cfg(unix)]
fn write_fifo_snapshot(file: &mut File, snapshot: &[u8]) {
    let until = Instant::now() + IO_DEADLINE;
    let mut offset = 0;
    while offset < snapshot.len() {
        assert!(Instant::now() < until, "FIFO write deadline");
        match file.write(&snapshot[offset..]) {
            Ok(0) => panic!("FIFO write returned zero"),
            Ok(count) => offset += count,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("write FIFO snapshot: {error}"),
        }
    }
}

#[cfg(unix)]
struct FifoFeeder {
    cancel: Arc<AtomicBool>,
    writes: Arc<AtomicUsize>,
    worker: Option<JoinHandle<()>>,
}

#[cfg(unix)]
impl FifoFeeder {
    fn start(path: PathBuf, manifest_path: PathBuf, first: Vec<u8>, second: Vec<u8>) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let writes = Arc::new(AtomicUsize::new(0));
        let worker_cancel = Arc::clone(&cancel);
        let worker_writes = Arc::clone(&writes);
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + CHILD_DEADLINE;
            let mut first_writer = open_fifo_writer_until(&path, deadline, &worker_cancel)
                .expect("first FIFO reader did not arrive before deadline");
            write_fifo_snapshot(&mut first_writer, &first);
            drop(first_writer);
            worker_writes.fetch_add(1, Ordering::AcqRel);

            loop {
                if worker_cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
                    return;
                }
                if let Ok(bytes) = fs::read(&manifest_path) {
                    if bytes.ends_with(b"\n")
                        && serde_json::from_slice::<serde_json::Value>(&bytes[..bytes.len() - 1])
                            .is_ok()
                    {
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(5));
            }

            // Do not open the next writer until the complete prepared record
            // exists. A second preload read is therefore deterministic and
            // consumes the distinct SECOND snapshot rather than concatenated
            // FIFO bytes.
            if let Some(mut second_writer) = open_fifo_writer_until(&path, deadline, &worker_cancel)
            {
                write_fifo_snapshot(&mut second_writer, &second);
                drop(second_writer);
                worker_writes.fetch_add(1, Ordering::AcqRel);
            }
        });
        Self {
            cancel,
            writes,
            worker: Some(worker),
        }
    }

    fn stop_and_join(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("FIFO feeder panicked");
        }
    }
}

#[cfg(unix)]
impl Drop for FifoFeeder {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(unix)]
#[test]
fn manifest_and_guest_reuse_the_same_preload_bytes() {
    let scratch = Scratch::new("preload-fifo");
    let preload = scratch.0.join("preload.fifo");
    let manifest_path = scratch.0.join("prepared.json");
    create_fifo(&preload);
    let first = br#"{"dirs":["/"],"files":{"/which":{"text":"FIRST"}},"version":2}"#.to_vec();
    let second = br#"{"dirs":["/"],"files":{"/which":{"text":"SECOND"}},"version":2}"#.to_vec();
    let entry = write_entry(
        &scratch.0,
        "read-preload.js",
        "console.log(vm.fs.read('/which'));",
    );
    let mut feeder = FifoFeeder::start(
        preload.clone(),
        manifest_path.clone(),
        first.clone(),
        second,
    );
    let run = run_cli(
        &scratch.0,
        &strings(&[
            &format!("--preload={}", preload.display()),
            &format!("--run-manifest={}", manifest_path.display()),
            entry.to_str().unwrap(),
        ]),
        b"",
    );
    feeder.stop_and_join();
    assert_normal_cli_result(&run, 0, "FIRST\n");
    assert_eq!(
        feeder.writes.load(Ordering::Acquire),
        1,
        "the input FIFO is consumed exactly once"
    );
    let (_, record) = manifest(&manifest_path);
    assert_eq!(record["inputs"]["preload"]["present"], true);
    assert_eq!(
        record["inputs"]["preload"]["sha256"],
        sha256_hex(&first),
        "record hashes exactly the snapshot bytes consumed by the guest"
    );
}
