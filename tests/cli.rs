//! End-to-end tests: drive the v8vm binary the way users and the Makefile do.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Writes `source` to a per-test file under cargo's scratch tmp dir.
fn script(name: &str, source: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let path = dir.join(format!("{}.js", name));
    std::fs::write(&path, source).expect("write test script");
    path
}

fn v8vm(args: &[&str], stdin: &[u8]) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_v8vm"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn v8vm");
    let mut pipe = child.stdin.take().unwrap();
    let input = stdin.to_vec();
    // Feed on a thread: a large input must not deadlock against the child.
    let feeder = std::thread::spawn(move || {
        let _ = pipe.write_all(&input);
    });
    let out = child.wait_with_output().expect("wait v8vm");
    feeder.join().unwrap();
    Run {
        code: out.status.code().expect("exit code, not a signal"),
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
    }
}

fn run_js(name: &str, source: &str) -> Run {
    let path = script(name, source);
    v8vm(&[path.to_str().unwrap()], b"")
}

/// Waits for a child with a bound, so a guest that ignores its termination
/// fails the test instead of hanging it. stdin must already be taken (and
/// held) by the caller before calling this.
fn wait_within(mut child: std::process::Child, secs: u64) -> std::process::Output {
    child.stdin.take(); // never close the pipe: the guest idles on it
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(std::time::Duration::from_secs(secs)) {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => panic!("wait failed: {}", e),
        Err(_) => panic!("guest did not exit within {} s", secs),
    }
}

#[test]
fn console_log_and_error_use_separate_streams() {
    let r = run_js("console", "console.log('out', 1); console.error('err');");
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "out 1\n");
    assert_eq!(r.stderr, "err\n");
}

#[test]
fn vm_exit_sets_the_exit_code() {
    assert_eq!(run_js("exit", "vm.exit(7)").code, 7);
}

#[test]
fn uncaught_exception_exits_1() {
    let r = run_js("throw", "throw new Error('boom')");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("boom"), "stderr: {}", r.stderr);
}

#[test]
fn unhandled_rejection_exits_1() {
    let r = run_js("reject", "Promise.reject(new Error('nope'))");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("nope"), "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("v8vm: unhandled promise rejection:"),
        "stderr: {}",
        r.stderr
    );
}

#[test]
fn rejection_handled_in_the_same_tick_passes() {
    let r = run_js(
        "rej-same-tick",
        "const p = Promise.reject(new Error('later'));
         p.catch((e) => console.log('caught', e.message));",
    );
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "caught later\n");
}

#[test]
fn rejection_handled_one_microtask_later_passes() {
    let r = run_js(
        "rej-microtask",
        "const p = Promise.reject(1);
         Promise.resolve().then(() => p.catch(() => console.log('ok')));",
    );
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "ok\n");
}

#[test]
fn rejection_handled_after_a_timer_fails() {
    // Node semantics: only still-unhandled after the microtask queue drains.
    let r = run_js(
        "rej-after-timer",
        "const p = Promise.reject(1);
         setTimeout(() => p.catch(() => {}), 20);",
    );
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("unhandled promise rejection: 1"), "stderr: {}", r.stderr);
}

#[test]
fn a_zero_ms_timer_cannot_rescue_a_rejection() {
    // The end-of-script checkpoint decides before any timer turn runs,
    // matching Node (the busy-wait keeps the timer from firing in-script).
    let r = run_js(
        "rej-zero-timer",
        "const p = Promise.reject(1);
         setTimeout(() => p.catch(() => console.log('late-caught')), 0);
         const t = Date.now(); while (Date.now() - t < 10) {}",
    );
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("unhandled promise rejection: 1"), "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "");
}

#[test]
fn a_catch_in_a_later_timer_cannot_rescue_within_the_turn() {
    // Both timers are due in the same turn; the checkpoint after the first
    // callback decides before the second one runs.
    let r = run_js(
        "rej-two-timers",
        "let p;
         setTimeout(() => { p = Promise.reject(new Error('r2')); }, 5);
         setTimeout(() => { p.catch(() => console.log('late-caught')); }, 5);
         const t = Date.now(); while (Date.now() - t < 20) {}",
    );
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("unhandled promise rejection: Error: r2"), "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "");
}

