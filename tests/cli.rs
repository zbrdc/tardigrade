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
        "const CAP = 1048576 - 64;
         vm.fs.write('/f', new Uint8Array(CAP));
         console.log(vm.fs.stat('/f').size);
         try { vm.fs.write('/f', new Uint8Array(CAP + 1)); console.log('no throw'); }
         catch (e) { console.log(e.message, vm.fs.stat('/f').size); }
         try { vm.fs.mkdir('/d', {recursive: true}); } catch (e) { console.log(e.message, vm.fs.exists('/d')); }",
    );
    let r = v8vm(&["--fs-quota=1", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "1048512\nfs quota exceeded 1048512\nfs quota exceeded false\n");
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
