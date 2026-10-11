//! Library API: run() owns per-run state, so guests can be reused
//! sequentially and can also run concurrently in one process.

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
    // The slow guest finishes naturally (no vm.exit of its own): with a
    // leaked exit code it would end with 31 instead of 0.
    let slow = spawn_guest(
        r#"
        setTimeout(() => {
          if (vm.fs.list('/').length !== 0) vm.exit(30);
        }, 60);
        "#,
        0,
        start.clone(),
    );
    assert_eq!(join_bounded(quick, 30), 31, "early exit code belongs to its own run");
    assert_eq!(join_bounded(slow, 30), 0, "naturally-finished guest sees no leaked exit code");
}

#[test]
fn preload_data_alone_seeds_the_filesystem() {
    // A host that pre-read the snapshot (sandboxed runs) sets no preload path.
    let cfg = Config {
        preload_data: Some(r#"{"/k":"v"}"#.to_string()),
        stdin_consumed: true,
        ..Config::new()
    };
    let guest = "if (vm.fs.read('/k') !== 'v') vm.exit(9);";
    assert_eq!(v8vm::run(guest, "preload-data.js", &cfg), 0);
}

#[test]
fn preload_data_takes_the_same_restore_path_as_a_preload_file() {
    // A sandboxed host hands over pre-read text; it must meet the same v2,
    // legacy and version checks a file read does, not a private shortcut.
    let run_with = |data: &str, guest: &str| {
        let cfg = Config {
            preload_data: Some(data.to_string()),
            stdin_consumed: true,
            ..Config::new()
        };
        v8vm::run(guest, "preload-data-paths.js", &cfg)
    };
    let v2 = r#"{"dirs":["/","/d"],"files":{"/d/b.bin":{"b64":"AP8="}},"version":2}"#;
    let check_v2 = "const b = vm.fs.readBytes('/d/b.bin');
                    if (b.length !== 2 || b[0] !== 0 || b[1] !== 255) vm.exit(9);";
    assert_eq!(run_with(v2, check_v2), 0, "v2 snapshot with a binary file");
    let legacy = r#"{"rel/x.txt":"hi"}"#;
    assert_eq!(run_with(legacy, "if (vm.fs.read('/rel/x.txt') !== 'hi') vm.exit(9);"), 0);
    // restore throws on an unknown version, which aborts the run with 2.
    assert_eq!(run_with(r#"{"version":3}"#, "vm.exit(0);"), 2);
}

#[test]
fn fs_quota_boundary_from_the_library_config() {
    let cfg = Config {
        // 64 bookkeeping + 2 key bytes ("/f") + 10 content.
        fs_quota_bytes: 76,
        stdin_consumed: true, // hermetic: never touch the harness stdin
        ..Config::new()
    };
    // A 10-byte file fills it exactly; no vm.exit inside try blocks here:
    // vm.exit returns normally and the guest keeps running until V8's
    // next stack-guard check, so later statements would overwrite the
    // exit code (a separate branch owns that fix).
    let exact = "vm.fs.write('/f', '0123456789');";
    assert_eq!(v8vm::run(exact, "quota-exact.js", &cfg), 0);
    // One byte over, on a fresh fs: the write must throw (exit 1).
    let over = "vm.fs.write('/f', '01234567890');";
    assert_eq!(v8vm::run(over, "quota-over.js", &cfg), 1);
    // Failure leaves the filesystem unchanged: still the original content.
    let unchanged = "vm.fs.write('/f', '0123456789');
                    let ok = false;
                    try { vm.fs.write('/f', '01234567890'); } catch (e) {
                      ok = e.message === 'fs quota exceeded'
                        && vm.fs.stat('/f').size === 10
                        && vm.fs.read('/f') === '0123456789';
                    }
                    if (!ok) throw new Error('fs changed by a failed write');";
    assert_eq!(v8vm::run(unchanged, "quota-unchanged.js", &cfg), 0);
}
