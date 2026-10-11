//! Library API: run() owns per-run state, so guests can be reused
//! sequentially and can also run concurrently in one process.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Barrier};
use std::time::Duration;

use v8vm::Config;

#[test]
fn run_returns_the_guest_exit_code_and_can_run_again() {
    let cfg = v8vm::Config::new();
    assert_eq!(v8vm::run("vm.exit(3)", "a.js", &cfg), 3);
    assert_eq!(v8vm::run("", "empty.js", &cfg), 0);
    assert_eq!(v8vm::run("throw new Error('x')", "b.js", &cfg), 1);
    assert_eq!(v8vm::run("for (;;) {}", "spin.js", &v8vm::Config { timeout_ms: 50, ..cfg }), 124);
}

/// Joins a guest thread with a bound, so a guest whose watchdog never fires
/// fails the test instead of hanging it.
fn join_bounded(handle: std::thread::JoinHandle<i32>, secs: u64) -> i32 {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(handle.join());
    });
    match rx.recv_timeout(Duration::from_secs(secs)) {
        Ok(Ok(code)) => code,
        Ok(Err(_)) => panic!("guest thread panicked"),
        Err(_) => panic!("guest did not finish within {} s", secs),
    }
}

fn spawn_guest(src: &str, timeout_ms: u64, gate: Arc<Barrier>) -> std::thread::JoinHandle<i32> {
    let src = src.to_string();
    std::thread::spawn(move || {
        gate.wait();
        let cfg = Config {
            timeout_ms,
            // hermetic: neither guest may consume the test harness stdin
            stdin_consumed: true,
            ..Config::new()
        };
        v8vm::run(&src, "guest.js", &cfg)
    })
}

/// One overlapping round: guest A self-checks its fs at 80 ms and exits 7;
/// guest B self-checks at 20 ms, then spins until its own 50 ms watchdog
/// kills it with 124. Asserts both exit codes and (via the self-check
/// exits 97/98) that neither saw the other's fs.
fn overlap_round() {
    let start = Arc::new(Barrier::new(2));
    let a = spawn_guest(
        r#"
        vm.fs.write('/alfa', 'a');
        setTimeout(() => {
          const names = vm.fs.list('/');
          // if/else: exactly one vm.exit runs, so its code survives V8's
          // asynchronous termination unwind
          if (names.length !== 1 || names[0] !== '/alfa') vm.exit(97);
          else vm.exit(7);
        }, 80);
        "#,
        0,
        start.clone(),
    );
    let b = spawn_guest(
        r#"
        vm.fs.write('/beta', 'b');
        setTimeout(() => {
          const names = vm.fs.list('/');
          if (names.length !== 1 || names[0] !== '/beta') vm.exit(98);
          else for (;;) {}
        }, 20);
        "#,
        50,
        start.clone(),
    );
    assert_eq!(join_bounded(a, 30), 7, "guest A: isolated fs, own exit code");
    assert_eq!(join_bounded(b, 30), 124, "guest B: killed by its own watchdog");
}

#[test]
fn two_guests_run_concurrently_without_sharing_state() {
    // Looping turns the rarest scheduling hiccup into a failure and proves
    // the per-run state holds across many overlapping executions.
    for _ in 0..50 {
        overlap_round();
    }
}

#[test]
fn early_vm_exit_stays_per_run_while_another_guest_is_pending() {
    let start = Arc::new(Barrier::new(2));
    let quick = spawn_guest(
        r#"
        vm.fs.write('/q', 'q');
        vm.exit(31);
        "#,
        0,
        start.clone(),
    );
    let slow = spawn_guest(
        r#"
        setTimeout(() => {
          if (vm.fs.list('/').length !== 0) vm.exit(30);
          else vm.exit(32);
        }, 60);
        "#,
        0,
        start.clone(),
    );
    assert_eq!(join_bounded(quick, 30), 31, "early exit code belongs to its own run");
    assert_eq!(join_bounded(slow, 30), 32, "pending guest keeps its own state and code");
}

#[test]
fn stdin_hub_keeps_unread_input_for_the_next_run() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_stdin-two-runs"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn stdin-two-runs");
    let mut stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");

    // Read child output on a thread so a starved guest fails fast instead
    // of hanging the test.
    let (line_tx, line_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            let _ = line_tx.send(std::mem::take(&mut line));
        }
    });
    let next_line = |what: &str| -> String {
        line_rx
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|_| panic!("no {} output within 30 s", what))
    };

    stdin.write_all(b"one\n").expect("feed first chunk");
    stdin.flush().expect("flush first chunk");
    assert_eq!(next_line("first"), "first:\"one\\n\"\n");

    // This chunk must survive the end of the first run: nothing subscribed
    // consumes it until the second run asks for it.
    stdin.write_all(b"two\n").expect("feed second chunk");
    stdin.flush().expect("flush second chunk");
    assert_eq!(next_line("second"), "second:\"two\\n\"\n");

    drop(stdin); // EOF
    assert!(child.wait().expect("wait child").success());
}
