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
    // single-layer design: V8 aborted the process instead. The largest cap
    // is covered too: with too little headroom below V8's limit, the fatal
    // path wins the race even with the GC check in place.
    let path = script("oom-map", "const m = new Map(); for (let i = 0;; i++) m.set(i, i);");
    for cap in ["32", "256"] {
        let r = v8vm(&[&format!("--memory={}", cap), path.to_str().unwrap()], b"");
        assert_eq!(r.code, 137, "cap {}: stderr: {}", cap, r.stderr);
        assert!(
            r.stderr.contains("v8vm: heap limit exceeded"),
            "cap {}: stderr: {}",
            cap,
            r.stderr
        );
    }
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
    // 100k clear calls must not accumulate per-clear bookkeeping, and none
    // of the cleared timers may ever fire.
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
