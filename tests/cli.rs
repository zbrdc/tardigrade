//! End-to-end tests: drive the v8vm binary the way users and the Makefile do.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod common;

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

/// run_js with extra CLI flags before the script path.
fn run_js_with(flags: &[&str], name: &str, source: &str) -> Run {
    let path = script(name, source);
    let mut args = flags.to_vec();
    args.push(path.to_str().unwrap());
    v8vm(&args, b"")
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
    // Table growth must end in the near-heap-limit callback (137), not in
    // V8's fatal out-of-memory abort.
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
    // Fine-grained growth of small live objects must end in the
    // near-heap-limit callback too.
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
fn large_array_churn_near_the_cap_passes() {
    // The round-4 blocker: multi-megabyte temporaries with a real live set
    // must not fire the heap cap - collections succeed, the run finishes.
    for (live, churn, cap) in [("8", "40", "32"), ("2", "20", "64"), ("25", "20", "128")] {
        let source = "const keep = []; for (let i = 0; i < LIVE; i++) keep.push(new Array(250000).fill(i + 0.5));
             for (let i = 0; i < CHURN; i++) { new Array(CSZ).fill(i + 0.5); }
             console.log('ok');"
            .replace("LIVE", live)
            .replace("CHURN", churn)
            .replace("CSZ", if cap == "128" { "4000000" } else { "1000000" });
        let path = script("churn-large-arrays", &source);
        let r = v8vm(&[&format!("--memory={}", cap), path.to_str().unwrap()], b"");
        assert_eq!(r.code, 0, "live {}: stderr: {}", live, r.stderr);
        assert_eq!(r.stdout, "ok\n", "live {}", live);
    }
}

#[test]
fn multi_megabyte_temporaries_at_a_small_cap_pass() {
    // Same blocker, ch2.js shape: 400 x 4 MB arrays with zero live data.
    let path = script(
        "churn-huge-temps",
        "let x = 0;
         for (let i = 0; i < 400; i++) { x += new Array(500000).fill(i + 0.5).length; }
         console.log('ok', x);",
    );
    let r = v8vm(&["--memory=32", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(r.stdout.starts_with("ok 200000000"), "stdout: {}", r.stdout);

    // And z.js shape at the smallest cap: each 8 MB temporary fills the
    // whole heap; the collections between them must keep the run alive.
    let path = script(
        "churn-cap-sized-temps",
        "let x = 0;
         for (let i = 0; i < 100; i++) { x += new Array(1000000).fill(i).length; }
         console.log('done', x);",
    );
    let r = v8vm(&["--memory=8", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "done 100000000\n");
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
    // all pass; the allocator budget refuses only past the cap.
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
fn a_structured_clone_past_the_arraybuffer_budget_is_a_catchable_data_clone_error() {
    // The clone needs a second 6 MB buffer; the 8 MB budget refuses it, and
    // the refusal surfaces through webapi's DataCloneError path.
    let r = run_js_with(
        &["--memory=8"],
        "clone-over-budget",
        "try { structuredClone(new ArrayBuffer(6 * 1024 * 1024)); console.log('cloned'); }
         catch (e) { console.log('caught', e.name); }
         console.log(structuredClone(new Uint8Array([1, 2, 3])).length);",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "caught DataCloneError\n3\n");
}

#[test]
fn host_built_buffers_draw_on_the_arraybuffer_budget() {
    // vm.fs.readBytes and TextEncoder.encode build their buffers from Rust
    // bytes, not through V8's allocator; retained copies must still hit the
    // 8 MB budget as RangeErrors instead of growing without bound.
    let r = run_js_with(
        &["--memory=8"],
        "host-buffers-budget",
        "vm.fs.write('/big', 'x'.repeat(3 * 1024 * 1024));
         const text = 'y'.repeat(3 * 1024 * 1024);
         for (const [name, make] of [
           ['readBytes', () => vm.fs.readBytes('/big')],
           ['encode', () => new TextEncoder().encode(text)],
         ]) {
           const keep = [];
           try { for (let i = 0; i < 5; i++) keep.push(make()); console.log(name, 'no refusal'); }
           catch (e) { console.log(name, e.name, keep.length); }
         }",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "readBytes RangeError 2\nencode RangeError 2\n");
}

#[test]
fn churning_host_built_buffers_never_trips_the_budget() {
    // Nothing is retained: the refused charge must collect the garbage
    // buffers and retry, as V8 does for its own allocations.
    let r = run_js_with(
        &["--memory=8"],
        "host-buffers-churn",
        "vm.fs.write('/big', 'x'.repeat(3 * 1024 * 1024));
         const text = 'y'.repeat(3 * 1024 * 1024);
         let n = 0;
         for (let i = 0; i < 100; i++) {
           n += new TextEncoder().encode(text).length + vm.fs.readBytes('/big').length;
         }
         console.log('churn', n);",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "churn 629145600\n");
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

/// The dump holds exactly the file '/f' with content 'x': nothing a guest
/// did after its vm.exit (write, mkdir, rename, remove, restore) landed.
fn assert_only_f_dumped(json: &str) {
    let dumped: serde_json::Value = serde_json::from_str(json).expect("dump json");
    assert_eq!(
        dumped,
        serde_json::json!({"dirs": ["/"], "files": {"/f": {"text": "x"}}, "version": 2})
    );
}

#[test]
fn fs_writes_after_vm_exit_do_not_land_in_the_dump() {
    let dump = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("exit-freeze.json");
    let _ = std::fs::remove_file(&dump);
    let path = script(
        "exit-fs-freeze",
        "vm.fs.write('/f', 'x'); vm.exit(0);
         vm.fs.write('/g', 'y'); vm.fs.mkdir('/d'); vm.fs.rename('/f', '/h');
         vm.fs.remove('/f'); vm.restore('{}');",
    );
    let r = v8vm(
        &[&format!("--dump={}", dump.display()), path.to_str().unwrap()],
        b"",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let json = std::fs::read_to_string(&dump).expect("dump written");
    assert_only_f_dumped(&json);
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
    assert_only_f_dumped(&json);
}

#[test]
fn dump_is_written_after_an_oom() {
    // The dump is serialized host-side from the RAM fs and runs no JS, so
    // an exhausted heap cannot hurt it: the files written before the
    // growth are in the dump, and the exit stays 137.
    let dump = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("oom-dump.json");
    let _ = std::fs::remove_file(&dump);
    let path = script(
        "oom-dump",
        "vm.fs.write('/kept.txt', 'before the oom');
         const a = []; for (;;) a.push(new Array(1e5).fill(1));",
    );
    let r = v8vm(
        &[
            "--memory=32",
            &format!("--dump={}", dump.display()),
            path.to_str().unwrap(),
        ],
        b"",
    );
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("v8vm: heap limit exceeded"), "stderr: {}", r.stderr);
    assert!(!r.stderr.contains("dump"), "no dump complaint: {}", r.stderr);
    let json = std::fs::read_to_string(&dump).expect("dump written after an OOM");
    assert!(json.contains("/kept.txt") && json.contains("before the oom"), "dump: {}", json);
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
fn fs_stores_bytes_and_write_accepts_strings_arrays_and_views() {
    let r = run_js(
        "fsbytes",
        "vm.fs.write('/b.bin', new Uint8Array([255, 254, 0, 65]));
         const bs = vm.fs.readBytes('/b.bin');
         console.log(bs.constructor.name, bs.length, bs[0], bs[3]);
         const u8 = new Uint8Array([1, 2, 3, 4]);
         vm.fs.write('/v.bin', u8.buffer);
         vm.fs.write('/dv.bin', new DataView(u8.buffer));
         vm.fs.write('/off.bin', new Uint8Array(u8.buffer, 1, 2));
         vm.fs.write('/s.bin', 'hé');
         vm.fs.write('/z', new Uint8Array(0));
         const rd = (p) => Array.from(vm.fs.readBytes(p)).join(',');
         console.log(rd('/v.bin'), rd('/dv.bin'), rd('/off.bin'), rd('/s.bin'), vm.fs.readBytes('/z').length);
         const text = vm.fs.read('/b.bin');
         console.log(text.charCodeAt(0) === 0xFFFD, text.charCodeAt(3));
         try { vm.fs.write('/x', 42); console.log('no throw'); } catch (e) { console.log(e.message); }",
    );
    assert_eq!(r.code, 0);
    assert_eq!(
        r.stdout,
        "Uint8Array 4 255 65\n1,2,3,4 1,2,3,4 2,3 104,195,169 0\ntrue 65\nwrite data must be a string, ArrayBuffer, or typed array\n"
    );
}

#[test]
fn fs_quota_flag_enforces_the_limit_and_leaves_state_unchanged() {
    // 1 MiB quota; a root-level file of quota minus the 64-byte entry
    // overhead fits exactly, one byte more does not.
    let path = script(
        "quota-cli",
        "const CAP = 1048576 - 64 - '/f'.length;
         vm.fs.write('/f', new Uint8Array(CAP));
         console.log(vm.fs.stat('/f').size);
         try { vm.fs.write('/f', new Uint8Array(CAP + 1)); console.log('no throw'); }
         catch (e) { console.log(e.message, vm.fs.stat('/f').size); }
         try { vm.fs.mkdir('/d', {recursive: true}); } catch (e) { console.log(e.message, vm.fs.exists('/d')); }",
    );
    let r = v8vm(&["--fs-quota=1", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "1048510\nfs quota exceeded 1048510\nfs quota exceeded false\n");
}

#[test]
fn fs_quota_zero_disables_the_limit_and_bad_values_exit_2() {
    let path = script("quota-off", "vm.fs.write('/f', new Uint8Array(1048576 + 1)); console.log(vm.fs.stat('/f').size);");
    let r = v8vm(&["--fs-quota=0", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "1048577\n");
    assert_eq!(v8vm(&["--fs-quota=x", "-"], b"").code, 2);
    assert_eq!(v8vm(&["--fs-quota=18446744073709551616", "-"], b"").code, 2);
}

#[test]
fn snapshot_reports_version_2_with_text_and_b64_entries() {
    let r = run_js(
        "snap-shape",
        "vm.fs.write('/t.txt', 'plain');
         vm.fs.write('/b.bin', new Uint8Array([255, 254]));
         const snap = JSON.parse(vm.snapshot());
         console.log(snap.version, snap.dirs.join(','), snap.files['/t.txt'].text, snap.files['/b.bin'].b64);",
    );
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "2 / plain //4=\n");
}

#[test]
fn dump_preload_roundtrips_binary_files_and_dirs() {
    let dump = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("bin-roundtrip.json");
    let writer = script(
        "bin-writer",
        "vm.fs.write('/img.bin', new Uint8Array([0, 255, 254, 1, 254, 255, 0]));
         vm.fs.mkdir('/cfg', {recursive: true});
         vm.fs.write('/cfg/x', 't');",
    );
    let w = v8vm(&[&format!("--dump={}", dump.display()), writer.to_str().unwrap()], b"");
    assert_eq!(w.code, 0, "stderr: {}", w.stderr);
    let reader = script(
        "bin-reader",
        "const b = vm.fs.readBytes('/img.bin');
         console.log(Array.from(b).join(','), vm.fs.stat('/img.bin').size, vm.fs.exists('/cfg'), vm.fs.read('/cfg/x'), vm.fs.stat('/cfg').type);",
    );
    let r = v8vm(&[&format!("--preload={}", dump.display()), reader.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "0,255,254,1,254,255,0 7 true t dir\n");
}

#[test]
fn preload_accepts_the_legacy_snapshot_fixture() {
    let fixture =
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/legacy-snapshot.json");
    let path = script(
        "legacy-reader",
        "console.log(vm.fs.read('/greeting.txt'), vm.fs.exists('/lib'), vm.fs.list('/').join(','));
         vm.run('/lib/util.js');
         console.log(globalThis.seven);",
    );
    let r = v8vm(&[&format!("--preload={}", fixture), path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "hello from the old format true /greeting.txt,/lib/util.js\n7\n");
}

#[test]
fn restore_in_the_guest_accepts_both_formats_and_reports_bad_ones() {
    let r = run_js(
        "restore-formats",
        "vm.fs.write('/now', 'x');
         vm.restore('{\"/old\":\"legacy\"}');
         console.log(vm.fs.read('/old'), vm.fs.exists('/now'));
         const snap = vm.snapshot();
         vm.fs.remove('/old', {recursive: true});
         vm.restore(snap);
         console.log(vm.fs.read('/old'));
         try { vm.restore('[]'); } catch (e) { console.log(e.message); }",
    );
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "legacy true\nlegacy\ninvalid snapshot: expected an object\n");
}


#[test]
fn fs_write_rejects_detached_but_out_of_bounds_views_write_empty() {
    let r = run_js(
        "fs-buffers",
        "const ab = new ArrayBuffer(8);
         const view = new Uint8Array(ab);
         ab.transfer(); // detaches ab and view with it
         try { vm.fs.write('/d1', view); console.log('view no throw'); }
         catch (e) { console.log(e.message); }
         try { vm.fs.write('/d2', ab); console.log('ab no throw'); }
         catch (e) { console.log(e.message); }
         const rab = new ArrayBuffer(16, {maxByteLength: 32});
         const oob = new Uint8Array(rab, 8, 4);
         rab.resize(4); // the view no longer fits the buffer
         try { vm.fs.write('/d3', oob); console.log('oob wrote', vm.fs.readBytes('/d3').length, 'bytes'); }
         catch (e) { console.log('oob threw', e.message); }
         // shared buffers and views over them are accepted
         const sab = new SharedArrayBuffer(4);
         const sv = new Uint8Array(sab);
         sv.set([9, 8, 7, 6]);
         vm.fs.write('/s1', sv);
         vm.fs.write('/s2', sab);
         console.log(Array.from(vm.fs.readBytes('/s1')).join(','), Array.from(vm.fs.readBytes('/s2')).join(','));
         try { vm.fs.write('/x', 42); console.log('no throw'); } catch (e) { console.log(e.message); }",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout,
        "cannot write from a detached ArrayBuffer\ncannot write from a detached ArrayBuffer\noob wrote 0 bytes\n9,8,7,6 9,8,7,6\nwrite data must be a string, ArrayBuffer, or typed array\n"
    );
}

#[test]
fn fs_partially_and_fully_out_of_bounds_views_write_empty() {
    // A view left out of bounds by a resizable shrink has byteLength 0,
    // exactly as the guest sees it, so the write stores an empty file.
    let r = run_js(
        "fs-oob-views",
        "const cases = [];
         const mk = (off, len) => { const r = new ArrayBuffer(16, {maxByteLength: 32});
                                    const v = new Uint8Array(r, off, len); r.resize(4); return v; };
         vm.fs.write('/partial', mk(2, 8));
         vm.fs.write('/far', mk(8, 4));
         const dv = (() => { const r = new ArrayBuffer(16, {maxByteLength: 32});
                             const v = new DataView(r, 8, 4); r.resize(4); return v; })();
         vm.fs.write('/dv', dv);
         console.log(vm.fs.readBytes('/partial').length, vm.fs.readBytes('/far').length, vm.fs.readBytes('/dv').length);
         // regrown: the view is valid again and writes its real bytes
         const reg = (() => { const r = new ArrayBuffer(16, {maxByteLength: 32});
                              const v = new Uint8Array(r, 8, 4);
                              r.resize(4); r.resize(16); v.set([1,2,3,4]); return v; })();
         vm.fs.write('/reg', reg);
         console.log(Array.from(vm.fs.readBytes('/reg')).join(','));",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "0 0 0\n1,2,3,4\n");
}

#[test]
fn fs_paths_reject_lone_surrogates_but_allow_real_replacement_chars() {
    let r = run_js(
        "fs-surrogates",
        "try { vm.fs.write('/\\uD800', 'x'); console.log('no throw'); } catch (e) { console.log(e.message); }
         vm.fs.write('/real', 'x');
         console.log(vm.fs.exists('/\\uD801'));
         try { vm.fs.read('/\\uD800'); } catch (e) { console.log('read:', e.message); }
         // a genuine U+FFFD is a legal path byte and must round-trip
         vm.fs.write('/a\\uFFFDb', 'x');
         console.log(vm.fs.exists('/a\\uFFFDb'), vm.fs.read('/a\\uFFFDb'));",
    );
    assert_eq!(r.code, 0);
    assert_eq!(
        r.stdout,
        "path contains lone surrogates\nfalse\nread: path contains lone surrogates\ntrue x\n"
    );
}

#[test]
fn fs_replacement_char_paths_round_trip_through_dump_and_preload() {
    let dump = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fffd-path.json");
    let writer = script(
        "fffd-writer",
        "vm.fs.write('/a\\uFFFDb', 'x'); vm.fs.mkdir('/d\\uFFFD', {recursive: true}); vm.fs.write('/d\\uFFFD/f', 'y');",
    );
    let w = v8vm(&[&format!("--dump={}", dump.display()), writer.to_str().unwrap()], b"");
    assert_eq!(w.code, 0, "stderr: {}", w.stderr);
    let reader = script(
        "fffd-reader",
        "console.log(vm.fs.read('/a\\uFFFDb'), vm.fs.exists('/d\\uFFFD'), vm.fs.read('/d\\uFFFD/f'));\n         console.log(vm.fs.list('/').includes('/a\\uFFFDb'), vm.fs.readdir('/').includes('d\\uFFFD'));\n         console.log(vm.snapshot().includes('a\\uFFFDb'));",
    );
    let r = v8vm(&[&format!("--preload={}", dump.display()), reader.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "x true y\ntrue true\ntrue\n");
}

#[test]
fn fs_path_arguments_propagate_to_string_exceptions() {
    let r = run_js(
        "fs-tostring",
        "try { vm.fs.read({ toString() { throw new Error('boom'); } }); console.log('no throw'); }
         catch (e) { console.log(e.message); }
         try { vm.fs.write({ toString() { throw new Error('bad path'); } }, 'x'); console.log('no throw'); }
         catch (e) { console.log(e.message); }",
    );
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "boom\nbad path\n");
}

#[test]
fn fs_read_and_read_bytes_of_a_directory_throw() {
    let r = run_js(
        "fs-dirs",
        "vm.fs.mkdir('/d', {recursive: true});
         for (const f of [() => vm.fs.read('/d'), () => vm.fs.readBytes('/d')]) {
           try { f(); console.log('no throw'); } catch (e) { console.log(e.message); }
         }",
    );
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "is a directory: /d\nis a directory: /d\n");
}

#[test]
fn fs_remove_returns_undefined() {
    let r = run_js(
        "fs-remove-undef",
        "vm.fs.write('/f', 'x');
         const r = vm.fs.remove('/f');
         console.log(typeof r);",
    );
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "undefined\n");
}

#[test]
fn restore_rejects_bad_versions_and_non_canonical_base64() {
    let r = run_js(
        "restore-strict",
        "try { vm.restore('{\"version\":3,\"files\":{}}'); } catch (e) { console.log(e.message); }
         try { vm.restore('{\"version\":2,\"files\":{\"/x\":{\"b64\":\"AA==AAAA\"}}}'); } catch (e) { console.log(e.message); }
         try { vm.restore('{\"version\":2,\"files\":{\"/x\":{\"b64\":\"AB==\"}}}'); } catch (e) { console.log(e.message); }
         vm.restore('{\"version\":2,\"files\":{}}');
         console.log('empty v2 ok');",
    );
    assert_eq!(r.code, 0);
    assert_eq!(
        r.stdout,
        "unsupported snapshot version 3\ninvalid snapshot: bad base64\ninvalid snapshot: bad base64\nempty v2 ok\n"
    );
}

#[test]
fn fs_quota_flag_multiplication_overflow_exits_2() {
    // 2^64 bytes would need an mb value whose << 20 overflows u64.
    assert_eq!(v8vm(&["--fs-quota=17592186044416", "-"], b"").code, 2);
    assert_eq!(v8vm(&["--fs-quota=17592186044415", "-"], b"").code, 0);
}

#[test]
fn preload_accepts_relative_legacy_keys() {
    let fixture =
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/legacy-relative.json");
    let path = script(
        "legacy-relative",
        "console.log(vm.fs.read('/rel/a.txt'), vm.fs.read('/abs.txt'), vm.fs.list('/').join(','));",
    );
    let r = v8vm(&[&format!("--preload={}", fixture), path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "relative key ok absolute key ok /abs.txt,/rel/a.txt\n");
}

#[test]
#[ignore] // memory-heavy: run with `make test-heavy`
fn fs_file_too_large_to_read_as_a_string() {
    // Above V8's maximum string length (~536.87 M chars), read must throw
    // a normal guest Error while readBytes still works. Memory-heavy on
    // purpose: nothing smaller can cross the engine's string limit. The
    // guest's 540 MB buffer and the readBytes copy both count against the
    // ArrayBuffer budget, so the cap is raised above their sum.
    let path = script(
        "fs-huge-read",
        "vm.fs.write('/f', new Uint8Array(540 * 1024 * 1024));
         try { vm.fs.read('/f'); console.log('no throw'); }
         catch (e) { console.log(e.message); }
         console.log(vm.fs.readBytes('/f').length);",
    );
    let r = v8vm(&["--fs-quota=0", "--memory=1200", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "file too large to read as a string\n566231040\n");
}

#[test]
fn restore_rejects_duplicate_paths_and_keeps_the_quota_honest() {
    // bypass.js shape, scaled: fill the quota, then try to shrink the
    // file with many spellings of its own path.
    let path = script(
        "dup-bypass",
        "const SIZE = 1048576 - 66;
         vm.fs.write('/a', new Uint8Array(SIZE));
         const files = {};
         for (let i = 1; i <= 50; i++) files['/'.repeat(i) + 'a'] = { text: '' };
         try { vm.restore(JSON.stringify({ version: 2, files })); console.log('restored'); }
         catch (e) { console.log(e.message); }
         console.log('a size', vm.fs.stat('/a').size);
         try { vm.fs.write('/big', new Uint8Array(1048576)); console.log('over-quota write ok'); }
         catch (e) { console.log(e.message); }
         try { vm.restore('{\"b\":\"1\",\"/b\":\"2\"}'); } catch (e) { console.log(e.message); }",
    );
    let r = v8vm(&["--fs-quota=1", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout,
        "duplicate path in snapshot: /a\na size 1048510\nfs quota exceeded\nduplicate path in snapshot: /b\n"
    );
}

#[test]
fn rename_converts_arguments_one_at_a_time() {
    let r = run_js(
        "rename-order",
        "let secondRan = false;
         try { vm.fs.rename({ toString() { throw new Error('first'); } },
                            { toString() { secondRan = true; throw new Error('second'); } }); }
         catch (e) { console.log('caught', e.message, 'secondRan', secondRan); }
         try { vm.fs.rename('/x', { toString() { throw new Error('to-throws'); } }); }
         catch (e) { console.log('caught', e.message); }",
    );
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "caught first secondRan false\ncaught to-throws\n");
}

#[test]
fn write_probes_string_lengths_against_the_quota() {
    // A multi-byte string is charged by its UTF-8 length, and rejected
    // before the copy, like buffers are.
    let path = script(
        "str-probe",
        "try { vm.fs.write('/s', '\\u00e9'.repeat(600000)); console.log('no throw'); }
         catch (e) { console.log(e.message); }
         vm.fs.write('/s', '\\u00e9'.repeat(100000));
         console.log(vm.fs.stat('/s').size);
         try { vm.fs.write('/t', { toString() { throw new Error('never runs'); } }); }
         catch (e) { console.log(e.message.slice(0, 20)); }",
    );
    let r = v8vm(&["--fs-quota=1", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "fs quota exceeded\n200000\nwrite data must be a\n");
}

#[test]
fn a_failed_dump_exits_74_when_the_guest_succeeded() {
    let path = script("dump-fail", "vm.fs.write('/f', 'x');");
    // Writing the dump into a directory fails; the guest itself was fine.
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let r = v8vm(&[&format!("--dump={}", dir.display()), path.to_str().unwrap()], b"");
    assert_eq!(r.code, 74);
    assert!(r.stderr.contains("cannot write dump"), "stderr: {}", r.stderr);
    // A guest that already failed keeps its own exit code.
    let bad = script("dump-fail-2", "throw new Error('boom');");
    let r2 = v8vm(&[&format!("--dump={}", dir.display()), bad.to_str().unwrap()], b"");
    assert_eq!(r2.code, 1);
    // A healthy dump still exits 0.
    let ok_dump = dir.join("dump-ok.json");
    let r3 = v8vm(&[&format!("--dump={}", ok_dump.display()), path.to_str().unwrap()], b"");
    assert_eq!(r3.code, 0);
}

/// A fresh, empty directory under the scratch dir for one test's dump files.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn fresh_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

/// Names of the entries in `dir`, sorted.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn entries(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("list dir")
        .map(|e| e.expect("dir entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Runs v8vm with RLIMIT_FSIZE set to `limit` bytes, so any write that grows
/// a file past it fails partway with EFBIG. SIGXFSZ is ignored (the default
/// action kills the process); the setting survives the exec.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn v8vm_with_file_size_cap(args: &[&str], limit: u64) -> Run {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_v8vm"));
    cmd.args(args).stdin(Stdio::null());
    // SAFETY: the closure only makes async-signal-safe libc calls.
    unsafe {
        cmd.pre_exec(move || {
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            let cap = libc::rlimit { rlim_cur: limit, rlim_max: limit };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &cap) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = cmd.output().expect("run v8vm");
    Run {
        code: out.status.code().expect("exit code, not a signal"),
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
    }
}

/// The agent loop reads and writes one file (--preload = --dump). A dump
/// that fails partway must leave the previous one whole. RLIMIT_FSIZE makes
/// the failure a real partial write (the first KiB lands, then EFBIG): an
/// in-place std::fs::write has already truncated the old file by then, while
/// the temp-file path loses only the temp file. Unlike a read-only
/// directory, this also fails for root and is the mid-write case itself.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn a_failed_dump_write_leaves_the_old_dump_intact() {
    let dir = fresh_dir("atomic-dump-fail");
    let dump = dir.join("ws.json");
    std::fs::write(&dump, "OLD-DUMP").expect("seed dump");
    let guest = script("atomic-dump-fail", "vm.fs.write('/big', 'x'.repeat(65536));");
    let dump_arg = format!("--dump={}", dump.display());
    let r = v8vm_with_file_size_cap(&[&dump_arg, guest.to_str().unwrap()], 1024);
    assert_eq!(r.code, 74, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("cannot write dump"), "stderr: {}", r.stderr);
    assert!(r.stderr.contains("File too large"), "OS error missing: {}", r.stderr);
    assert_eq!(std::fs::read_to_string(&dump).unwrap(), "OLD-DUMP");
    assert_eq!(entries(&dir), ["ws.json"], "temp file left behind");
}

/// Runs a guest that writes /f and dumps to `dump`, with a time limit so a
/// hang is a failure. The guest script is named after the test.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn dump_run(test: &str, dump: &std::path::Path) -> Run {
    let guest = script(test, "vm.fs.write('/f', 'x');");
    let dump_arg = format!("--dump={}", dump.display());
    v8vm_bounded(&[&dump_arg, guest.to_str().unwrap()], b"", std::time::Duration::from_secs(60))
}

/// The replacement keeps the old dump's mode.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn a_dump_replacement_keeps_the_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = fresh_dir("atomic-dump-mode");
    let dump = dir.join("ws.json");
    std::fs::write(&dump, "OLD").expect("seed dump");
    std::fs::set_permissions(&dump, std::fs::Permissions::from_mode(0o640)).unwrap();
    let r = dump_run("atomic-dump-mode", &dump);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(std::fs::read_to_string(&dump).unwrap().contains("\"/f\""));
    assert_eq!(std::fs::metadata(&dump).unwrap().permissions().mode() & 0o777, 0o640);
    assert_eq!(entries(&dir), ["ws.json"], "temp file left behind");
}

/// A symlink is never renamed over: the kernel resolves it on open, which
/// also keeps fs.protected_symlinks in force in sticky directories. The link
/// stays and the file it names gets the new content (and keeps its mode).
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn a_symlinked_dump_keeps_the_link_and_replaces_the_target_content() {
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    let dir = fresh_dir("symlink-dump");
    let real = dir.join("real.json");
    let link = dir.join("link.json");
    std::fs::write(&real, "OLD").expect("seed dump");
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
    let inode = std::fs::metadata(&real).unwrap().ino();
    symlink("real.json", &link).expect("symlink");
    let r = dump_run("symlink-dump", &link);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    // Written in place: a rename over the resolved target would also keep
    // the link but give the target a new inode.
    assert_eq!(std::fs::metadata(&real).unwrap().ino(), inode, "target was replaced");
    assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink(), "link was replaced");
    assert!(std::fs::read_to_string(&real).unwrap().contains("\"/f\""));
    assert_eq!(std::fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o640);
    assert_eq!(entries(&dir), ["link.json", "real.json"], "temp file left behind");
}

/// A dangling symlink is followed by open(O_CREAT), so the dump creates the
/// file the link names instead of replacing the link with a regular file.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn a_dangling_symlink_dump_creates_the_target_and_keeps_the_link() {
    let dir = fresh_dir("dangling-dump");
    let link = dir.join("link.json");
    std::os::unix::fs::symlink("target.json", &link).expect("symlink");
    let r = dump_run("dangling-dump", &link);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink(), "link was replaced");
    let target = dir.join("target.json");
    assert!(std::fs::read_to_string(&target).unwrap().contains("\"/f\""));
    assert_eq!(entries(&dir), ["link.json", "target.json"], "temp file left behind");
}

/// --dump=/dev/stdout with stdout redirected to a file writes into that same
/// file: a rename over the resolved path would give the shell's descriptor an
/// unlinked inode and lose everything it wrote there.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn a_dump_to_dev_stdout_redirected_to_a_file_keeps_the_inode() {
    use std::os::unix::fs::MetadataExt;
    let dir = fresh_dir("dump-dev-stdout");
    let out = dir.join("out.txt");
    let out_file = std::fs::File::create(&out).expect("create stdout file");
    let inode = out_file.metadata().unwrap().ino();
    let guest = script("dump-dev-stdout", "vm.fs.write('/f', 'x');");
    let mut child = Command::new(env!("CARGO_BIN_EXE_v8vm"))
        .args(["--dump=/dev/stdout", guest.to_str().unwrap()])
        .stdin(Stdio::null())
        .stdout(out_file)
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn v8vm");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll v8vm") {
            break status;
        }
        if std::time::Instant::now() > deadline {
            child.kill().expect("kill hung v8vm");
            child.wait().expect("reap v8vm");
            panic!("v8vm still running after 60s");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(0));
    assert_eq!(std::fs::metadata(&out).unwrap().ino(), inode, "stdout file was replaced");
    assert!(std::fs::read_to_string(&out).unwrap().contains("\"/f\""));
    assert_eq!(entries(&dir), ["out.txt"], "temp file left behind");
}

/// A directory cannot be a dump: exit 74 with the OS error, and nothing is
/// left behind in the parent.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn a_directory_dump_target_exits_74_and_leaves_nothing_behind() {
    let dir = fresh_dir("dump-is-dir");
    let target = dir.join("ws.json");
    std::fs::create_dir(&target).expect("create directory target");
    let r = dump_run("dump-is-dir", &target);
    assert_eq!(r.code, 74, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("cannot write dump"), "stderr: {}", r.stderr);
    assert!(r.stderr.contains("Is a directory"), "OS error missing: {}", r.stderr);
    assert_eq!(entries(&dir), ["ws.json"], "temp file left behind");
    assert!(target.is_dir());
}

/// Documented rename semantics: a read-only (0400) dump is replaced because
/// rename needs no write permission on the file, and its mode is kept.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn a_read_only_dump_is_replaced_and_stays_read_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = fresh_dir("dump-read-only");
    let dump = dir.join("ws.json");
    std::fs::write(&dump, "OLD").expect("seed dump");
    std::fs::set_permissions(&dump, std::fs::Permissions::from_mode(0o400)).unwrap();
    let r = dump_run("dump-read-only", &dump);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(std::fs::read_to_string(&dump).unwrap().contains("\"/f\""));
    assert_eq!(std::fs::metadata(&dump).unwrap().permissions().mode() & 0o777, 0o400);
    assert_eq!(entries(&dir), ["ws.json"], "temp file left behind");
}

/// The temp name embeds the target's name, so a name near NAME_MAX must not
/// make the temp name too long.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn a_250_character_dump_name_is_written() {
    let dir = fresh_dir("dump-long-name");
    let name = format!("{}.json", "n".repeat(245));
    assert_eq!(name.len(), 250);
    let dump = dir.join(&name);
    std::fs::write(&dump, "OLD").expect("seed dump");
    let r = dump_run("dump-long-name", &dump);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(std::fs::read_to_string(&dump).unwrap().contains("\"/f\""));
    assert_eq!(entries(&dir), [name], "temp file left behind");
}

/// A device is not replaceable by rename (and must not get a temp file in
/// /dev): --dump=/dev/null keeps working as the in-place write did.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn a_dump_to_a_device_is_written_in_place() {
    let guest = script("dump-devnull", "vm.fs.write('/f', 'x');");
    let r = v8vm(&["--dump=/dev/null", guest.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
}

#[test]
fn full_quota_control_characters_survive_a_snapshot_round_trip() {
    // ctrl.js shape, scaled down: JSON escapes control characters to six
    // bytes each, so the gate must be 6x the quota for a full-quota
    // filesystem of them to restore from its own snapshot.
    let path = script(
        "ctrl-roundtrip",
        "const base = '/' + '\\u0001'.repeat(3990);
         let n = 0;
         try { for (;;) { vm.fs.write(base + n, '\\u0001\\u0001\\u0001'); n++; } }
         catch (e) { }
         console.log('quota full after', n, 'files');
         const snap = vm.snapshot();
         vm.fs.remove(base + '0', {});
         vm.restore(snap);
         console.log('restored', vm.fs.stat('/').size === n, vm.fs.read(base + '0').length);",
    );
    // A 1 MiB quota: ~260 long control-character keys fill it, and their
    // ~6 MB snapshot (control bytes escape to six each) must clear the
    // 6x gate and restore cleanly - a 4x gate would reject it.
    let r = v8vm(&["--fs-quota=1", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let mut lines = r.stdout.lines();
    let full = lines.next().unwrap();
    assert!(full.starts_with("quota full after "), "stdout: {}", r.stdout);
    let n: usize = full
        .trim_start_matches("quota full after ")
        .trim_end_matches(" files")
        .parse()
        .expect("file count");
    assert!(n > 200, "expected the quota to hold many entries: {}", n);
    assert_eq!(lines.next().unwrap(), "restored true 3");
}

/// One way a run can end, for the every-ending dump test.
struct Ending {
    what: &'static str,
    src: &'static str,
    flags: &'static [&'static str],
    stdin: &'static [u8],
    want: i32,
}

const fn ending(what: &'static str, src: &'static str, want: i32) -> Ending {
    Ending { what, src, flags: &[], stdin: b"", want }
}

/// True when `name` under the scratch dir exists and holds file "/f".
fn dump_has_f(name: &str) -> bool {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    path.exists() && std::fs::read_to_string(&path).unwrap_or_default().contains("\"/f\"")
}

#[test]
fn dump_is_written_for_every_ending() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let endings = [
        ending("vm.exit(0) at top level", "vm.fs.write('/f','x'); vm.exit(0);", 0),
        ending("vm.exit(0) in a timer", "vm.fs.write('/f','x'); setTimeout(() => vm.exit(0), 5);", 0),
        ending("vm.exit(0) in a microtask", "vm.fs.write('/f','x'); Promise.resolve().then(() => vm.exit(0));", 0),
        ending("vm.exit(3)", "vm.fs.write('/f','x'); vm.exit(3);", 3),
        ending("uncaught exception", "vm.fs.write('/f','x'); throw new Error('boom');", 1),
        ending("unhandled rejection", "vm.fs.write('/f','x'); Promise.reject(new Error('no'));", 1),
        Ending {
            flags: &["--timeout=200"],
            ..ending("timeout", "vm.fs.write('/f','x'); for (;;) {}", 124)
        },
        Ending {
            stdin: b"go",
            ..ending(
                "vm.exit driven by stdin",
                "vm.fs.write('/f','x'); vm.stdin.read().then((c) => vm.exit(c === 'go' ? 4 : 5));",
                4,
            )
        },
        Ending {
            flags: &["--serve"],
            stdin: b"1 + 1\n",
            ..ending("--serve to stdin EOF", "vm.fs.write('/f','x');", 0)
        },
    ];
    for (i, e) in endings.iter().enumerate() {
        let script_path = script(&format!("ending-{}", i), e.src);
        let name = format!("ending-{}.json", i);
        let dump = dir.join(&name);
        let _ = std::fs::remove_file(&dump); // only this run's dump counts
        let dump_arg = format!("--dump={}", dump.display());
        let mut args = e.flags.to_vec();
        args.push(&dump_arg);
        args.push(script_path.to_str().unwrap());
        let r = v8vm_bounded(&args, e.stdin, std::time::Duration::from_secs(60));
        assert_eq!(r.code, e.want, "{}: exit code (stderr: {})", e.what, r.stderr);
        assert!(dump_has_f(&name), "{}: dump missing /f", e.what);
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    sandboxed_ending_writes_its_dump();
}

/// --sandbox rewrites the dump in place (Landlock forbids a temp file), so
/// a clean vm.exit(0) must still produce it. Runs only where the kernel can
/// sandbox; elsewhere `sandboxed` checks the refusal and the case is skipped.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn sandboxed_ending_writes_its_dump() {
    let script_path = script("ending-sandbox", "vm.fs.write('/f','x'); vm.exit(0);");
    let dump = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ending-sandbox.json");
    let _ = std::fs::remove_file(&dump);
    let dump_arg = format!("--dump={}", dump.display());
    let args = [dump_arg.as_str(), script_path.to_str().unwrap()];
    let Some(r) = sandboxed("dump_is_written_for_every_ending", &args, b"") else {
        return;
    };
    assert_eq!(r.code, 0, "sandboxed vm.exit(0) (stderr: {})", r.stderr);
    assert!(dump_has_f("ending-sandbox.json"), "sandboxed vm.exit(0): dump missing /f");
}

#[test]
fn fs_error_messages_truncate_huge_paths() {
    let r = run_js(
        "huge-path-msg",
        "const p = 'a'.repeat(300000);
         try { vm.fs.read(p); console.log('no throw'); }
         catch (e) { console.log(e.message.length < 300, e.message.startsWith('path must be absolute: a'), e.message.endsWith('...')); }
         console.log(vm.fs.exists(p));",
    );
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "true true true\nfalse\n");
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
    v8vm_bounded_started(args, stdin, limit, |_| {})
}

/// `v8vm_bounded` that also calls `started` with the child's pid right after
/// the spawn, before any input is sent.
fn v8vm_bounded_started(
    args: &[&str],
    stdin: &[u8],
    limit: std::time::Duration,
    started: impl FnOnce(u32),
) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_v8vm"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn v8vm");
    started(child.id());
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

// ---- --sandbox (OS self-containment) ----

const SANDBOX_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

/// The flag scripts gate their skips on: exit 0 with a word when sandboxing
/// is possible and 2 with the reason otherwise. The kernel is queried
/// independently of the library under test, so a lying answer fails.
#[test]
fn sandbox_check_matches_the_kernel() {
    let r = v8vm(&["--sandbox-check"], b"");
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if common::kernel_supports_sandbox() {
        assert_eq!((r.code, r.stdout.trim()), (0, "supported"), "stderr: {}", r.stderr);
        return;
    }
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("--sandbox is not available"), "stderr: {}", r.stderr);
}

/// The probe flag only means something as the sole argument. Anywhere before
/// the entry it is a usage error, so a typo cannot turn a run into a check.
#[test]
fn sandbox_check_before_the_entry_or_with_other_flags_is_a_usage_error() {
    let path = script("sb-check-misplaced", "console.log('ran');");
    let entry = path.to_str().unwrap();
    for args in [vec!["--sandbox-check", "--bogus"], vec!["--sandbox-check", entry], vec!["--", "--sandbox-check"]] {
        let r = v8vm(&args, b"");
        assert_eq!(r.code, 2, "{:?} stderr: {}", args, r.stderr);
        assert_eq!(r.stdout, "", "{:?}: no probe answer and no guest run", args);
        assert!(r.stderr.contains("--sandbox-check must be the only argument"), "stderr: {}", r.stderr);
    }
}

/// After the entry (with or without `--`) it is an ordinary guest argument:
/// the script runs and sees it in vm.args.
#[test]
fn sandbox_check_after_the_entry_is_a_guest_argument() {
    let path = script("sb-check-arg", "console.log(JSON.stringify(vm.args));");
    let entry = path.to_str().unwrap();
    for args in [vec![entry, "--sandbox-check"], vec![entry, "--", "--sandbox-check"]] {
        let r = v8vm(&args, b"");
        assert_eq!(r.code, 0, "{:?} stderr: {}", args, r.stderr);
        assert_eq!(r.stdout, "[\"--sandbox-check\"]\n", "{:?}", args);
    }
}

/// A dump the sandbox cannot anchor is refused while it prepares, before any
/// kernel layer is installed, so these hold on every kernel.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod sandbox_prepare {
    use super::*;

    #[test]
    fn sandbox_refuses_a_dump_it_cannot_prepare() {
        let dump = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("no-such-dir/sb.json");
        let path = script("sb-refused", "console.log('ran');");
        let dump_arg = format!("--dump={}", dump.display());
        let r = v8vm_bounded(&["--sandbox", &dump_arg, path.to_str().unwrap()], b"", SANDBOX_LIMIT);
        assert_eq!(r.code, 2);
        assert_eq!(r.stdout, "", "guest must not run");
        assert!(r.stderr.contains("sandbox"), "stderr: {}", r.stderr);
    }

    #[test]
    fn sandbox_refuses_a_dump_that_is_not_a_regular_file() {
        // The child's stdout is a pipe, so /dev/stdout is one too.
        let path = script("sb-pipe-dump", "console.log('ran');");
        let r = v8vm_bounded(
            &["--sandbox", "--dump=/dev/stdout", path.to_str().unwrap()],
            b"",
            SANDBOX_LIMIT,
        );
        assert_eq!(r.code, 2);
        assert_eq!(r.stdout, "", "guest must not run");
        assert!(r.stderr.contains("--dump must be a regular file"), "stderr: {}", r.stderr);
    }
}

/// Whether this kernel can sandbox. Where it cannot, the test must then
/// check the refusal with `assert_refused` and return, and a skip notice is
/// printed. V8VM_REQUIRE_SANDBOX=1 makes such a kernel a failure instead.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn sandbox_available(test: &str) -> bool {
    if common::kernel_supports_sandbox() {
        return true;
    }
    assert!(
        !common::sandbox_required(),
        "V8VM_REQUIRE_SANDBOX=1 but the kernel reports Landlock ABI {}, need {}",
        common::landlock_abi(),
        common::REQUIRED_LANDLOCK_ABI
    );
    common::skipped(test);
    false
}

/// On a kernel without Landlock ABI 6 the contract is a clear exit 2 and no
/// guest run, never an unsandboxed run.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn assert_refused(args: &[&str], stdin: &[u8]) {
    let r = v8vm_bounded(args, stdin, SANDBOX_LIMIT);
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "", "guest must not run");
    assert!(r.stderr.contains("v8vm: --sandbox:"), "stderr: {}", r.stderr);
    assert!(r.stderr.contains("Landlock"), "stderr: {}", r.stderr);
}