#[test]
fn vm_exit_wins_over_a_pending_rejection() {
    // Node: process.exit(0) after Promise.reject(1) still exits 0; the
    // rejection never reaches its tick decision.
    let r = run_js("rej-then-exit", "Promise.reject(1); vm.exit(0);");
    assert_eq!(r.code, 0);
    assert_eq!(r.stderr, "");
}

#[test]
fn a_rejection_from_a_stdin_continuation_fails_the_run() {
    let path = script(
        "rej-stdin",
        "vm.stdin.read().then(() => { throw new Error('boom') });",
    );
    for input in [b"hi\n" as &[u8], b""] {
        let r = v8vm(&[path.to_str().unwrap()], input);
        assert_eq!(r.code, 1, "input {:?}: stderr: {}", input, r.stderr);
        assert!(r.stderr.contains("unhandled promise rejection: Error: boom"), "stderr: {}", r.stderr);
    }
}

#[test]
fn an_oom_inside_a_microtask_reports_137() {
    // The termination is swallowed inside the microtask checkpoint, so the
    // shared reason flag, not V8's state, must conclude the run.
    let path = script(
        "oom-microtask",
        "const a = []; Promise.resolve().then(() => { for (;;) a.push({ x: new Array(1e4).fill(1) }); });",
    );
    let r = v8vm(&["--memory=32", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("heap limit exceeded"), "stderr: {}", r.stderr);
}

#[test]
fn the_watchdog_reaches_a_microtask_that_spins() {
    let path = script("spin-microtask", "Promise.resolve().then(() => { for (;;) {} });");
    let start = std::time::Instant::now();
    let r = v8vm(&["--timeout=300", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 124, "stderr: {}", r.stderr);
    assert!(start.elapsed().as_secs() < 2, "took {:?}", start.elapsed());
}

#[test]
fn the_watchdog_reaches_a_guest_idle_on_a_far_timer() {
    let path = script("idle-timer", "setTimeout(() => {}, 10000);");
    let start = std::time::Instant::now();
    let r = v8vm(&["--timeout=1000", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 124, "stderr: {}", r.stderr);
    assert!(start.elapsed().as_secs() < 2, "took {:?}", start.elapsed());
}

#[test]
fn the_watchdog_reaches_a_guest_idle_on_stdin() {
    let path = script("idle-stdin", "vm.stdin.read().then(() => {});");
    let mut child = Command::new(env!("CARGO_BIN_EXE_v8vm"))
        .args(["--timeout=1000", path.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn v8vm");
    let _held = child.stdin.take(); // open, never written: the guest idles
    let start = std::time::Instant::now();
    let out = wait_within(child, 3);
    assert_eq!(out.status.code(), Some(124));
    assert!(start.elapsed().as_secs() < 2, "took {:?}", start.elapsed());
}

#[test]
fn rejection_value_is_stringified_in_stderr() {
    let r = run_js("rej-stringified", "Promise.reject({ code: 42 })");
    assert_eq!(r.code, 1);
    assert!(
        r.stderr.contains("v8vm: unhandled promise rejection: [object Object]"),
        "stderr: {}",
        r.stderr
    );
}

#[test]
fn async_function_throwing_without_a_catcher_fails() {
    let r = run_js("rej-async", "(async () => { throw new Error('async boom'); })();");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("async boom"), "stderr: {}", r.stderr);
}

#[test]
fn timeout_kills_a_spinning_guest_with_124() {
    let path = script("spin", "for (;;) {}");
    let r = v8vm(&["--timeout=100", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 124);
}

#[test]
fn memory_zero_is_a_usage_error() {
    let path = script("mem-zero", "console.log('never runs');");
    let r = v8vm(&["--memory=0", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("--memory"), "stderr: {}", r.stderr);
}

#[test]
fn memory_below_eight_is_a_usage_error() {
    let path = script("mem-four", "console.log('never runs');");
    let r = v8vm(&["--memory=4", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("at least 8"), "stderr: {}", r.stderr);
}

#[test]
fn map_number_growth_reports_137_not_v8s_fatal_oom() {
    // Table growth never reached the near-heap-limit callback on the old
    // single-layer design: V8 aborted the process instead.
    let path = script("oom-map", "const m = new Map(); for (let i = 0;; i++) m.set(i, i);");
    for cap in ["32", "64"] {
        let r = v8vm(&[&format!("--memory={}", cap), path.to_str().unwrap()], b"");
        assert_eq!(r.code, 137, "cap {}: stderr: {}", cap, r.stderr);
        assert!(
            r.stderr.contains("v8vm: heap limit exceeded"),
            "cap {}: stderr: {}",
            cap,
            r.stderr
        );
    }
    // (At 256 this exact guest races V8's own Map size limit - see
    // set_growth_reports_137_at_the_largest_cap for the deterministic
    // over-cap shape there.)
}

#[test]
fn small_object_growth_reports_137() {
    // Fine-grained growth never reaches the near-heap-limit callback in
    // this V8; the GC epilogue is what catches it at the cap.
    let path = script("oom-objects", "const a = []; for (;;) a.push({});");
    let r = v8vm(&["--memory=32", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("v8vm: heap limit exceeded (32 MB)"), "stderr: {}", r.stderr);
}

#[test]
fn set_growth_reports_137_at_the_largest_cap() {
    // The deterministic over-cap shape at 256: the Map-size race above does
    // not exist for string Sets.
    let path = script("oom-set", "const s = new Set(); for (let i = 0;; i++) s.add('s' + i);");
    let r = v8vm(&["--memory=256", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("v8vm: heap limit exceeded"), "stderr: {}", r.stderr);
}

#[test]
fn map_of_objects_growth_reports_137() {
    let path = script(
        "oom-map-obj",
        "const m = new Map(); for (let i = 0;; i++) m.set(i, { a: i, b: 'k' + i });",
    );
    let r = v8vm(&["--memory=32", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
}

#[test]
fn object_key_growth_reports_137() {
    let path = script("oom-objkeys", "const o = {}; for (let i = 0;; i++) o['k' + i] = { a: i };");
    let r = v8vm(&["--memory=32", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
}

#[test]
fn string_concat_reports_the_range_error_not_an_abort() {
    // Cons strings keep the live heap small; the run ends at V8's maximum
    // string length with an ordinary uncaught RangeError (Node does the
    // same), not with the heap cap or a fatal abort.
    let path = script("oom-str", "let s = 'x'; for (;;) s = s + s;");
    let r = v8vm(&["--memory=32", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 1, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("Invalid string length"), "stderr: {}", r.stderr);
}

#[test]
fn the_smallest_supported_cap_reports_137() {
    let path = script("oom-cap8", "const m = new Map(); for (let i = 0;; i++) m.set(i, i);");
    let r = v8vm(&["--memory=8", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("v8vm: heap limit exceeded (8 MB)"), "stderr: {}", r.stderr);
}

#[test]
fn big_temporary_arrays_under_the_cap_never_report_137() {
    // The round-2 blocker: garbage counted as usage. 100 temporary 8 MB
    // arrays with a tiny live set must finish cleanly.
    let path = script(
        "churn-big-arrays",
        "let x = 0; for (let r = 0; r < 100; r++) { const t = new Array(1e6).fill(r); x += t.length; }
         console.log('done', x);",
    );
    for cap in ["64", "256"] {
        let r = v8vm(&[&format!("--memory={}", cap), path.to_str().unwrap()], b"");
        assert_eq!(r.code, 0, "cap {}: stderr: {}", cap, r.stderr);
        assert_eq!(r.stdout, "done 100000000\n");
    }
}

#[test]
fn moderate_array_churn_under_the_cap_never_reports_137() {
    let path = script(
        "churn-moderate",
        "let x = 0; for (let r = 0; r < 200; r++) { const t = new Array(200000).fill(r); x += t.length; }
         console.log('done', x);",
    );
    let r = v8vm(&["--memory=32", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "done 40000000\n");
}

#[test]
fn a_live_set_with_map_copies_under_the_cap_never_reports_137() {
    // p3.js scaled down: a real live set plus repeated whole-set copies.
    let path = script(
        "live-plus-copies",
        "const live = []; for (let i = 0; i < 200000; i++) live.push({ i, n: 'v' + i, c: [i] });
         let s = 0; for (let r = 0; r < 8; r++) { const t = live.map((o) => ({ ...o, r })); s += t.length; }
         console.log('done', s);",
    );
    let r = v8vm(&["--memory=256", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "done 1600000\n");
}

#[test]
fn arraybuffer_budget_fractions_of_the_cap() {
    // Held buffers at 25/50/90 percent of the cap with churn on top must
    // all pass; the shared budget refuses only past the cap.
    for (live, cap) in [("16", "64"), ("32", "64"), ("57", "64")] {
        let source = "const keep = []; let n = 0;
             for (let i = 0; i < 200; i++) {
               const b = new Uint8Array(1048576); b[0] = i; keep.push(b);
               if (keep.length > LIVE) keep.shift(); n++;
               for (let k = 0; k < 2000; k++) n = (n + k * i) | 0;
             }
             console.log('done', n !== 0);"
            .replace("LIVE", live);
        let path = script("ab-fraction", &source);
        let r = v8vm(&[&format!("--memory={}", cap), path.to_str().unwrap()], b"");
        assert_eq!(r.code, 0, "live {}: stderr: {}", live, r.stderr);
        assert!(r.stdout.starts_with("done"), "stdout: {}", r.stdout);
    }
}

#[test]
fn a_caught_arraybuffer_failure_does_not_kill_later_work() {
    let path = script(
        "ab-caught",
        "for (const e of [30, 34, 36, 37, 38, 40, 45, 52]) {
           try { const b = new ArrayBuffer(2 ** e); console.log(e, 'ok', b.byteLength); }
           catch (err) { console.log(e, err.name); }
         }
         const fine = new ArrayBuffer(1 << 20); new Uint8Array(fine).fill(1);
         setTimeout(() => console.log('later', fine.byteLength), 5);",
    );
    let r = v8vm(&["--memory=32", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(r.stdout.contains("RangeError"), "stdout: {}", r.stdout);
    assert!(r.stdout.ends_with("later 1048576\n"), "stdout: {}", r.stdout);
}

#[test]
fn a_huge_memory_flag_does_not_wrap() {
    // 16 TiB in MB used to shift-wrap to zero at two call sites; a zero
    // cap would refuse this buffer.
    let path = script("huge-cap", "const b = new Uint8Array(1 << 20); b[0] = 1; console.log('ok', b[0]);");
    let r = v8vm(&["--memory=17592186044416", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "ok 1\n");
}

#[test]
fn a_guest_exit_prefers_code_then_reason_over_exceptions() {
    // A blocked callback returns undefined, the guest throws before V8
    // observes the termination - the exit code / reason still wins.
    let path = script("exit-then-use", "vm.exit(2); vm.fs.read('/x').split(',');");
    let r = v8vm(&[path.to_str().unwrap()], b"");
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
    assert_eq!(r.stderr, "");
}

#[test]
fn a_timeout_beats_the_exception_from_a_blocked_callback() {
    let path = script("hammer-exit", "vm.fs.write('/x', 'abc'); for (;;) { vm.fs.read('/x').length; }");
    let r = v8vm(&["--timeout=50", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 124, "stderr: {}", r.stderr);
    assert_eq!(r.stderr, "");
}

#[test]
fn a_callback_loop_oom_reports_137_not_a_type_error() {
    let path = script(
        "oom-callback-loop",
        "vm.fs.write('/x', 'y'); const a = [];
         for (;;) { a.push(vm.fs.read('/x').length, new Array(1000).fill(1)); }",
    );
    let r = v8vm(&["--memory=32", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("v8vm: heap limit exceeded (32 MB)"), "stderr: {}", r.stderr);
    assert!(!r.stderr.contains("TypeError"), "stderr: {}", r.stderr);
}

#[test]
fn vm_exit_beats_the_stdin_continuation() {
    let path = script("exit-stdin", "vm.exit(3); vm.stdin.read().then(() => {});");
    let r = v8vm(&[path.to_str().unwrap()], b"");
    assert_eq!(r.code, 3, "stderr: {}", r.stderr);
    assert_eq!(r.stderr, "");
}

#[test]
fn a_hundred_mib_arraybuffer_under_the_cap_passes() {
    // External (ArrayBuffer) memory counts toward the cap: 100 MiB is
    // under a 256 MiB cap, so the run must succeed.
    let path = script("one-buffer", "new Uint8Array(100 << 20).fill(1); console.log('ok');");
    let r = v8vm(&["--memory=256", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "ok\n");
}

#[test]
fn retained_arraybuffers_past_the_cap_fail_with_a_range_error() {
    // The counting allocator refuses the allocation that would cross the
    // cap; V8 turns the refusal into a RangeError, which this guest does
    // not catch (a catching guest keeps running - see the fail2 shape).
    let path = script("typed-oom", "const a = []; for (;;) a.push(new Uint8Array(1 << 20));");
    let start = std::time::Instant::now();
    let r = v8vm(&["--memory=256", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 1, "stderr: {}", r.stderr);
    assert!(start.elapsed().as_secs() < 5, "took {:?}", start.elapsed());
    assert!(
        r.stderr.contains("RangeError") && r.stderr.contains("Array buffer allocation failed"),
        "stderr: {}",
        r.stderr
    );
    // RSS stays near the cap: the run ends at the refusal, not gigabytes
    // later. (Measured directly with /usr/bin/time: ~30 MB at cap 256.)
}

#[test]
fn vm_exit_stops_host_effects_immediately() {
    // The termination V8 needs a stack check to observe; the host must not
    // run side effects in that window.
    let r = run_js("exit-freeze", "vm.exit(3); console.log('after');");
    assert_eq!(r.code, 3);
    assert_eq!(r.stdout, "");
    assert_eq!(r.stderr, "");
}

#[test]
fn the_first_vm_exit_code_wins() {
    let r = run_js(
        "exit-first-wins",
        "try { vm.exit(3) } catch (e) {} console.log('after'); vm.exit(4);",
    );
    assert_eq!(r.code, 3);
    assert_eq!(r.stdout, "");
}

#[test]
fn fs_writes_after_vm_exit_do_not_land_in_the_dump() {
    let dump = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("exit-freeze.json");
    let _ = std::fs::remove_file(&dump);
    let path = script(
        "exit-fs-freeze",
        "vm.fs.write('/f', 'x'); vm.exit(0); vm.fs.write('/g', 'y');",
    );
    let r = v8vm(
        &[&format!("--dump={}", dump.display()), path.to_str().unwrap()],
        b"",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let json = std::fs::read_to_string(&dump).expect("dump written");
    assert_eq!(json, "{\"/f\":\"x\"}");
}

#[test]
fn vm_exit_inside_a_timer_callback_stops_effects() {
    let dump = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("exit-freeze-timer.json");
    let _ = std::fs::remove_file(&dump);
    let path = script(
        "exit-timer-freeze",
        "vm.fs.write('/f', 'x');
         setTimeout(() => { vm.exit(0); vm.fs.write('/g', 'y'); console.log('after'); }, 5);",
    );
    let r = v8vm(
        &[&format!("--dump={}", dump.display()), path.to_str().unwrap()],
        b"",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "");
    let json = std::fs::read_to_string(&dump).expect("dump written");
    assert_eq!(json, "{\"/f\":\"x\"}");
}

#[test]
fn dump_is_skipped_after_an_oom() {
    let dump = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("oom-dump.json");
    let _ = std::fs::remove_file(&dump);
    let path = script("oom-dump", "const a = []; for (;;) a.push(new Array(1e5).fill(1));");
    let r = v8vm(
        &[
            "--memory=32",
            &format!("--dump={}", dump.display()),
            path.to_str().unwrap(),
        ],
        b"",
    );
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("v8vm: dump skipped after heap limit"),
        "stderr: {}",
        r.stderr
    );
    assert!(!dump.exists(), "no dump may be written from an exhausted heap");
}

#[test]
fn heap_cap_oom_exits_137_quickly_without_a_signal() {
    // The run helper unwraps status.code(), so a signal death (V8's own
    // OOM abort, SIGTRAP) fails this test before the code assert runs.
    let path = script("oom", "const a = []; for (;;) a.push(new Array(1e5).fill(1));");
    let start = std::time::Instant::now();
    let r = v8vm(&["--memory=32", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("v8vm: heap limit exceeded (32 MB)"),
        "stderr: {}",
        r.stderr
    );
    assert!(start.elapsed().as_secs() < 10, "took {:?}", start.elapsed());
}

#[test]
fn guests_staying_under_the_cap_are_unaffected() {
    let path = script(
        "under-cap",
        "const a = []; for (let i = 0; i < 100; i++) a.push(new Array(1000).fill(7));
         console.log(a.length);",
    );
    let r = v8vm(&["--memory=32", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "100\n");
}

#[test]
fn a_small_memory_cap_still_creates_the_isolate() {
    // The initial heap used to be hard-coded at 64 MiB, so any cap below
    // that died with a V8 check failure (SIGTRAP) before the guest ran.
    let path = script("mem-16", "console.log('fits');");
    let r = v8vm(&["--memory=16", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "fits\n");
}

#[test]
fn usage_errors_exit_2() {
    assert_eq!(v8vm(&[], b"").code, 2);
    assert_eq!(v8vm(&["--bogus", "x.js"], b"").code, 2);
    assert_eq!(v8vm(&["/nonexistent/entry.js"], b"").code, 2);
}

#[test]
fn timers_fire_in_due_order_and_clear_works() {
    let r = run_js(
        "timers",
        "const seen = [];
         setTimeout(() => seen.push('b'), 40);
         setTimeout(() => seen.push('a'), 5);
         const c = setTimeout(() => seen.push('cleared'), 10);
         clearTimeout(c);
         let n = 0;
         const iv = setInterval(() => { if (++n === 3) { clearInterval(iv); seen.push('iv'); } }, 20);
         setTimeout((x, y) => { seen.push(x + y); console.log(seen.join(',')); }, 200, 'x', 'y');",
    );
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "a,b,iv,xy\n");
}

#[test]
fn a_storm_of_set_and_clear_leaves_no_timers_behind() {
    // None of the 100k cleared timers may ever fire; the loop must reach
    // the checking timer. (Bookkeeping itself is the O(1) RunningTimer
    // slot, covered by the unit tests.)
    let r = run_js(
        "clear-storm",
        "let ran = 0;
         for (let i = 0; i < 100000; i++) clearTimeout(setTimeout(() => { ran++; }, 300));
         setTimeout(() => console.log('ran=' + ran), 400);",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "ran=0\n");
}

#[test]
fn a_fractional_clear_inside_its_own_callback_does_not_stop_it() {
    // clearTimeout(iv + 0.5) must not match timer iv: the interval keeps
    // running past the fractional clear and is only stopped by the exact
    // clearInterval in the checking timer.
    let r = run_js(
        "fractional-clear",
        "let n = 0;
         const iv = setInterval(() => { n++; if (n === 2) clearTimeout(iv + 0.5); }, 5);
         setTimeout(() => { clearInterval(iv); console.log('n=' + n); }, 100);",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(r.stdout.starts_with("n="), "stdout: {}", r.stdout);
    assert_ne!(r.stdout, "n=2\n", "the interval must survive the fractional clear");
}

#[test]
fn rejecting_with_a_symbol_still_prints_a_reason() {
    let r = run_js("rej-symbol", "Promise.reject(Symbol('s'))");
    assert_eq!(r.code, 1);
    assert!(
        r.stderr.contains("v8vm: unhandled promise rejection: <unprintable rejection value>"),
        "stderr: {}",
        r.stderr
    );
}

#[test]
fn a_hundred_thousand_reject_then_catch_round_trips_quickly() {
    // Forgetting used to scan every remembered rejection (O(n^2): 50k took
    // 7 s, 200k over a minute). The bound is generous on purpose: the point
    // is the curve, not the machine.
    let start = std::time::Instant::now();
    let r = run_js(
        "rejection-storm",
        "const ps = [];
         for (let i = 0; i < 100000; i++) ps.push(Promise.reject(i));
         for (const p of ps) p.catch(() => {});
         console.log('done');",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "done\n");
    assert!(start.elapsed().as_secs() < 2, "took {:?}", start.elapsed());
}

#[test]
fn net_messages_arrive_one_generation_per_turn() {
    let r = run_js(
        "net",
        "const server = vm.net.port(), client = vm.net.port();
         vm.net.listen(server, (p, from) => vm.net.send(server, from, 'echo:' + p));
         vm.net.listen(client, (p) => { console.log(p); vm.net.close(client); vm.net.close(server); });
         vm.net.send(client, server, 'ping');
         try { vm.net.send(client, 999, 'x'); } catch (e) { console.log(e.message); }",
    );
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "no listener on port 999\necho:ping\n");
}

#[test]
fn ramfs_read_write_list_remove() {
    let r = run_js(
        "ramfs",
        "vm.fs.write('/d/a.txt', 'A'); vm.fs.write('/d/b.txt', 'B'); vm.fs.write('/other', 'O');
         console.log(vm.fs.list('/d').join(','));
         vm.fs.remove('/d/a.txt');
         console.log(vm.fs.list('/d').join(','));
         for (const f of [() => vm.fs.read('/d/a.txt'), () => vm.fs.remove('/d/a.txt')]) {
           try { f(); console.log('no throw'); } catch (e) { console.log(e.message); }
         }",
    );
    assert_eq!(r.code, 0);
    assert_eq!(
        r.stdout,
        "/d/a.txt,/d/b.txt\n/d/b.txt\nno such file: /d/a.txt\nno such file: /d/a.txt\n"
    );
}

#[test]
fn dump_then_preload_restores_the_filesystem() {
    let dump = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("roundtrip.json");
    let writer = script("dump-writer", "vm.fs.write('/k', 'v\\u00e9');");
    let w = v8vm(&[&format!("--dump={}", dump.display()), writer.to_str().unwrap()], b"");
    assert_eq!(w.code, 0);

    let reader = script("preload-reader", "console.log(vm.fs.read('/k'));");
    let r = v8vm(&[&format!("--preload={}", dump.display()), reader.to_str().unwrap()], b"");
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "vé\n");
}

#[test]
fn missing_preload_exits_2() {
    let path = script("noop", "");
    let r = v8vm(&["--preload=/nonexistent.json", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 2);
}

#[test]
fn vm_run_evals_a_file_from_the_ramfs() {
    let r = run_js(
        "vmrun",
        "vm.fs.write('/lib.js', 'globalThis.twice = (x) => x * 2;'); vm.run('/lib.js'); console.log(twice(21));",
    );
    assert_eq!(r.stdout, "42\n");
}

const STDIN_ECHO: &str = "
(async () => {
  let total = '';
  for (;;) { const c = await vm.stdin.read(); if (c === '') break; total += c; }
  console.log(total.length + ':' + (total === EXPECTED ? 'same' : 'different'));
})();";

#[test]
fn stdin_keeps_multibyte_characters_whole_across_read_boundaries() {
    // A 2-, 3- and 4-byte character each placed so a 64 KiB read can cut it.
    let mut payload = String::new();
    for ch in ["é", "日", "😀"] {
        payload.push_str(&"a".repeat(65535 - payload.len() % 65536));
        payload.push_str(ch);
        payload.push_str(&"b".repeat(100));
    }
    let expected = payload.encode_utf16().count();
    let source = format!(
        "const EXPECTED = {}; {}",
        serde_json::to_string(&payload).unwrap(),
        STDIN_ECHO
    );
    let path = script("stdin-multibyte", &source);
    let r = v8vm(&[path.to_str().unwrap()], payload.as_bytes());
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, format!("{}:same\n", expected));
}

#[test]
fn stdin_from_entry_dash_is_at_eof() {
    let r = v8vm(
        &["-"],
        b"vm.stdin.read().then((c) => console.log('[' + c + ']'));",
    );
    assert_eq!(r.stdout, "[]\n");
}

/// Each serve test gets its own entry file: they run in parallel and a
/// shared path races writer against reader (an empty read silently skips
/// the entry script).
fn serve(entry: &str, commands: &str) -> Run {
    let path = script(entry, "globalThis.state = { n: 0 };");
    v8vm(&["--serve", path.to_str().unwrap()], commands.as_bytes())
}

#[test]
fn serve_answers_one_json_line_per_command() {
    let r = serve("serve-answers", "1 + 1\nstate.n += 5\n({a: [1, 'x']})\n");
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "2\n5\n{\"a\":[1,\"x\"]}\n");
}

#[test]
fn serve_accepts_a_trailing_line_comment() {
    let r = serve("serve-comment", "40 + 2 // the answer\n");
    assert_eq!(r.stdout, "42\n");
}

#[test]
fn serve_prints_null_for_undefined_results() {
    let r = serve("serve-null", "undefined\n(() => 1)\nstate.missing\n");
    assert_eq!(r.stdout, "null\nnull\nnull\n");
}

#[test]
fn serve_reports_thrown_errors_as_json_and_keeps_going() {
    let r = serve("serve-errors", "(() => { throw new Error('bad') })()\n7\n");
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "{\"$error\":\"bad\"}\n7\n");
}

#[test]
fn serve_settles_timers_between_commands() {
    let r = serve("serve-timers", "setTimeout(() => { state.n = 9 }, 5)\nstate.n\n");
    assert_eq!(r.stdout, "1\n9\n"); // first line is the timer id
}

#[test]
fn an_unhandled_rejection_inside_a_serve_command_fails_the_session() {
    let path = script("serve-reject", "globalThis.p = null;");
    let r = v8vm(
        &["--serve", path.to_str().unwrap()],
        b"p = Promise.reject('sv')\n42\n",
    );
    assert_eq!(r.code, 1, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "{}\n", "the reply goes out before the boundary decides");
    assert!(
        r.stderr.contains("v8vm: unhandled promise rejection: sv"),
        "stderr: {}",
        r.stderr
    );
}

#[test]
fn an_oom_inside_a_serve_expression_reports_137() {
    let path = script("serve-oom", "");
    let r = v8vm(
        &["--memory=32", "--serve", path.to_str().unwrap()],
        b"(() => { const a = []; for (;;) a.push(new Array(1e5).fill(1)); })()\n",
    );
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("heap limit exceeded"), "stderr: {}", r.stderr);
}

#[test]
fn serve_replies_reach_stdout_before_later_guest_output() {
    // Rendezvous: reply N must be on stdout before anything the guest prints
    // after reply N. A queued net message is delivered by the event loop's
    // first phase with no timer floor, so a missing ack shows up as a
    // misplaced line; the 0 ms timer covers the scheduled case. Loop to
    // catch the race.
    let path = script("serve-order", "globalThis.n = 0;");
    // One expression per command: the comma operator schedules work and
    // yields the reply value.
    let commands = b"(vm.net.listen(1, () => console.log('n1')), vm.net.send(1, 1, 'x'), 'A')\n(vm.net.listen(2, () => console.log('n2')), vm.net.send(2, 2, 'x'), 'B')\nsetTimeout(() => console.log('t'), 0), 'C'\n";
    let expected = "\"A\"\nn1\n\"B\"\nn2\n\"C\"\nt\n";
    let mut violations = 0;
    for _ in 0..50 {
        let r = v8vm(&["--serve", path.to_str().unwrap()], commands);
        assert_eq!(r.code, 0, "stderr: {}", r.stderr);
        if r.stdout != expected {
            violations += 1;
        }
    }
    assert_eq!(violations, 0, "reply ordering violated {} of 50 times", violations);
}

/// Like `v8vm`, but kills the child and fails if it outlives `limit`, so a
/// hang regression is a test failure instead of a stuck test run.
fn v8vm_bounded(args: &[&str], stdin: &[u8], limit: std::time::Duration) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_v8vm"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn v8vm");
    let mut pipe = child.stdin.take().unwrap();
    let input = stdin.to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = pipe.write_all(&input);
        // pipe drops here: EOF without any trailing newline from us
    });
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll v8vm") {
            break status;
        }
        if start.elapsed() > limit {
            child.kill().expect("kill hung v8vm");
            child.wait().expect("reap v8vm");
            panic!("v8vm still running after {:?}", limit);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    feeder.join().unwrap();
    let out = child.wait_with_output().expect("collect v8vm output");
    Run {
        code: status.code().expect("exit code, not a signal"),
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
    }
}

#[test]
fn serve_answers_a_final_command_without_a_trailing_newline_and_exits() {
    let path = script("serve-no-newline", "globalThis.s = 1;");
    let r = v8vm_bounded(
        &["--serve", path.to_str().unwrap()],
        b"1+1",
        std::time::Duration::from_secs(10),
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "2\n");
}
