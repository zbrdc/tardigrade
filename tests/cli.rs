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
}

#[test]
fn timeout_kills_a_spinning_guest_with_124() {
    let path = script("spin", "for (;;) {}");
    let r = v8vm(&["--timeout=100", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 124);
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

/// A dump the sandbox cannot anchor is refused while it prepares, before any
/// kernel layer is installed, so these hold on every kernel.
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
        assert_eq!(json, serde_json::json!({"/g": "v"}));
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