/// Runs v8vm under `--sandbox`; None means the kernel cannot sandbox and the
/// refusal has been checked.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn sandboxed(test: &str, args: &[&str], stdin: &[u8]) -> Option<Run> {
    let mut full = vec!["--sandbox"];
    full.extend_from_slice(args);
    if sandbox_available(test) {
        return Some(v8vm_bounded(&full, stdin, SANDBOX_LIMIT));
    }
    assert_refused(&full, stdin);
    None
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod sandbox_run {
    use super::*;

    /// The heap callback and the watchdog thread both run under the
    /// filters: the watchdog is created after `apply` (clone with
    /// CLONE_THREAD, futex, nanosleep), and the heap cap terminates and
    /// still writes the dump through the Landlock-anchored file.
    #[test]
    fn sandbox_enforces_the_heap_cap_and_the_timeout() {
        let grow = script(
            "sb-oom",
            "vm.fs.write('/kept', 'k'); const a = []; for (let i = 0;; i++) a.push({ i });",
        );
        let dump = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sb-oom.json");
        let _ = std::fs::remove_file(&dump);
        let dump_arg = format!("--dump={}", dump.display());
        let args = ["--memory=32", dump_arg.as_str(), grow.to_str().unwrap()];
        let Some(r) = sandboxed("sandbox_enforces_the_heap_cap_and_the_timeout", &args, b"")
        else {
            return;
        };
        assert_eq!(r.code, 137, "stderr: {}", r.stderr);
        assert!(r.stderr.contains("v8vm: heap limit exceeded (32 MB)"), "stderr: {}", r.stderr);
        let json = std::fs::read_to_string(&dump).expect("dump written after the OOM");
        assert!(json.contains("/kept"), "dump: {}", json);

        // Pure JS: only the watchdog thread's own terminate request can stop it.
        let spin = script("sb-timeout", "for (;;) {}");
        let args = ["--timeout=100", spin.to_str().unwrap()];
        let r = sandboxed("sandbox_enforces_the_heap_cap_and_the_timeout", &args, b"")
            .expect("the kernel sandboxed the first run");
        assert_eq!(r.code, 124, "stderr: {}", r.stderr);
    }

    #[test]
    fn sandbox_runs_a_plain_guest_end_to_end() {
        let path = script(
            "sb-hello",
            "const arr = new Array(100000).fill('x'.repeat(50));
             setTimeout(() => console.log('tick ' + arr.length), 5);
             console.log('hello');",
        );
        let Some(r) = sandboxed("sandbox_runs_a_plain_guest_end_to_end", &[path.to_str().unwrap()], b"")
        else {
            return;
        };
        assert_eq!(r.code, 0, "stderr: {}", r.stderr);
        assert_eq!(r.stdout, "hello\ntick 100000\n");
    }

    #[test]
    fn sandbox_timeout_kills_a_spinning_guest_with_124() {
        let path = script("sb-spin", "for (;;) {}");
        let args = ["--timeout=100", path.to_str().unwrap()];
        let Some(r) = sandboxed("sandbox_timeout_kills_a_spinning_guest_with_124", &args, b"") else {
            return;
        };
        assert_eq!(r.code, 124);
    }

    #[test]
    fn sandbox_dump_writes_the_allowed_file() {
        let dump = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sb-dump.json");
        // Start from no file: a leftover snapshot from an earlier run would
        // mask a broken dump write.
        let _ = std::fs::remove_file(&dump);
        let path = script("sb-dump-guest", "vm.fs.write('/g', 'v');");
        let dump_arg = format!("--dump={}", dump.display());
        let args = [dump_arg.as_str(), path.to_str().unwrap()];
        let Some(r) = sandboxed("sandbox_dump_writes_the_allowed_file", &args, b"") else {
            return;
        };
        assert_eq!(r.code, 0, "stderr: {}", r.stderr);
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&dump).expect("dump file")).expect("dump json");
        assert_eq!(
            json,
            serde_json::json!({"dirs": ["/"], "files": {"/g": {"text": "v"}}, "version": 2})
        );
    }

    /// Waits for the child to carry a seccomp filter, so a serve run that
    /// silently skipped the sandbox fails instead of passing on output alone.
    fn wait_for_seccomp(pid: u32) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let status = std::fs::read_to_string(format!("/proc/{}/status", pid)).unwrap_or_default();
            if status.contains("Seccomp:\t2") {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "serve never became sandboxed: {}", status);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn sandbox_serve_answers_commands_over_stdin() {
        let test = "sandbox_serve_answers_commands_over_stdin";
        let path = script("sb-serve", "globalThis.state = { n: 0 };");
        let args = ["--sandbox", "--serve", path.to_str().unwrap()];
        let commands = b"1 + 1\nstate.n += 5\n";
        if !sandbox_available(test) {
            assert_refused(&args, commands);
            return;
        }
        let r = v8vm_bounded_started(&args, commands, SANDBOX_LIMIT, wait_for_seccomp);
        assert_eq!(r.code, 0, "stderr: {}", r.stderr);
        assert_eq!(r.stdout, "2\n5\n");
    }

    /// The web-platform globals work under the sandbox layers too (the
    /// V8VM_REQUIRE_SANDBOX gate runs this test for real).
    #[test]
    fn sandbox_webapi_globals_work() {
        let test = "sandbox_webapi_globals_work";
        let path = script(
            "sb-webapi",
            r#"
            const e = new TextEncoder();
            const d = new TextDecoder();
            console.log(d.decode(e.encode("h\u00e9")), atob(btoa("ok")));
            console.log(structuredClone({ a: [1, 2] }).a.join("+"));
            console.log(typeof performance.now(), performance.now() >= 0);
            let n = 0;
            for (let i = 0; i < 3; i++) n = Math.random();
            console.log(n >= 0 && n < 1);
            "#,
        );
        let Some(r) = sandboxed(test, &[path.to_str().unwrap()], b"") else { return; };
        assert_eq!(r.code, 0, "stderr: {}", r.stderr);
        assert_eq!(r.stdout, "h\u{e9} ok\n1+2\nnumber true\ntrue\n");
    }
}

/// Where the layers cannot be built, `--sandbox` must still be a clear
/// refusal and never an unsandboxed run.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
#[test]
fn sandbox_flag_is_refused_on_unsupported_platforms() {
    let path = script("sb-unsupported", "console.log('ran');");
    let r = v8vm_bounded(&["--sandbox", path.to_str().unwrap()], b"", SANDBOX_LIMIT);
    assert_eq!(r.code, 2);
    assert_eq!(r.stdout, "", "guest must not run");
    assert!(
        r.stderr.contains("--sandbox is only supported on Linux x86_64"),
        "stderr: {}",
        r.stderr
    );
}
