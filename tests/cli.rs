//! End-to-end tests: drive the tardigrade binary the way users and the Makefile do.

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

fn tardigrade(args: &[&str], stdin: &[u8]) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tardigrade"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tardigrade");
    let mut pipe = child.stdin.take().unwrap();
    let input = stdin.to_vec();
    // Feed on a thread: a large input must not deadlock against the child.
    let feeder = std::thread::spawn(move || {
        let _ = pipe.write_all(&input);
    });
    let out = child.wait_with_output().expect("wait tardigrade");
    feeder.join().unwrap();
    Run {
        code: out.status.code().expect("exit code, not a signal"),
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
    }
}

fn run_js(name: &str, source: &str) -> Run {
    let path = script(name, source);
    tardigrade(&[path.to_str().unwrap()], b"")
}

/// run_js with extra CLI flags before the script path.
fn run_js_with(flags: &[&str], name: &str, source: &str) -> Run {
    let path = script(name, source);
    let mut args = flags.to_vec();
    args.push(path.to_str().unwrap());
    tardigrade(&args, b"")
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
fn a_negative_vm_exit_code_still_exits() {
    let r = run_js("exit-negative", "vm.exit(-1); console.log('AFTER')");
    // process::exit truncates the status to 8 bits: -1 reaches the shell as 255.
    assert_eq!(r.code, 255);
    assert_eq!(r.stdout, "", "nothing may run after vm.exit");
}

#[test]
fn the_guest_time_zone_is_utc_even_with_a_host_zone() {
    let path = script("tz-leak", "console.log(new Date(0).getTimezoneOffset())");
    let out = Command::new(env!("CARGO_BIN_EXE_tardigrade"))
        .env("TZ", "America/New_York")
        .arg(&path)
        .output()
        .expect("spawn tardigrade");
    assert!(out.status.success(), "status: {:?}", out.status);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "0\n");
}

#[test]
fn intl_and_locale_calls_throw_instead_of_crashing_the_process() {
    // ICU data is not loaded: these used to abort (133) or segfault (139).
    // They must now be catchable and the run must finish normally.
    let r = run_js(
        "intl-stub",
        r#"
        const attempts = [
          () => new Date(0).toLocaleString(),
          () => new Date(0).toLocaleDateString(),
          () => (1234.5).toLocaleString(),
          () => "a".localeCompare("b"),
          () => new Intl.DateTimeFormat().format(0),
          () => new Intl.NumberFormat().format(1),
          () => [...new Intl.Segmenter().segment("ab")],
        ];
        let caught = 0;
        for (const f of attempts) { try { f(); } catch (e) { caught++; } }
        console.log("caught", caught);
        "#,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "caught 7\n");
}

#[test]
fn vm_exit_zero_with_pending_timers_exits_promptly() {
    // Regression: eval_entry returns 0 for vm.exit(0), and the caller used
    // to read that as "enter the event loop", so the run waited out every
    // pending timer instead of exiting.
    let start = std::time::Instant::now();
    let r = run_js(
        "exit-zero-pending",
        "setTimeout(() => {}, 60000); vm.exit(0);",
    );
    assert_eq!(r.code, 0);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(10),
        "took {:?}",
        start.elapsed()
    );
}

#[test]
fn timer_and_listener_tables_are_bounded() {
    let r = run_js(
        "table-caps",
        r#"
        let timerErr = "";
        try { for (let i = 0; i < 9000; i++) setTimeout(() => {}, 60000); }
        catch (e) { timerErr = e.message; }
        let listenErr = "";
        try { for (let p = 1; p < 2000; p++) vm.net.listen(p, () => {}); }
        catch (e) { listenErr = e.message; }
        console.log(timerErr, "|", listenErr);
        vm.exit(0);
        "#,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "too many live timers | too many net listeners\n");
}

#[test]
fn net_request_reaches_only_the_granted_local_endpoint() {
    use std::io::{Read, Write};
    // One-shot HTTP server on loopback: read the request, reply fixed.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf);
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello")
            .unwrap();
    });
    let grant = format!("--allow-call=local=http://127.0.0.1:{port}/x,POST");
    let r = run_js_with(
        &[grant.as_str(), "--net-allow-private"],
        "net-request",
        r#"
        (async () => {
          const res = await vm.net.request("local", "ping");
          console.log(res.status, res.text, typeof res);
          let denied = "";
          try { await vm.net.request("other", "x"); } catch (e) { denied = String(e && e.message || e); }
          console.log("denied:", denied.includes("no such capability"));
        })();
        "#,
    );
    server.join().unwrap();
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "200 hello object\ndenied: true\n");
}

#[test]
fn net_request_private_endpoint_is_refused_without_the_allow_flag() {
    let grant = "--allow-call=local=http://127.0.0.1:9/x,POST";
    let r = run_js_with(
        &[grant],
        "net-request-private",
        r#"
        (async () => {
          try { await vm.net.request("local", "ping"); console.log("NOT REFUSED"); }
          catch (e) { console.log("refused:", String(e && e.message || e).includes("private")); }
        })();
        "#,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(r.stdout.contains("refused: true"), "stdout: {}", r.stdout);
}

#[test]
fn a_pending_net_request_settles_across_command_boundaries() {
    // docs/SERVING_CONTRACT.md: work a command leaves pending stays part
    // of the session and settles exactly once; the late rejection is not
    // an extra reply and never desynchronises the line protocol.
    use std::io::Read;
    use std::sync::mpsc;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    let _server = std::thread::spawn(move || {
        // Hold the connection open without answering: the sidecar's
        // per-request timeout is what rejects it. Report the connection so
        // the test never blocks on a join if no request is ever made.
        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            match listener.accept() {
                Ok((mut conn, _)) => {
                    let _ = tx.send(());
                    conn.set_nonblocking(false).unwrap();
                    conn.set_read_timeout(Some(std::time::Duration::from_secs(15)))
                        .unwrap();
                    let mut buf = [0u8; 4096];
                    let _ = conn.read(&mut buf);
                    let _ = conn.read(&mut buf); // ends when the sidecar gives up
                    return;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return,
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
    let grant = format!("--allow-call=slow=http://127.0.0.1:{port}/,POST");
    // Serve commands are single expressions; the comma operator starts the
    // request, stores the promise and still replies "go" without awaiting.
    let commands = concat!(
        "(globalThis.p = vm.net.request('slow', 'x')",
        ".then((v) => 'ok', (e) => String(e && e.message || e)), 'go')\n",
        "1\n",
        "p\n"
    );
    let r = serve_with(
        &[grant.as_str(), "--net-allow-private", "--net-timeout=500"],
        "serve-net-late-settle",
        commands,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok(),
        "the request must reach the server"
    );
    let lines: Vec<&str> = r.stdout.lines().collect();
    assert_eq!(lines.len(), 3, "stdout: {}", r.stdout);
    assert_eq!(lines[0], "\"go\"", "first reply: {}", lines[0]);
    assert_eq!(lines[1], "1", "unrelated command: {}", lines[1]);
    // The sidecar's error taxonomy is coarse by design: a timed-out
    // request settles as a "network" transport error, exactly once.
    assert!(
        lines[2].contains("network"),
        "the late rejection must settle exactly once: {}",
        lines[2]
    );
}

#[test]
fn double_dash_ends_option_parsing_for_guest_args() {
    let path = script("dashdash-args", "console.log(JSON.stringify(vm.args));");
    let entry = path.to_str().unwrap();
    let r = tardigrade(&["--", entry, "-x", "--timeout=5", "-"], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "[\"-x\",\"--timeout=5\",\"-\"]\n");
    // Without `--`, a dash argument after the entry is still a usage error.
    let r = tardigrade(&[entry, "-x"], b"");
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
}

fn write_bundle(name: &str, entry: &str, engine: &str) -> std::path::PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let bundle = serde_json::json!({
        "format": "tardigrade.deploy-bundle",
        "version": 1,
        "runtime": {"v8": engine},
        "entry": {
            "path": "app.js",
            "source": entry,
            "sha256": tardigrade_sha256(entry.as_bytes()),
        },
        "budgets": {"heap_mb": 32, "timeout_ms": 5000},
    });
    std::fs::write(&path, serde_json::to_vec(&bundle).unwrap()).unwrap();
    path
}

// The tests cannot reach the crate-private sha helper; ring is a direct
// dependency, so the digest is computed the same documented way.
fn tardigrade_sha256(bytes: &[u8]) -> String {
    use ring::digest::{digest, SHA256};
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn a_validated_bundle_runs_its_entry() {
    let bundle = write_bundle(
        "ok-bundle.json",
        "console.log('from-bundle');",
        &tardigrade::bundle::engine_version(),
    );
    let r = tardigrade(&[&format!("--bundle={}", bundle.to_str().unwrap())], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "from-bundle\n");
}

#[test]
fn a_tampered_bundle_is_refused_before_any_guest() {
    let path = write_bundle(
        "tampered-bundle.json",
        "console.log('x');",
        &tardigrade::bundle::engine_version(),
    );
    let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    v["entry"]["source"] = "console.log('swapped');".into();
    std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
    let r = tardigrade(&[&format!("--bundle={}", path.to_str().unwrap())], b"");
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("mismatch"), "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "", "no guest output may exist after refusal");
}

#[test]
fn a_bundle_for_a_foreign_runtime_is_refused() {
    let path = write_bundle("foreign-bundle.json", "console.log('x');", "0.0.0-other");
    let r = tardigrade(&[&format!("--bundle={}", path.to_str().unwrap())], b"");
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("prepared for V8"), "stderr: {}", r.stderr);
}
#[test]
fn identity_prints_machine_readable_version_and_engine() {
    let r = tardigrade(&["--identity"], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let v: serde_json::Value = serde_json::from_str(r.stdout.trim()).unwrap();
    assert_eq!(
        v["tardigrade"],
        serde_json::json!(env!("CARGO_PKG_VERSION"))
    );
    assert!(!v["v8"].as_str().unwrap().is_empty());
}

#[test]
fn non_utf8_entry_is_exit_2_not_a_panic() {
    let r = tardigrade(&["-"], b"\xff\xfe console.log(1)");
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("not valid UTF-8"), "stderr: {}", r.stderr);
    // From a file, too (previously misreported as "cannot open").
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let path = dir.join("non-utf8-entry.js");
    std::fs::write(&path, b"\xff\xfe").unwrap();
    let r = tardigrade(&[path.to_str().unwrap()], b"");
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("not valid UTF-8"), "stderr: {}", r.stderr);
}

#[test]
fn wasm_and_resizable_memory_are_bounded_by_the_memory_cap() {
    // At --memory=8, a 5000-page (320 MiB) WebAssembly.Memory and a
    // 64 MiB-max resizable ArrayBuffer must refuse with a catchable
    // RangeError, while small allocations still work and grow is bounded.
    let r = run_js_with(
        &["--memory=8"],
        "wasm-rab-cap",
        r#"
        const out = [];
        try { new WebAssembly.Memory({ initial: 5000 }); out.push("wasm:not-refused"); }
        catch (e) { out.push("wasm:" + e.constructor.name); }
        const mem = new WebAssembly.Memory({ initial: 1, maximum: 10 });
        out.push("small:" + mem.buffer.byteLength);
        try { mem.grow(200); out.push("grow:not-refused"); }
        catch (e) { out.push("grow:" + e.constructor.name); }
        try { new ArrayBuffer(1024, { maxByteLength: 64 * 1024 * 1024 }); out.push("rab:not-refused"); }
        catch (e) { out.push("rab:" + e.constructor.name); }
        const rab = new ArrayBuffer(1024, { maxByteLength: 4096 });
        rab.resize(4096);
        out.push("rab-small:" + rab.byteLength);
        out.push("instanceof:" + (mem instanceof WebAssembly.Memory) + "/" + (rab instanceof ArrayBuffer));
        console.log(out.join(" "));
        "#,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout,
        "wasm:RangeError small:65536 grow:RangeError rab:RangeError rab-small:4096 instanceof:true/true\n"
    );
}

#[test]
fn guest_output_volume_is_bounded() {
    // --max-output=1 (MiB): a printing loop must stop at the cap with a
    // catchable RangeError, and total stdout stays near 1 MiB.
    let r = run_js_with(
        &["--max-output=1"],
        "output-cap",
        r#"
        const line = "x".repeat(1023);
        try { for (let i = 0; i < 5000; i++) console.log(line); }
        catch (e) { vm.exit(7); }
        vm.exit(3);
        "#,
    );
    assert_eq!(r.code, 7, "stderr: {}", r.stderr);
    assert!(
        r.stdout.len() <= 1024 * 1024 + 1024,
        "stdout was {} bytes, past the 1 MiB cap + one line",
        r.stdout.len()
    );
    assert!(
        r.stdout.len() > 512 * 1024,
        "cap tripped suspiciously early: {}",
        r.stdout.len()
    );
}

#[test]
fn module_entry_imports_from_the_ram_fs_and_awaits_at_top_level() {
    // Ship /lib.mjs through a preload snapshot; the .mjs entry imports it
    // relatively, uses top-level await, and dynamic-imports it again
    // (same cached instance).
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let snap = dir.join("mod-snap.json");
    std::fs::write(
        &snap,
        r#"{"dirs":["/"],"files":{"/lib.mjs":{"text":"export const answer = 41;"}},"version":2}"#,
    )
    .unwrap();
    let entry_path = dir.join("mod-main.mjs");
    std::fs::write(
        &entry_path,
        "import { answer } from \"./lib.mjs\";\n\
         const again = await import(\"/lib.mjs\");\n\
         console.log(answer + 1, again.answer === answer);\n",
    )
    .unwrap();
    let preload = format!("--preload={}", snap.display());
    let r = tardigrade(&[preload.as_str(), entry_path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "42 true\n");

    // Bare specifiers are refused with a readable error, exit 1.
    std::fs::write(&entry_path, "import \"some-package\";\n").unwrap();
    let r = tardigrade(&[preload.as_str(), entry_path.to_str().unwrap()], b"");
    assert_eq!(r.code, 1, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("bare module specifier"),
        "stderr: {}",
        r.stderr
    );

    // The same source as a classic script (.js, no --module) still fails.
    let classic = script("mod-classic", "import { answer } from \"./lib.mjs\";");
    let r = tardigrade(&[preload.as_str(), classic.to_str().unwrap()], b"");
    assert_eq!(r.code, 1, "stderr: {}", r.stderr);
}

#[test]
fn vm_net_round_trips_do_not_pay_a_poll_per_hop() {
    // Regression for the 50 ms turn poll: 50 request/reply round trips
    // used to take seconds (two hops each, up to ~50 ms a hop). With
    // queued messages spinning the next turn they finish in well under
    // the old floor; the bound below is generous for loaded CI.
    let start = std::time::Instant::now();
    let r = run_js(
        "net-latency",
        r"
        const server = vm.net.port(), client = vm.net.port();
        let n = 0;
        vm.net.listen(server, (p, from) => vm.net.send(server, from, 'echo:' + p));
        vm.net.listen(client, (p) => {
          n++;
          if (n >= 50) { console.log('roundtrips', n); vm.exit(0); }
          vm.net.send(client, server, 'ping' + n);
        });
        vm.net.send(client, server, 'ping0');
        ",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "roundtrips 50\n");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "50 round trips took {:?} (the 50 ms poll is back?)",
        start.elapsed()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn rss_cap_bounds_a_module_internal_wasm_memory() {
    // A memory defined *inside* a WebAssembly module never passes
    // through the JS constructors the prelude charges, so only the
    // process RSS backstop can bound it: 8000 initial pages (512 MiB)
    // against a 256 MB RSS cap must stop as a heap-cap hit, not finish.
    let r = run_js_with(
        &["--memory=256", "--rss-cap=256"],
        "rss-internal-wasm",
        r#"
        // (module (memory (export "m") 8000))
        const bytes = new Uint8Array([
          0x00,0x61,0x73,0x6d, 0x01,0x00,0x00,0x00,
          0x05,0x04,0x01,0x00,0xc0,0x3e,
          0x07,0x05,0x01,0x01,0x6d,0x02,0x00,
        ]);
        const inst = new WebAssembly.Instance(new WebAssembly.Module(bytes));
        const u8 = new Uint8Array(inst.exports.m.buffer);
        for (let i = 0; i < u8.length; i += 4096) u8[i] = 1;
        console.log("touched all pages", u8.length);
        "#,
    );
    assert_eq!(r.code, 137, "stdout: {} stderr: {}", r.stdout, r.stderr);
    assert!(
        !r.stdout.contains("touched all pages"),
        "stdout: {}",
        r.stdout
    );
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
        r.stderr
            .contains("tardigrade: unhandled promise rejection:"),
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
    assert!(
        r.stderr.contains("unhandled promise rejection: 1"),
        "stderr: {}",
        r.stderr
    );
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
    assert!(
        r.stderr.contains("unhandled promise rejection: 1"),
        "stderr: {}",
        r.stderr
    );
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
    assert!(
        r.stderr.contains("unhandled promise rejection: Error: r2"),
        "stderr: {}",
        r.stderr
    );
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
        let r = tardigrade(&[path.to_str().unwrap()], input);
        assert_eq!(r.code, 1, "input {:?}: stderr: {}", input, r.stderr);
        assert!(
            r.stderr
                .contains("unhandled promise rejection: Error: boom"),
            "stderr: {}",
            r.stderr
        );
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
    let r = tardigrade(&["--memory=32", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("heap limit exceeded"),
        "stderr: {}",
        r.stderr
    );
}

#[test]
fn the_watchdog_reaches_a_microtask_that_spins() {
    let path = script(
        "spin-microtask",
        "Promise.resolve().then(() => { for (;;) {} });",
    );
    let start = std::time::Instant::now();
    let r = tardigrade(&["--timeout=300", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 124, "stderr: {}", r.stderr);
    assert!(start.elapsed().as_secs() < 2, "took {:?}", start.elapsed());
}

#[test]
fn the_watchdog_reaches_a_guest_idle_on_a_far_timer() {
    let path = script("idle-timer", "setTimeout(() => {}, 10000);");
    let start = std::time::Instant::now();
    let r = tardigrade(&["--timeout=1000", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 124, "stderr: {}", r.stderr);
    assert!(start.elapsed().as_secs() < 2, "took {:?}", start.elapsed());
}

#[test]
fn the_watchdog_reaches_a_guest_idle_on_stdin() {
    let path = script("idle-stdin", "vm.stdin.read().then(() => {});");
    let mut child = Command::new(env!("CARGO_BIN_EXE_tardigrade"))
        .args(["--timeout=1000", path.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tardigrade");
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
        r.stderr
            .contains("tardigrade: unhandled promise rejection: [object Object]"),
        "stderr: {}",
        r.stderr
    );
}

#[test]
fn async_function_throwing_without_a_catcher_fails() {
    let r = run_js(
        "rej-async",
        "(async () => { throw new Error('async boom'); })();",
    );
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("async boom"), "stderr: {}", r.stderr);
}

#[test]
fn timeout_kills_a_spinning_guest_with_124() {
    let path = script("spin", "for (;;) {}");
    let r = tardigrade(&["--timeout=100", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 124);
}

#[test]
fn memory_zero_is_a_usage_error() {
    let path = script("mem-zero", "console.log('never runs');");
    let r = tardigrade(&["--memory=0", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("--memory"), "stderr: {}", r.stderr);
}

#[test]
fn memory_below_eight_is_a_usage_error() {
    let path = script("mem-four", "console.log('never runs');");
    let r = tardigrade(&["--memory=4", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("at least 8"), "stderr: {}", r.stderr);
}

#[test]
fn map_number_growth_reports_137_not_v8s_fatal_oom() {
    // Table growth must end bounded, never in V8's fatal out-of-memory
    // abort. Two shapes do that: the near-heap-limit callback (137), or -
    // since V8 152 - the engine's own table-size limit, reported as a
    // plain uncaught RangeError (exit 1) before the heap fills.
    let path = script(
        "oom-map",
        "const m = new Map(); for (let i = 0;; i++) m.set(i, i);",
    );
    for cap in ["32", "64"] {
        let r = tardigrade(&[&format!("--memory={}", cap), path.to_str().unwrap()], b"");
        let bounded = r.code == 137 && r.stderr.contains("tardigrade: heap limit exceeded")
            || r.code == 1 && r.stderr.contains("Map maximum size exceeded");
        assert!(bounded, "cap {}: stderr: {}", cap, r.stderr);
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
    let r = tardigrade(&["--memory=32", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("tardigrade: heap limit exceeded (32 MB)"),
        "stderr: {}",
        r.stderr
    );
}

#[test]
fn set_growth_reports_137_at_the_largest_cap() {
    // The deterministic over-cap shape at 256 on V8 13.9 with pointer
    // compression: string Sets now reach V8's structural Set maximum
    // (RangeError, exit 1) before the heap cap, so grow an array of
    // objects instead, which has no structural limit in the way.
    let path = script("oom-set", "const a = []; for (;;) a.push({});");
    let r = tardigrade(&["--memory=256", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("tardigrade: heap limit exceeded"),
        "stderr: {}",
        r.stderr
    );
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
        let r = tardigrade(&[&format!("--memory={}", cap), path.to_str().unwrap()], b"");
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
    let r = tardigrade(&["--memory=32", path.to_str().unwrap()], b"");
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
    let r = tardigrade(&["--memory=8", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "done 100000000\n");
}

#[test]
fn map_of_objects_growth_reports_137() {
    let path = script(
        "oom-map-obj",
        "const m = new Map(); for (let i = 0;; i++) m.set(i, { a: i, b: 'k' + i });",
    );
    let r = tardigrade(&["--memory=32", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
}

#[test]
fn object_key_growth_reports_137() {
    let path = script(
        "oom-objkeys",
        "const o = {}; for (let i = 0;; i++) o['k' + i] = { a: i };",
    );
    let r = tardigrade(&["--memory=32", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
}

#[test]
fn string_concat_reports_the_range_error_not_an_abort() {
    // Cons strings keep the live heap small; the run ends at V8's maximum
    // string length with an ordinary uncaught RangeError (Node does the
    // same), not with the heap cap or a fatal abort.
    let path = script("oom-str", "let s = 'x'; for (;;) s = s + s;");
    let r = tardigrade(&["--memory=32", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 1, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("Invalid string length"),
        "stderr: {}",
        r.stderr
    );
}

#[test]
fn the_smallest_supported_cap_reports_137() {
    let path = script(
        "oom-cap8",
        "const m = new Map(); for (let i = 0;; i++) m.set(i, i);",
    );
    let r = tardigrade(&["--memory=8", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("tardigrade: heap limit exceeded (8 MB)"),
        "stderr: {}",
        r.stderr
    );
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
        let r = tardigrade(&[&format!("--memory={}", cap), path.to_str().unwrap()], b"");
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
    let r = tardigrade(&["--memory=32", path.to_str().unwrap()], b"");
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
    let r = tardigrade(&["--memory=256", path.to_str().unwrap()], b"");
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
        let r = tardigrade(&[&format!("--memory={}", cap), path.to_str().unwrap()], b"");
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
    let r = tardigrade(&["--memory=32", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(r.stdout.contains("RangeError"), "stdout: {}", r.stdout);
    assert!(
        r.stdout.ends_with("later 1048576\n"),
        "stdout: {}",
        r.stdout
    );
}

#[test]
fn a_huge_memory_flag_does_not_wrap() {
    // 16 TiB in MB used to shift-wrap to zero at two call sites; a zero
    // cap would refuse this buffer.
    let path = script(
        "huge-cap",
        "const b = new Uint8Array(1 << 20); b[0] = 1; console.log('ok', b[0]);",
    );
    let r = tardigrade(&["--memory=17592186044416", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "ok 1\n");
}

#[test]
fn a_guest_exit_prefers_code_then_reason_over_exceptions() {
    // A blocked callback returns undefined, the guest throws before V8
    // observes the termination - the exit code / reason still wins.
    let path = script("exit-then-use", "vm.exit(2); vm.fs.read('/x').split(',');");
    let r = tardigrade(&[path.to_str().unwrap()], b"");
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
    assert_eq!(r.stderr, "");
}

#[test]
fn a_timeout_beats_the_exception_from_a_blocked_callback() {
    let path = script(
        "hammer-exit",
        "vm.fs.write('/x', 'abc'); for (;;) { vm.fs.read('/x').length; }",
    );
    let r = tardigrade(&["--timeout=50", path.to_str().unwrap()], b"");
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
    let r = tardigrade(&["--memory=32", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("tardigrade: heap limit exceeded (32 MB)"),
        "stderr: {}",
        r.stderr
    );
    assert!(!r.stderr.contains("TypeError"), "stderr: {}", r.stderr);
}

#[test]
fn vm_exit_beats_the_stdin_continuation() {
    let path = script("exit-stdin", "vm.exit(3); vm.stdin.read().then(() => {});");
    let r = tardigrade(&[path.to_str().unwrap()], b"");
    assert_eq!(r.code, 3, "stderr: {}", r.stderr);
    assert_eq!(r.stderr, "");
}

#[test]
fn a_hundred_mib_arraybuffer_under_the_cap_passes() {
    // External (ArrayBuffer) memory counts toward the cap: 100 MiB is
    // under a 256 MiB cap, so the run must succeed.
    let path = script(
        "one-buffer",
        "new Uint8Array(100 << 20).fill(1); console.log('ok');",
    );
    let r = tardigrade(&["--memory=256", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "ok\n");
}

#[test]
fn retained_arraybuffers_past_the_cap_fail_with_a_range_error() {
    // The counting allocator refuses the allocation that would cross the
    // cap; V8 turns the refusal into a RangeError, which this guest does
    // not catch (a catching guest keeps running - see the fail2 shape).
    let path = script(
        "typed-oom",
        "const a = []; for (;;) a.push(new Uint8Array(1 << 20));",
    );
    let start = std::time::Instant::now();
    let r = tardigrade(&["--memory=256", path.to_str().unwrap()], b"");
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
    let r = tardigrade(
        &[
            &format!("--dump={}", dump.display()),
            path.to_str().unwrap(),
        ],
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
    let r = tardigrade(
        &[
            &format!("--dump={}", dump.display()),
            path.to_str().unwrap(),
        ],
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
    let r = tardigrade(
        &[
            "--memory=32",
            &format!("--dump={}", dump.display()),
            path.to_str().unwrap(),
        ],
        b"",
    );
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("tardigrade: heap limit exceeded"),
        "stderr: {}",
        r.stderr
    );
    assert!(
        !r.stderr.contains("dump"),
        "no dump complaint: {}",
        r.stderr
    );
    let json = std::fs::read_to_string(&dump).expect("dump written after an OOM");
    assert!(
        json.contains("/kept.txt") && json.contains("before the oom"),
        "dump: {}",
        json
    );
}

#[test]
fn heap_cap_oom_exits_137_quickly_without_a_signal() {
    // The run helper unwraps status.code(), so a signal death (V8's own
    // OOM abort, SIGTRAP) fails this test before the code assert runs.
    let path = script(
        "oom",
        "const a = []; for (;;) a.push(new Array(1e5).fill(1));",
    );
    let start = std::time::Instant::now();
    let r = tardigrade(&["--memory=32", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("tardigrade: heap limit exceeded (32 MB)"),
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
    let r = tardigrade(&["--memory=32", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "100\n");
}

#[test]
fn a_small_memory_cap_still_creates_the_isolate() {
    // The initial heap used to be hard-coded at 64 MiB, so any cap below
    // that died with a V8 check failure (SIGTRAP) before the guest ran.
    let path = script("mem-16", "console.log('fits');");
    let r = tardigrade(&["--memory=16", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "fits\n");
}

#[test]
fn usage_errors_exit_2() {
    assert_eq!(tardigrade(&[], b"").code, 2);
    assert_eq!(tardigrade(&["--bogus", "x.js"], b"").code, 2);
    assert_eq!(tardigrade(&["/nonexistent/entry.js"], b"").code, 2);
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
    assert_ne!(
        r.stdout, "n=2\n",
        "the interval must survive the fractional clear"
    );
}

#[test]
fn rejecting_with_a_symbol_still_prints_a_reason() {
    let r = run_js("rej-symbol", "Promise.reject(Symbol('s'))");
    assert_eq!(r.code, 1);
    assert!(
        r.stderr
            .contains("tardigrade: unhandled promise rejection: <unprintable rejection value>"),
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
fn a_net_queue_flood_hits_the_memory_budget_as_a_catchable_range_error() {
    // The audit shape: a tight send loop to a listener that never runs
    // (no loop turn delivers) used to grow the host queue without bound.
    // The queued bytes are now charged to --memory, so the send that
    // would cross the cap throws a RangeError the guest survives.
    let r = run_js_with(
        &["--memory=64"],
        "net-flood-budget",
        "const p = vm.net.port();
         vm.net.listen(p, () => {});
         const msg = 'x'.repeat(64 * 1024);
         let n = 0;
         try { for (;;) { vm.net.send(p, p, msg); n++; } }
         catch (e) { console.log('caught', e.name, n > 0); }
         console.log('survived', n);",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "caught RangeError true\nsurvived 1024\n");
    // 1024 sends x 64 KiB is exactly the 64 MiB cap: the queue stopped at
    // the budget, not at gigabytes (speed is asserted in the next test).
}

#[test]
fn net_queue_flood_stays_bounded_and_fast() {
    // Same flood, bounded sends and a quick exit: the old shape took the
    // process into a hard abort (or a timeout) instead.
    let path = script(
        "net-flood-bounded",
        "const p = vm.net.port();
         vm.net.listen(p, () => {});
         const msg = 'x'.repeat(64 * 1024);
         let n = 0;
         try { for (;;) { vm.net.send(p, p, msg); n++; } } catch (e) {}
         console.log('sends', n);",
    );
    let start = std::time::Instant::now();
    let r = tardigrade(&["--memory=64", path.to_str().unwrap()], b"");
    assert!(start.elapsed().as_secs() < 5, "took {:?}", start.elapsed());
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let sends: usize = r
        .stdout
        .strip_prefix("sends ")
        .and_then(|n| n.trim().parse().ok())
        .expect("send count");
    assert!(sends <= 1100, "{} sends crossed the 64 MiB budget", sends);
}

#[test]
fn delivered_net_messages_return_their_budget_bytes() {
    // Delivery drops the queued Rust copy, so after the loop turns the
    // budget must be free for a second wave of the same size.
    let r = run_js_with(
        &["--memory=64"],
        "net-budget-return",
        "const p = vm.net.port();
         let got = 0;
         vm.net.listen(p, (m) => { got++; });
         const msg = 'x'.repeat(1024 * 1024);
         let n = 0;
         try { for (;;) { vm.net.send(p, p, msg); n++; } } catch (e) {}
         setTimeout(() => {
           console.log('delivered', got === n);
           for (let i = 0; i < 10; i++) vm.net.send(p, p, msg);
           console.log('budget returned');
         }, 0);",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "delivered true\nbudget returned\n");
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
    let r = tardigrade(&["--fs-quota=1", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout,
        "1048510\nfs quota exceeded 1048510\nfs quota exceeded false\n"
    );
}

#[test]
fn fs_quota_zero_disables_the_limit_and_bad_values_exit_2() {
    let path = script(
        "quota-off",
        "vm.fs.write('/f', new Uint8Array(1048576 + 1)); console.log(vm.fs.stat('/f').size);",
    );
    let r = tardigrade(&["--fs-quota=0", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "1048577\n");
    assert_eq!(tardigrade(&["--fs-quota=x", "-"], b"").code, 2);
    assert_eq!(
        tardigrade(&["--fs-quota=18446744073709551616", "-"], b"").code,
        2
    );
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
    let w = tardigrade(
        &[
            &format!("--dump={}", dump.display()),
            writer.to_str().unwrap(),
        ],
        b"",
    );
    assert_eq!(w.code, 0, "stderr: {}", w.stderr);
    let reader = script(
        "bin-reader",
        "const b = vm.fs.readBytes('/img.bin');
         console.log(Array.from(b).join(','), vm.fs.stat('/img.bin').size, vm.fs.exists('/cfg'), vm.fs.read('/cfg/x'), vm.fs.stat('/cfg').type);",
    );
    let r = tardigrade(
        &[
            &format!("--preload={}", dump.display()),
            reader.to_str().unwrap(),
        ],
        b"",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "0,255,254,1,254,255,0 7 true t dir\n");
}

#[test]
fn preload_accepts_the_legacy_snapshot_fixture() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/legacy-snapshot.json"
    );
    let path = script(
        "legacy-reader",
        "console.log(vm.fs.read('/greeting.txt'), vm.fs.exists('/lib'), vm.fs.list('/').join(','));
         vm.run('/lib/util.js');
         console.log(globalThis.seven);",
    );
    let r = tardigrade(
        &[&format!("--preload={}", fixture), path.to_str().unwrap()],
        b"",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout,
        "hello from the old format true /greeting.txt,/lib/util.js\n7\n"
    );
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
    assert_eq!(
        r.stdout,
        "legacy true\nlegacy\ninvalid snapshot: expected an object\n"
    );
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
    let w = tardigrade(
        &[
            &format!("--dump={}", dump.display()),
            writer.to_str().unwrap(),
        ],
        b"",
    );
    assert_eq!(w.code, 0, "stderr: {}", w.stderr);
    let reader = script(
        "fffd-reader",
        "console.log(vm.fs.read('/a\\uFFFDb'), vm.fs.exists('/d\\uFFFD'), vm.fs.read('/d\\uFFFD/f'));\n         console.log(vm.fs.list('/').includes('/a\\uFFFDb'), vm.fs.readdir('/').includes('d\\uFFFD'));\n         console.log(vm.snapshot().includes('a\\uFFFDb'));",
    );
    let r = tardigrade(
        &[
            &format!("--preload={}", dump.display()),
            reader.to_str().unwrap(),
        ],
        b"",
    );
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
    assert_eq!(tardigrade(&["--fs-quota=17592186044416", "-"], b"").code, 2);
    assert_eq!(tardigrade(&["--fs-quota=17592186044415", "-"], b"").code, 0);
}

#[test]
fn preload_accepts_relative_legacy_keys() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/legacy-relative.json"
    );
    let path = script(
        "legacy-relative",
        "console.log(vm.fs.read('/rel/a.txt'), vm.fs.read('/abs.txt'), vm.fs.list('/').join(','));",
    );
    let r = tardigrade(
        &[&format!("--preload={}", fixture), path.to_str().unwrap()],
        b"",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout,
        "relative key ok absolute key ok /abs.txt,/rel/a.txt\n"
    );
}

#[test]
#[ignore] // memory-heavy: run with `make test-heavy`
fn fs_file_too_large_to_read_as_a_string() {
    // Above V8's maximum string length (~536.87 M chars), read must throw
    // a normal guest Error while readBytes still works. Memory-heavy on
    // purpose: nothing smaller can cross the engine's string limit. The
    // write's 540 MB buffer is garbage once vm.fs.write returns, but the
    // ArrayBuffer budget releases its charge only after a GC, so the cap
    // is raised past buffer plus copy at once.
    let path = script(
        "fs-huge-read",
        "vm.fs.write('/f', new Uint8Array(540 * 1024 * 1024));
         try { vm.fs.read('/f'); console.log('no throw'); }
         catch (e) { console.log(e.message); }
         console.log(vm.fs.readBytes('/f').length);",
    );
    let r = tardigrade(
        &["--fs-quota=0", "--memory=1200", path.to_str().unwrap()],
        b"",
    );
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
    let r = tardigrade(&["--fs-quota=1", path.to_str().unwrap()], b"");
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
    let r = tardigrade(&["--fs-quota=1", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout,
        "fs quota exceeded\n200000\nwrite data must be a\n"
    );
}

#[test]
fn a_failed_dump_exits_74_when_the_guest_succeeded() {
    let path = script("dump-fail", "vm.fs.write('/f', 'x');");
    // Writing the dump into a directory fails; the guest itself was fine.
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let r = tardigrade(
        &[&format!("--dump={}", dir.display()), path.to_str().unwrap()],
        b"",
    );
    assert_eq!(r.code, 74);
    assert!(
        r.stderr.contains("cannot write dump"),
        "stderr: {}",
        r.stderr
    );
    // A guest that already failed keeps its own exit code.
    let bad = script("dump-fail-2", "throw new Error('boom');");
    let r2 = tardigrade(
        &[&format!("--dump={}", dir.display()), bad.to_str().unwrap()],
        b"",
    );
    assert_eq!(r2.code, 1);
    // A healthy dump still exits 0.
    let ok_dump = dir.join("dump-ok.json");
    let r3 = tardigrade(
        &[
            &format!("--dump={}", ok_dump.display()),
            path.to_str().unwrap(),
        ],
        b"",
    );
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
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// Runs tardigrade with RLIMIT_FSIZE set to `limit` bytes, so any write that grows
/// a file past it fails partway with EFBIG. SIGXFSZ is ignored (the default
/// action kills the process); the setting survives the exec.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn tardigrade_with_file_size_cap(args: &[&str], limit: u64) -> Run {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tardigrade"));
    cmd.args(args).stdin(Stdio::null());
    // SAFETY: the closure only makes async-signal-safe libc calls.
    unsafe {
        cmd.pre_exec(move || {
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            let cap = libc::rlimit {
                rlim_cur: limit,
                rlim_max: limit,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &cap) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = cmd.output().expect("run tardigrade");
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
    let guest = script(
        "atomic-dump-fail",
        "vm.fs.write('/big', 'x'.repeat(65536));",
    );
    let dump_arg = format!("--dump={}", dump.display());
    let r = tardigrade_with_file_size_cap(&[&dump_arg, guest.to_str().unwrap()], 1024);
    assert_eq!(r.code, 74, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("cannot write dump"),
        "stderr: {}",
        r.stderr
    );
    assert!(
        r.stderr.contains("File too large"),
        "OS error missing: {}",
        r.stderr
    );
    assert_eq!(std::fs::read_to_string(&dump).unwrap(), "OLD-DUMP");
    assert_eq!(entries(&dir), ["ws.json"], "temp file left behind");
}

/// Runs a guest that writes /f and dumps to `dump`, with a time limit so a
/// hang is a failure. The guest script is named after the test.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn dump_run(test: &str, dump: &std::path::Path) -> Run {
    let guest = script(test, "vm.fs.write('/f', 'x');");
    let dump_arg = format!("--dump={}", dump.display());
    tardigrade_bounded(
        &[&dump_arg, guest.to_str().unwrap()],
        b"",
        std::time::Duration::from_secs(60),
    )
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
    assert_eq!(
        std::fs::metadata(&dump).unwrap().permissions().mode() & 0o777,
        0o640
    );
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
    assert_eq!(
        std::fs::metadata(&real).unwrap().ino(),
        inode,
        "target was replaced"
    );
    assert!(
        std::fs::symlink_metadata(&link).unwrap().is_symlink(),
        "link was replaced"
    );
    assert!(std::fs::read_to_string(&real).unwrap().contains("\"/f\""));
    assert_eq!(
        std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert_eq!(
        entries(&dir),
        ["link.json", "real.json"],
        "temp file left behind"
    );
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
    assert!(
        std::fs::symlink_metadata(&link).unwrap().is_symlink(),
        "link was replaced"
    );
    let target = dir.join("target.json");
    assert!(std::fs::read_to_string(&target).unwrap().contains("\"/f\""));
    assert_eq!(
        entries(&dir),
        ["link.json", "target.json"],
        "temp file left behind"
    );
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
    let mut child = Command::new(env!("CARGO_BIN_EXE_tardigrade"))
        .args(["--dump=/dev/stdout", guest.to_str().unwrap()])
        .stdin(Stdio::null())
        .stdout(out_file)
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tardigrade");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll tardigrade") {
            break status;
        }
        if std::time::Instant::now() > deadline {
            child.kill().expect("kill hung tardigrade");
            child.wait().expect("reap tardigrade");
            panic!("tardigrade still running after 60s");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(0));
    assert_eq!(
        std::fs::metadata(&out).unwrap().ino(),
        inode,
        "stdout file was replaced"
    );
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
    assert!(
        r.stderr.contains("cannot write dump"),
        "stderr: {}",
        r.stderr
    );
    assert!(
        r.stderr.contains("Is a directory"),
        "OS error missing: {}",
        r.stderr
    );
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
    assert_eq!(
        std::fs::metadata(&dump).unwrap().permissions().mode() & 0o777,
        0o400
    );
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
    let r = tardigrade(&["--dump=/dev/null", guest.to_str().unwrap()], b"");
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
    let r = tardigrade(&["--fs-quota=1", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let mut lines = r.stdout.lines();
    let full = lines.next().unwrap();
    assert!(
        full.starts_with("quota full after "),
        "stdout: {}",
        r.stdout
    );
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
    Ending {
        what,
        src,
        flags: &[],
        stdin: b"",
        want,
    }
}

/// True when `name` under the scratch dir exists and holds file "/f".
fn dump_has_f(name: &str) -> bool {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    path.exists()
        && std::fs::read_to_string(&path)
            .unwrap_or_default()
            .contains("\"/f\"")
}

#[test]
fn dump_is_written_for_every_ending() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let endings = [
        ending(
            "vm.exit(0) at top level",
            "vm.fs.write('/f','x'); vm.exit(0);",
            0,
        ),
        ending(
            "vm.exit(0) in a timer",
            "vm.fs.write('/f','x'); setTimeout(() => vm.exit(0), 5);",
            0,
        ),
        ending(
            "vm.exit(0) in a microtask",
            "vm.fs.write('/f','x'); Promise.resolve().then(() => vm.exit(0));",
            0,
        ),
        ending("vm.exit(3)", "vm.fs.write('/f','x'); vm.exit(3);", 3),
        ending("vm.exit(-1)", "vm.fs.write('/f','x'); vm.exit(-1);", 255),
        ending(
            "uncaught exception",
            "vm.fs.write('/f','x'); throw new Error('boom');",
            1,
        ),
        ending(
            "unhandled rejection",
            "vm.fs.write('/f','x'); Promise.reject(new Error('no'));",
            1,
        ),
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
        let r = tardigrade_bounded(&args, e.stdin, std::time::Duration::from_secs(60));
        assert_eq!(
            r.code, e.want,
            "{}: exit code (stderr: {})",
            e.what, r.stderr
        );
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
    assert!(
        dump_has_f("ending-sandbox.json"),
        "sandboxed vm.exit(0): dump missing /f"
    );
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
    let w = tardigrade(
        &[
            &format!("--dump={}", dump.display()),
            writer.to_str().unwrap(),
        ],
        b"",
    );
    assert_eq!(w.code, 0);

    let reader = script("preload-reader", "console.log(vm.fs.read('/k'));");
    let r = tardigrade(
        &[
            &format!("--preload={}", dump.display()),
            reader.to_str().unwrap(),
        ],
        b"",
    );
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "vé\n");
}

#[test]
fn missing_preload_exits_2() {
    let path = script("noop", "");
    let r = tardigrade(
        &["--preload=/nonexistent.json", path.to_str().unwrap()],
        b"",
    );
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
    let r = tardigrade(&[path.to_str().unwrap()], payload.as_bytes());
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, format!("{}:same\n", expected));
}

#[test]
fn stdin_from_entry_dash_is_at_eof() {
    let r = tardigrade(
        &["-"],
        b"vm.stdin.read().then((c) => console.log('[' + c + ']'));",
    );
    assert_eq!(r.stdout, "[]\n");
}

/// Each serve test gets its own entry file: they run in parallel and a
/// shared path races writer against reader (an empty read silently skips
/// the entry script).
fn serve(entry: &str, commands: &str) -> Run {
    serve_with(&[], entry, commands)
}

/// serve() with extra CLI flags (before --serve).
fn serve_with(flags: &[&str], entry: &str, commands: &str) -> Run {
    let path = script(entry, "globalThis.state = { n: 0 };");
    let mut args = flags.to_vec();
    args.push("--serve");
    args.push(path.to_str().unwrap());
    tardigrade(&args, commands.as_bytes())
}

#[test]
fn serve_answers_one_json_line_per_command() {
    let r = serve("serve-answers", "1 + 1\nstate.n += 5\n({a: [1, 'x']})\n");
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "2\n5\n{\"a\":[1,\"x\"]}\n");
}

#[test]
fn serve_diverts_guest_console_output_to_stderr() {
    // stdout is the reply channel in serve mode: a guest print must not
    // appear between replies (it would desynchronise consumers).
    let r = serve(
        "serve-console-divert",
        "console.log('guest noise'), 41 + 1\n",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "42\n");
    assert!(r.stderr.contains("guest noise"), "stderr: {}", r.stderr);
}

#[test]
fn serve_accepts_a_trailing_line_comment() {
    let r = serve("serve-comment", "40 + 2 // the answer\n");
    assert_eq!(r.stdout, "42\n");
}

#[test]
fn serve_accepts_a_command_line_at_the_cap() {
    // docs/SERVING_CONTRACT.md: a line of exactly 8 MiB (newline
    // included) is still an ordinary command.
    let mut big = String::with_capacity(8 * 1024 * 1024 + 1);
    big.push_str("40 + 2 //");
    while big.len() < 8 * 1024 * 1024 - 1 {
        big.push('a');
    }
    big.push('\n');
    let r = serve("serve-line-cap-at", &big);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "42\n");
}

#[test]
fn serve_ends_the_session_on_an_oversized_command_line() {
    // docs/SERVING_CONTRACT.md: past the cap the session ends like other
    // malformed input - no execution, no reply, exit 0 - instead of
    // buffering unbounded host memory.
    let mut giant = String::with_capacity(8 * 1024 * 1024 + 2);
    giant.push_str("1 + 1 //");
    while giant.len() < 8 * 1024 * 1024 + 1 {
        giant.push('a');
    }
    giant.push('\n');
    let r = serve("serve-line-cap-over", &giant);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "", "an oversized command must not run");
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
    let r = serve(
        "serve-timers",
        "setTimeout(() => { state.n = 9 }, 5)\nstate.n\n",
    );
    assert_eq!(r.stdout, "1\n9\n"); // first line is the timer id
}

/// Reads one thread's CPU time (utime+stime, in clock ticks) from
/// /proc, or None if the thread is gone. comm is at most 15 characters.
#[cfg(target_os = "linux")]
fn thread_cpu_ticks(pid: u32, comm: &str) -> Option<u64> {
    let tasks = std::fs::read_dir(format!("/proc/{}/task", pid)).ok()?;
    for t in tasks.flatten() {
        let tid = t.file_name().to_string_lossy().to_string();
        let name = std::fs::read_to_string(format!("/proc/{}/task/{}/comm", pid, tid)).ok()?;
        if name.trim() != comm {
            continue;
        }
        let stat = std::fs::read_to_string(format!("/proc/{}/task/{}/stat", pid, tid)).ok()?;
        // Fields after the (comm) pair: state is field 3, so utime and
        // stime (fields 14, 15) are the 12th and 13th tokens (indices
        // 11 and 12).
        let rest = stat.rsplit_once(')')?.1;
        let mut it = rest.split_whitespace();
        it.nth(10)?;
        let utime: u64 = it.next()?.parse().ok()?;
        let stime: u64 = it.next()?.parse().ok()?;
        return Some(utime + stime);
    }
    None
}

/// A session of nothing but near-budget spins: every command times out,
/// so the run watchdog wakes with the CommandTimeout reason again and
/// again while serve recovers. It must yield between wakes, not spin:
/// the measured 2 ms busy-spin per recovery accumulated ~20 ticks in
/// ~1.3 s; the fixed thread stays under a fraction of that. The thread
/// name fits the kernel's 15-character comm limit exactly (see
/// thread_cpu_ticks).
#[cfg(target_os = "linux")]
#[test]
fn the_run_watchdog_does_not_spin_while_serve_recovers_timeouts() {
    let mut commands = String::new();
    for i in 0..150 {
        commands.push_str(&format!(
            "(function(){{var t=performance.now();while(performance.now()-t<10.5);}})()\n'ok{i}'\n"
        ));
    }
    let path = script("serve-wd-spin", "globalThis.state = { n: 0 };");
    let mut child = Command::new(env!("CARGO_BIN_EXE_tardigrade"))
        .args(["--command-timeout=10", "--serve", path.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tardigrade");
    let pid = child.id();
    let mut pipe = child.stdin.take().unwrap();
    let input = commands.into_bytes();
    std::thread::spawn(move || {
        let _ = pipe.write_all(&input);
    });
    let mut out_pipe = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        let mut sink = Vec::new();
        use std::io::Read;
        let _ = out_pipe.read_to_end(&mut sink);
    });
    let start = std::time::Instant::now();
    let ticks = loop {
        assert!(
            start.elapsed() < std::time::Duration::from_secs(30),
            "session never ran"
        );
        if let Some(t) = thread_cpu_ticks(pid, "tardigrade-runw") {
            if start.elapsed() >= std::time::Duration::from_millis(1000) {
                break t;
            }
        }
        if let Some(status) = child.try_wait().expect("poll tardigrade") {
            panic!("session ended before it could be sampled: {}", status);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let _ = child.wait().expect("reap tardigrade");
    assert!(ticks <= 5, "run watchdog used {} CPU ticks in ~1 s", ticks);
}

/// A virtual await settles only at the timer it waits on, even when
/// other timers are queued further out: the loop must not advance the
/// clock on its way out once the promise is settled (a tuple match once
/// evaluated the park decision even for a finished loop, jumping the
/// clock to the next pending timer).
#[test]
fn a_virtual_await_does_not_advance_the_clock_past_its_own_timer() {
    let path = script("serve-virtual-await", "globalThis.state = { n: 0 };");
    let args = ["--clock=virtual", "--serve", path.to_str().unwrap()];
    let r = tardigrade_bounded(
        &args,
        b"(setTimeout(() => {}, 50000), new Promise((r) => setTimeout(() => r(1), 0)))\nperformance.now()\n",
        std::time::Duration::from_secs(10),
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "1\n1\n", "the clock stops at the awaited timer");

    let r = tardigrade_bounded(
        &args,
        b"(setInterval(() => {}, 7), new Promise((r) => setTimeout(() => r(1), 0)))\nperformance.now()\n",
        std::time::Duration::from_secs(10),
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "1\n1\n", "the clock stops at the awaited timer");
}

#[test]
fn serve_under_virtual_time_settles_only_due_work_after_a_reply() {
    // A leftover repeating timer must not spin virtual time (and the
    // CPU) between commands: the post-reply settling runs only work
    // already due, later timers stay queued. Without a command timeout
    // the old behavior never returned to the next command at all.
    let r = tardigrade_bounded(
        &[
            "--clock=virtual",
            "--serve",
            script("serve-virtual-interval", "globalThis.state = { n: 0 };")
                .to_str()
                .unwrap(),
        ],
        b"(setInterval(() => {}, 10), 1)\nperformance.now()\n2\n",
        std::time::Duration::from_secs(10),
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "1\n0\n2\n");
}

#[test]
fn serve_survives_a_command_that_fails_to_compile() {
    let r = serve("serve-syntax", "1 +\n40 + 2\n");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let mut lines = r.stdout.lines();
    let first = lines.next().unwrap();
    assert!(first.contains("$error"), "{}", first);
    assert!(
        first.contains("SyntaxError"),
        "the name is part of the reply: {}",
        first
    );
    assert_eq!(lines.next(), Some("42"), "the session continues");
    assert_eq!(lines.next(), None);
}
#[test]
fn serve_names_thrown_errors_and_thrown_strings() {
    let r = serve("serve-thrown", "null.f()\n(() => { throw 'str' })()\n3\n");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let mut lines = r.stdout.lines();
    let named = lines.next().unwrap();
    assert!(named.starts_with("{\"$error\":\"TypeError: "), "{}", named);
    assert_eq!(lines.next(), Some("{\"$error\":\"str\"}"));
    assert_eq!(lines.next(), Some("3"));
}

#[test]
fn serve_awaits_promises_and_prints_the_settled_value() {
    let r = serve(
        "serve-await",
        "(async () => { await null; return 21 * 2 })()\nPromise.resolve(7)\nnew Promise((r) => setTimeout(() => r('tick'), 20))\n",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "42\n7\n\"tick\"\n");
}

#[test]
fn an_unencodable_promise_value_is_an_error_reply_not_a_session_end() {
    // The ok handler's own failure (BigInt, circular) must be caught and
    // answered, not rejected down the chain as an unhandled rejection.
    let path = script(
        "serve-unencodable",
        "globalThis.o = {}; globalThis.o.self = globalThis.o;",
    );
    let r = tardigrade(
        &["--serve", path.to_str().unwrap()],
        b"Promise.resolve(1n)\n(async () => 1n)()\nPromise.resolve(o)\n(async () => o)()\n5\n",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let mut lines = r.stdout.lines();
    for expected in [
        "TypeError: Do not know how to serialize a BigInt",
        "TypeError: Do not know how to serialize a BigInt",
        "TypeError: Converting circular structure to JSON",
        "TypeError: Converting circular structure to JSON",
    ] {
        let line = lines.next().unwrap();
        assert!(
            line.contains("$error") && line.contains(expected),
            "{}",
            line
        );
    }
    assert_eq!(lines.next(), Some("5"), "the session continues");
    assert_eq!(lines.next(), None);
}

#[test]
fn a_custom_thenable_is_awaited_through_promise_adoption() {
    let r = serve("serve-thenable", "({ then(f) { f('custom') } })\n3\n");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "\"custom\"\n3\n");
}

#[test]
fn serve_replies_error_for_an_async_rejection_and_continues() {
    let r = serve(
        "serve-async-reject",
        "new Promise((_, rej) => setTimeout(() => rej(new Error('later')), 10))\n(async () => { await null; throw 'str' })()\n8\n",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout,
        "{\"$error\":\"later\"}\n{\"$error\":\"str\"}\n8\n"
    );
}

#[test]
fn serve_command_timeout_bounds_a_never_settling_promise() {
    let r = serve_with(
        &["--command-timeout=400"],
        "serve-never",
        "new Promise(() => {})\n1 + 1\n",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "{\"$error\":\"command timed out\"}\n2\n");
}

#[test]
fn a_shorter_run_timeout_fires_during_the_command_timeout_wait() {
    // The idle wait for the command deadline must sit on the termination
    // condvar, not sleep the whole budget: --timeout has to land on time.
    // The 3 s bound fails the test outright if the wait overshoots.
    let path = script("serve-two-clocks", "globalThis.state = { n: 0 };");
    let r = tardigrade_bounded(
        &[
            "--timeout=500",
            "--command-timeout=4000",
            "--serve",
            path.to_str().unwrap(),
        ],
        b"new Promise(() => {})\n",
        std::time::Duration::from_secs(3),
    );
    assert_eq!(r.code, 124, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "", "the run ended before any reply was owed");
}

#[test]
fn the_command_timeout_bounds_the_post_reply_settling() {
    // A spinning timer scheduled by the command spins the boundary loop
    // after the reply: the deadline must stop it, say so on stderr once,
    // and leave the session usable.
    let r = serve_with(
        &["--command-timeout=400"],
        "serve-post-spin",
        "setTimeout(() => { for(;;){} }, 0), 1\n2\n",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "1\n2\n");
    assert_eq!(
        r.stderr.matches("timed out after its reply").count(),
        1,
        "stderr: {}",
        r.stderr
    );
}

#[test]
fn the_command_timeout_bounds_a_repeating_timer() {
    // A repeating timer keeps the boundary loop alive forever (without a
    // deadline the session never returns to its commands - pre-existing);
    // with one, each command's settling ends at its deadline.
    let r = serve_with(
        &["--command-timeout=400"],
        "serve-interval",
        "setInterval(() => {}, 10), 1\n2\n",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "1\n2\n");
    assert!(
        r.stderr.contains("timed out after its reply"),
        "stderr: {}",
        r.stderr
    );
}

#[test]
fn vm_exit_at_the_deadline_wins_or_never_happened() {
    // Spin right at the budget, then vm.exit(9): either vm.exit ran and
    // the session ends with 9 and no reply, or the deadline killed the
    // spin before vm.exit ran and the session answers and continues.
    // The bug signature - exit 9 together with replies - must not occur.
    let path = script("serve-exit-race", "globalThis.state = { n: 0 };");
    for i in 0..10 {
        let spin = [1.7, 1.85, 1.95, 2.0, 2.05][i % 5];
        let cmd = format!(
            "(function(){{var t=performance.now();while(performance.now()-t<{spin});vm.exit(9)}})()\n'x'\n"
        );
        let r = tardigrade_bounded(
            &["--command-timeout=2", "--serve", path.to_str().unwrap()],
            cmd.as_bytes(),
            std::time::Duration::from_secs(20),
        );
        let exited = r.code == 9;
        assert_eq!(
            exited,
            r.stdout.is_empty(),
            "session {}: code {} stdout {:?}",
            i,
            r.code,
            r.stdout
        );
        if !exited {
            assert_eq!(r.code, 0, "session {}: stderr {}", i, r.stderr);
        }
    }
}

#[test]
fn vm_exit_in_a_leftover_timer_ends_the_session_after_the_reply() {
    let r = serve_with(
        &["--command-timeout=500"],
        "serve-exit-timer",
        "setTimeout(() => vm.exit(4), 10), 1\n",
    );
    assert_eq!(r.code, 4, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "1\n", "the reply goes out before the exit lands");
}

#[test]
fn vm_exit_wins_when_the_command_timeout_fires_on_top_of_it() {
    // The timer sets the exit code and then spins: the command timeout
    // that stops the spin must not swallow the exit - the session ends
    // with 4, no later command runs, no timeout is reported.
    let r = serve_with(
        &["--command-timeout=400"],
        "serve-exit-over-timeout",
        "setTimeout(() => { vm.exit(4); for (;;) {} }, 10), 1\n'x'\n",
    );
    assert_eq!(r.code, 4, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "1\n");
    assert!(!r.stderr.contains("timed out"), "stderr: {}", r.stderr);
}

#[test]
fn an_await_is_not_held_hostage_by_other_pending_work() {
    // A live repeating timer must not hold the reply until the budget:
    // the loop runs turn by turn and stops when the promise settles.
    // (Each later boundary still spends its own budget on the interval,
    // hence the stderr lines and the generous bound.)
    let r = tardigrade_bounded(
        &[
            "--command-timeout=400",
            "--serve",
            script("serve-await-turns", "globalThis.state = { n: 0 };")
                .to_str()
                .unwrap(),
        ],
        b"(setInterval(() => {}, 10), new Promise((r) => setTimeout(() => r(1), 50)))\n2\n",
        std::time::Duration::from_secs(5),
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout, "1\n2\n",
        "the reply is the settled value, not a timeout"
    );
    assert!(
        r.stderr.contains("timed out after its reply"),
        "stderr: {}",
        r.stderr
    );
}

/// Spawns a serve session, feeds `commands`, and reads the first reply
/// line within `limit` (None: none came). The child is killed by pid:
/// leftover repeating work can hold the session long past its reply.
fn serve_first_reply(args: &[&str], commands: &[u8], limit: std::time::Duration) -> Option<String> {
    use std::io::BufRead;
    let mut child = Command::new(env!("CARGO_BIN_EXE_tardigrade"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tardigrade");
    let mut pipe = child.stdin.take().unwrap();
    let input = commands.to_vec();
    std::thread::spawn(move || {
        let _ = pipe.write_all(&input);
    });
    let out = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let got = std::io::BufReader::new(out)
            .read_line(&mut line)
            .unwrap_or(0);
        let _ = tx.send((got > 0).then(|| line.trim_end().to_string()));
    });
    let reply = rx.recv_timeout(limit).ok().flatten();
    child.kill().expect("kill serve child");
    let _ = child.wait();
    reply
}

#[test]
fn an_await_answers_promptly_without_a_command_timeout() {
    // Without --command-timeout the old code waited for the loop to go
    // idle; a live interval meant no reply ever. The reply must arrive
    // while the interval still runs (the session itself is then killed by
    // pid: that interval holds its boundary forever - pre-existing).
    let path = script("serve-await-noflag", "globalThis.state = { n: 0 };");
    let reply = serve_first_reply(
        &["--serve", path.to_str().unwrap()],
        b"(setInterval(() => {}, 10), new Promise((r) => setTimeout(() => r(1), 50)))\n",
        std::time::Duration::from_secs(5),
    );
    assert_eq!(reply.as_deref(), Some("1"));
}

#[test]
fn a_hijacked_then_cannot_break_the_one_line_protocol() {
    // A guest-replaced Promise.prototype.then can hand back arbitrary
    // text; the reply contract (one JSON line) wins over it.
    let r = serve(
        "serve-hijacked-then",
        "Promise.prototype.then = function () { return 'multi\\nline'; }, Promise.resolve(1)\n5\n",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "{\"$error\":\"<encoding failed>\"}\n5\n");
}

#[test]
fn vm_exit_wins_over_the_command_timeout() {
    let r = serve_with(
        &["--command-timeout=100000"],
        "serve-exit-cmd",
        "vm.exit(7)\n",
    );
    assert_eq!(r.code, 7, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "", "the session ends, no reply is owed");
}

#[test]
fn the_heap_cap_wins_over_the_command_timeout() {
    let path = script("serve-oom-cmd", "");
    let r = tardigrade_bounded(
        &[
            "--memory=32",
            "--command-timeout=60000",
            "--serve",
            path.to_str().unwrap(),
        ],
        b"(() => { const a = []; for (;;) a.push(new Array(1e5).fill(1)); })()\n",
        std::time::Duration::from_secs(60),
    );
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("heap limit exceeded"),
        "stderr: {}",
        r.stderr
    );
}

#[test]
fn a_run_level_timeout_ends_a_serve_session_with_124() {
    let path = script("serve-run-timeout", "globalThis.state = { n: 0 };");
    let r = tardigrade_bounded(
        &[
            "--timeout=300",
            "--command-timeout=5000",
            "--serve",
            path.to_str().unwrap(),
        ],
        b"(function () { for (;;) {} })()\n",
        std::time::Duration::from_secs(10),
    );
    assert_eq!(r.code, 124, "stderr: {}", r.stderr);
}

#[test]
fn serve_command_timeout_breaks_an_infinite_loop_and_state_survives() {
    let r = serve_with(
        &["--command-timeout=400"],
        "serve-spin-cmd",
        "state.n = 5\n(function () { for (;;) {} })()\nstate.n\n",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "5\n{\"$error\":\"command timed out\"}\n5\n");
}

#[test]
fn serve_command_timeout_breaks_a_spinning_await_and_state_survives() {
    // The spin sits inside a promise continuation: the timeout must stop
    // the event loop's turn, not only the command's own script.
    let r = serve_with(
        &["--command-timeout=400"],
        "serve-spin-await",
        "state.n = 6\nnew Promise((r) => setTimeout(() => { for(;;){} r(1) }, 10))\nstate.n\n",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "6\n{\"$error\":\"command timed out\"}\n6\n");
}

#[test]
fn a_command_finishing_at_its_deadline_does_not_poison_the_next() {
    // Interleaved: each spin runs right at the 10 ms budget, so the
    // watchdog fires just as the command finishes, and the trivial
    // command after it must still answer exactly. A leftover or stray
    // terminate shows up as a false "timed out" on a trivial command, a
    // dropped reply, or a killed session. The generous budget keeps
    // honest wall-clock expiries rare (a stalled host can still cause a
    // few), so no retry is allowed and at most 2 trivial replies may be
    // the timeout error; everything else must be exact.
    let mut commands = String::new();
    for i in 0..1000 {
        let spin = [9.0, 9.5, 10.0, 10.5, 11.0][i % 5];
        commands.push_str(&format!(
            "(function(){{var t=performance.now();while(performance.now()-t<{spin});}})()\n'ok{i}'\n"
        ));
    }
    let path = script("serve-edge", "globalThis.state = { n: 0 };");
    let r = tardigrade_bounded(
        &["--command-timeout=10", "--serve", path.to_str().unwrap()],
        commands.as_bytes(),
        std::time::Duration::from_secs(60),
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let lines: Vec<&str> = r.stdout.lines().collect();
    assert_eq!(lines.len(), 2000, "every command answered, none lost");
    let mut false_timeouts = 0;
    for i in (0..2000).step_by(2) {
        assert!(
            lines[i] == "null" || lines[i].contains("$error"),
            "spin {}: {}",
            i,
            lines[i]
        );
        if lines[i + 1] == "{\"$error\":\"command timed out\"}" {
            false_timeouts += 1;
        } else {
            assert_eq!(
                lines[i + 1],
                format!("\"ok{}\"", i / 2),
                "trivial command corrupted"
            );
        }
    }
    assert!(
        false_timeouts <= 2,
        "{} false timeouts on trivial commands",
        false_timeouts
    );
}

#[test]
fn serve_accepts_crlf_lines() {
    let r = serve("serve-crlf", "1 + 1\r\n2 + 2\r\n");
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "2\n4\n");
}

#[test]
fn serve_skips_empty_lines_between_commands() {
    let r = serve("serve-empty", "1\n\n2\n");
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "1\n2\n");
}

#[test]
fn an_invalid_utf8_line_ends_the_session_with_exit_0() {
    let path = script("serve-bad-utf8", "globalThis.state = { n: 0 };");
    let r = tardigrade(
        &["--serve", path.to_str().unwrap()],
        b"1 + 1\n\xff\xfe\n2\n",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout, "2\n",
        "the bad line and what follows are not replies"
    );
}

#[test]
fn a_bad_command_timeout_flag_is_a_usage_error() {
    let path = script("serve-bad-flag", "globalThis.state = { n: 0 };");
    let r = tardigrade(
        &["--command-timeout=soon", "--serve", path.to_str().unwrap()],
        b"",
    );
    assert_eq!(r.code, 2);
    assert!(
        r.stderr.contains("bad --command-timeout"),
        "stderr: {}",
        r.stderr
    );
}

#[test]
fn an_unhandled_rejection_inside_a_serve_command_fails_the_session() {
    // The command's own value is fine; the rejection belongs to another
    // promise nobody handles, so the run's boundary decides as usual.
    let path = script("serve-reject", "globalThis.p = null;");
    let r = tardigrade(
        &["--serve", path.to_str().unwrap()],
        b"(void Promise.reject('sv'), 42)\n99\n",
    );
    assert_eq!(r.code, 1, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout, "42\n",
        "the reply goes out before the boundary decides"
    );
    assert!(
        r.stderr
            .contains("tardigrade: unhandled promise rejection: sv"),
        "stderr: {}",
        r.stderr
    );
}

#[test]
fn a_rejection_of_the_command_value_is_an_error_reply_and_the_session_continues() {
    let r = serve(
        "serve-return-reject",
        "Promise.reject('sv')\n(async () => { throw new Error('fast') })()\n7\n",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "{\"$error\":\"sv\"}\n{\"$error\":\"fast\"}\n7\n");
}

#[test]
fn serve_answers_object_literal_commands_as_one_expression() {
    let r = serve("serve-object", "{a: 1}\n{\"a\": 1}\n");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "{\"a\":1}\n{\"a\":1}\n");
}

#[test]
fn a_statement_command_is_a_syntax_error_reply() {
    let r = serve("serve-statement", "let x = 1\n40 + 2\n");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let mut lines = r.stdout.lines();
    let first = lines.next().unwrap();
    assert!(first.contains("SyntaxError"), "{}", first);
    assert_eq!(lines.next(), Some("42"), "the session continues");
}

#[test]
fn a_replaced_eval_does_not_reach_later_commands() {
    let r = serve("serve-eval-hijack", "globalThis.eval = 42\n1 + 1\n");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "42\n2\n");
}

#[test]
fn serve_leaves_no_host_globals_behind() {
    // The check runs as a second command: a scratch global left by the
    // first would be visible by then.
    let r = serve(
        "serve-no-scratch",
        "1 + 1\nObject.keys(globalThis).filter((k) => k.indexOf('__tardigrade') === 0)\n",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "2\n[]\n");
}

#[test]
fn an_oom_inside_a_serve_expression_reports_137() {
    let path = script("serve-oom", "");
    let r = tardigrade(
        &["--memory=32", "--serve", path.to_str().unwrap()],
        b"(() => { const a = []; for (;;) a.push(new Array(1e5).fill(1)); })()\n",
    );
    assert_eq!(r.code, 137, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("heap limit exceeded"),
        "stderr: {}",
        r.stderr
    );
}

#[test]
fn serve_replies_reach_stdout_before_later_guest_output() {
    // Updated for the serve-channel split: guest console output is
    // diverted to stderr, so stdout carries exactly the replies, in
    // order, and the guest prints land on stderr. (This test used to
    // assert the interleaved single-stream order; that contract is what
    // desynchronised serve consumers and is gone on purpose.)
    let path = script("serve-order", "globalThis.n = 0;");
    // One expression per command: the comma operator schedules work and
    // yields the reply value.
    let commands = b"(vm.net.listen(1, () => console.log('n1')), vm.net.send(1, 1, 'x'), 'A')\n(vm.net.listen(2, () => console.log('n2')), vm.net.send(2, 2, 'x'), 'B')\nsetTimeout(() => console.log('t'), 0), 'C'\n";
    let mut violations = 0;
    for _ in 0..50 {
        let r = tardigrade(&["--serve", path.to_str().unwrap()], commands);
        assert_eq!(r.code, 0, "stderr: {}", r.stderr);
        if r.stdout != "\"A\"\n\"B\"\n\"C\"\n" || r.stderr != "n1\nn2\nt\n" {
            violations += 1;
        }
    }
    assert_eq!(
        violations, 0,
        "reply ordering violated {} of 50 times",
        violations
    );
}

/// Like `tardigrade`, but kills the child and fails if it outlives `limit`, so a
/// hang regression is a test failure instead of a stuck test run.
fn tardigrade_bounded(args: &[&str], stdin: &[u8], limit: std::time::Duration) -> Run {
    tardigrade_bounded_started(args, stdin, limit, |_| {})
}

/// `tardigrade_bounded` that also calls `started` with the child's pid right after
/// the spawn, before any input is sent.
fn tardigrade_bounded_started(
    args: &[&str],
    stdin: &[u8],
    limit: std::time::Duration,
    started: impl FnOnce(u32),
) -> Run {
    use std::io::Read;
    let mut child = Command::new(env!("CARGO_BIN_EXE_tardigrade"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tardigrade");
    started(child.id());
    let mut pipe = child.stdin.take().unwrap();
    let input = stdin.to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = pipe.write_all(&input);
        // pipe drops here: EOF without any trailing newline from us
    });
    // Drain both pipes while waiting: a long session outproduces the pipe
    // capacity and would otherwise block on its own stdout forever.
    let mut out_pipe = child.stdout.take().unwrap();
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let mut err_pipe = child.stderr.take().unwrap();
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf
    });
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll tardigrade") {
            break status;
        }
        if start.elapsed() > limit {
            child.kill().expect("kill hung tardigrade");
            child.wait().expect("reap tardigrade");
            panic!("tardigrade still running after {:?}", limit);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    feeder.join().unwrap();
    let stdout = String::from_utf8(out_reader.join().expect("stdout reader")).unwrap();
    let stderr = String::from_utf8(err_reader.join().expect("stderr reader")).unwrap();
    Run {
        code: status.code().expect("exit code, not a signal"),
        stdout,
        stderr,
    }
}

#[test]
fn serve_answers_a_final_command_without_a_trailing_newline_and_exits() {
    let path = script("serve-no-newline", "globalThis.s = 1;");
    let r = tardigrade_bounded(
        &["--serve", path.to_str().unwrap()],
        b"1+1",
        std::time::Duration::from_secs(10),
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "2\n");
}

// ---- --clock=virtual ----

/// run_js_with plus a wall-time bound on the whole run, so a virtual-time
/// regression that waits in real time fails the test instead of slowing it.
fn run_js_timed(flags: &[&str], name: &str, source: &str) -> (Run, std::time::Duration) {
    let start = std::time::Instant::now();
    let r = run_js_with(flags, name, source);
    (r, start.elapsed())
}

#[test]
fn virtual_time_fires_a_10s_timer_instantly_and_exactly() {
    let (r, took) = run_js_timed(
        &["--clock=virtual", "--timeout=5000"],
        "vclock-10s",
        "const t = Date.now();\n         setTimeout(() => console.log('fired', Date.now() - t), 10000);",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "fired 10000\n");
    assert!(took < std::time::Duration::from_secs(1), "took {:?}", took);
}

#[test]
fn virtual_time_interval_ticks_advance_exactly() {
    let (r, took) = run_js_timed(
        &["--clock=virtual", "--timeout=5000"],
        "vclock-interval",
        "const t = Date.now(); let n = 0;\n         const iv = setInterval(() => {\n           n++; console.log('tick', n, Date.now() - t);\n           if (n === 3) clearInterval(iv);\n         }, 250);",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "tick 1 250\ntick 2 500\ntick 3 750\n");
    assert!(took < std::time::Duration::from_secs(1), "took {:?}", took);
}

#[test]
fn virtual_time_orders_nested_and_interleaved_timers() {
    let r = run_js_with(
        &["--clock=virtual", "--timeout=5000"],
        "vclock-order",
        "setTimeout(() => { console.log('a', Date.now());\n                            setTimeout(() => console.log('a2', Date.now()), 10); }, 100);\n         setTimeout(() => console.log('b', Date.now()), 50);",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "b 50\na 100\na2 110\n");
}

#[test]
fn virtual_time_performance_now_is_the_elapsed_virtual_ms() {
    let r = run_js_with(
        &["--clock=virtual", "--epoch=1000", "--timeout=5000"],
        "vclock-perf",
        "console.log(performance.now(), Date.now());\n         setTimeout(() => console.log(performance.now(), Date.now()), 10000);",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "0 1000\n10000 11000\n");
}

#[test]
fn virtual_time_epoch_is_honored_by_every_date_read() {
    // 1234567890123 ms is 2009-02-13 UTC: far from the real clock, so a
    // host-clock leak through any Date form shows up as a wrong value.
    let r = run_js_with(
        &["--clock=virtual", "--epoch=1234567890123", "--timeout=5000"],
        "vclock-epoch",
        "const E = 1234567890123;\n         console.log(Date.now() === E, new Date().getTime() === E);\n         console.log(new Date().getUTCFullYear(), new Date(12345).getTime() === 12345);\n         console.log(Date() === new Date().toString(), typeof Date());\n         setTimeout(() => console.log('t', Date.now() - E), 500);",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "true true\n2009 true\ntrue string\nt 500\n");
}

#[test]
fn virtual_date_is_rebuilt_over_the_original() {
    // The override must not hand the guest the original constructor (the
    // host clock) through any of the usual object-graph paths - the
    // prototype chain of Date itself included - and it must keep the
    // Date surface (function form, subclassing, statics, constructor
    // shape) looking like the platform's.
    let r = run_js_with(
        &["--clock=virtual", "--epoch=77", "--timeout=5000"],
        "vclock-date-shape",
        "const E = 77;\n         console.log(new Date().constructor === Date, Date.name, Date.length);\n         console.log(Object.getPrototypeOf(new Date()) === Date.prototype);\n         console.log(typeof globalThis.__virtualDateMs);\n         class D extends Date {}\n         console.log(new D(5).getTime() === 5, new D().getTime() === E, D.now() === E);\n         console.log(Date.UTC(1970, 0, 1) === 0, typeof Date.parse);\n         console.log(Date(999) === Date(), new Date().getUTCFullYear());\n         // Date's own [[Prototype]] is Function.prototype in every\n         // spelling, so nothing through it constructs with the host clock.\n         console.log(Object.getPrototypeOf(Date) === Function.prototype,\n                     Date.__proto__ === Function.prototype,\n                     Reflect.getPrototypeOf(Date) === Function.prototype,\n                     Object.getPrototypeOf(D) === Date);\n         let protoCtor = 'constructed';\n         try { new (Object.getPrototypeOf(Date))(); } catch (e) { protoCtor = e.name; }\n         console.log(protoCtor, new (Date.bind())().getTime() === E);\n         console.log(Object.keys(Date).length === 0,\n                     Object.getOwnPropertyNames(Date).sort().join(','),\n                     Object.getOwnPropertyDescriptor(Date, 'prototype').writable,\n                     Date.now.name,\n                     (() => { try { new Date.now(); return 'constructed'; } catch (e) { return e.name; } })());",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout,
        "true Date 7\ntrue\nundefined\ntrue true true\ntrue function\ntrue 1970\n".to_string()
            + "true true true true\nTypeError true\ntrue UTC,length,name,now,parse,prototype false now TypeError\n"
    );
}

#[test]
fn every_intrinsic_the_date_override_uses_survives_being_replaced() {
    // The decoder probe shape, applied to Date: swap each intrinsic the
    // prelude's override touches for a throwing boom, run the whole Date
    // surface, restore, and require the same results - the override
    // captured Reflect.construct/apply (and everything else it uses) at
    // prelude time, so a guest cannot reroute or trap it, least of all to
    // capture the host constructor.
    let r = run_js_with(
        &["--clock=virtual", "--epoch=77", "--timeout=5000"],
        "vclock-date-intrinsics",
        r#"
        const DP = Object.defineProperty, GOPD = Object.getOwnPropertyDescriptor;
        const boom = function () { throw new Error("HIJACKED"); };
        const E = 77;
        const run = () => {
          // String accumulation, not an array: under the index accessors
          // below, pushing into a fresh (holey) array runs their setters,
          // and the harness must observe the override, not itself.
          let out = "";
          const t = (n, f) => {
            try { out += n + "=" + String(f()) + " "; }
            catch (x) { out += n + "!" + (x && x.name) + " "; }
          };
          t("now", () => Date.now() === E);
          t("new", () => new Date().getTime() === E);
          t("arg", () => new Date(5).getTime() === 5);
          t("multi", () => new Date(2000, 5, 7).getUTCFullYear() === 2000);
          t("coerce", () => new Date({ valueOf: () => 5 }).getTime() === 5);
          t("call", () => Date() === Date());
          t("subclass", () => new (class extends Date {})().getTime() === E);
          t("utc", () => Date.UTC(1970, 0, 1) === 0);
          t("parse", () => typeof Date.parse("2000-01-01T00:00:00Z"));
          return out.trim();
        };
        const base = run();
        if (base.includes("HIJACKED")) { console.log("baseline broken:", base); vm.exit(1); }
        const targets = [
          [globalThis, "Reflect"], [globalThis, "Array"],
          [Reflect, "construct"], [Reflect, "apply"],
          [Function.prototype, "call"], [Function.prototype, "apply"],
          [Date.prototype, "toString"], [Date.prototype, Symbol.toPrimitive],
        ];
        for (const [o, k] of targets) {
          const desc = GOPD(o, k);
          DP(o, k, { value: boom, configurable: true, writable: true });
          const now = run();
          if (desc) DP(o, k, desc); else delete o[k];
          if (now !== base) { console.log("DIFFERS under", String(k), ":", now, "|", base); vm.exit(1); }
        }
        // Round-2 probe shape: index accessors on the prototype chains.
        // The override used to copy its arguments through a fresh array,
        // and every copied index ran these traps (new Date(2000, 5, 7)
        // built year 99999). The strict arguments object and the array
        // literal both carry own data indices, so nothing may touch them.
        let trapHits = 0;
        const saved = [];
        for (const proto of [Array.prototype, Object.prototype]) {
          for (const idx of [0, 1, 2]) {
            saved.push([proto, idx, GOPD(proto, idx)]);
            DP(proto, idx, {
              get() { trapHits++; return 99999; },
              set() { trapHits++; },
              configurable: true,
            });
          }
        }
        const trapped = run();
        for (const [proto, idx, desc] of saved) {
          if (desc) DP(proto, idx, desc); else delete proto[idx];
        }
        if (trapHits !== 0 || trapped !== base) {
          console.log("TRAPPED", trapHits, "times:", trapped, "|", base);
          vm.exit(1);
        }
        console.log("stable", base);
        "#,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout,
        "stable now=true new=true arg=true multi=true coerce=true call=true subclass=true utc=true parse=number\n"
    );
}

#[test]
fn virtual_time_is_fully_deterministic_with_a_seed() {
    // Math.random (--seed), Date.now/performance.now and the timer
    // sequence all feed the output; two runs must match byte for byte.
    let src = "console.log('r', Math.random(), Math.random());\n              console.log('d', Date.now(), performance.now());\n              setTimeout(() => console.log('a', Math.random(), Date.now()), 100);\n              setTimeout(() => console.log('b', Math.random(), Date.now()), 50);\n              const iv = setInterval(() => console.log('i', Math.random(), Date.now()), 30);\n              setTimeout(() => { clearInterval(iv); vm.exit(0); }, 120);";
    let flags = [
        "--clock=virtual",
        "--epoch=99",
        "--seed=7",
        "--timeout=5000",
    ];
    let a = run_js_with(&flags, "vclock-det-a", src);
    let b = run_js_with(&flags, "vclock-det-b", src);
    assert_eq!(a.code, 0, "stderr: {}", a.stderr);
    assert_eq!(b.code, 0, "stderr: {}", b.stderr);
    assert_eq!(a.stdout, b.stdout, "runs must be identical");
    assert!(a.stdout.contains("d 99 0"));
}

#[test]
fn virtual_time_timeout_still_kills_a_busy_loop_with_124() {
    // The watchdog measures real time: a guest that busy-loops under
    // virtual time is stopped exactly like a real-time one.
    let path = script("vclock-spin", "for (;;) {}");
    let r = tardigrade_bounded(
        &["--clock=virtual", "--timeout=150", path.to_str().unwrap()],
        b"",
        std::time::Duration::from_secs(10),
    );
    assert_eq!(r.code, 124);
}

#[test]
fn virtual_time_does_not_wait_for_a_pending_stdin_read() {
    // The clock jumps while a stdin read is pending: the read stays
    // pending (nothing real has arrived), the timer fires at its virtual
    // deadline anyway, and the run ends without waiting for stdin. stdin
    // stays open the whole time, so the read truly never resolves.
    let path = script(
        "vclock-stdin",
        "vm.stdin.read().then((c) => console.log('stdin', JSON.stringify(c)));\n         setTimeout(() => { console.log('timer', Date.now()); vm.exit(0); }, 1000);",
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_tardigrade"))
        .args(["--clock=virtual", "--timeout=5000", path.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tardigrade");
    let hold = child.stdin.take().unwrap(); // held open: no data, no EOF
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send(child.wait_with_output()).unwrap());
    let out = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("guest finished without waiting for stdin")
        .expect("wait tardigrade");
    drop(hold);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "timer 1000\n");
}

#[test]
fn virtual_time_net_delivery_does_not_advance_the_clock() {
    // Net delivery is real and unchanged: messages are delivered on the
    // next loop turn without moving the virtual clock; only timers do.
    // The second send is queued from inside a timer callback, so it is
    // pending at the loop's turn boundary - exactly where a virtual-time
    // implementation would be tempted to advance the clock.
    let r = run_js_with(
        &["--clock=virtual", "--timeout=5000"],
        "vclock-net",
        "const p = vm.net.port();\n         vm.net.listen(p, (m) => {\n           console.log('net', Date.now(), m);\n           if (m === 'x') setTimeout(() => vm.net.send(p, p, 'y'), 100);\n         });\n         vm.net.send(p, p, 'x');",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "net 0 x\nnet 100 y\n");
}

#[test]
fn same_delay_timers_fire_in_registration_order() {
    // Equal whole-ms deadlines are common, so ties need the registration
    // order as the tiebreak: 18 timers over three shared deadlines must
    // fire FIFO, in both real mode (this test, the shape of the review's
    // ord.js probe) and virtual mode (below), exactly as Node does.
    let src = "const out = [];\n         for (let i = 0; i < 8; i++) setTimeout(() => out.push('a' + i), 0);\n         for (let i = 0; i < 5; i++) setTimeout(() => out.push('b' + i), 1);\n         for (let i = 0; i < 5; i++) setTimeout(() => out.push('c' + i), 5);\n         setTimeout(() => console.log(out.join(' ')), 50);";
    let want = "a0 a1 a2 a3 a4 a5 a6 a7 b0 b1 b2 b3 b4 c0 c1 c2 c3 c4\n";
    let r = run_js("vclock-fifo-real", src);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, want);
    let r = run_js_with(
        &["--clock=virtual", "--timeout=5000"],
        "vclock-fifo-virt",
        src,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, want);
}

#[test]
fn tied_timers_across_nesting_fire_in_registration_order() {
    // z is scheduled from inside in-y's callback with the same deadline
    // (100) as the x timers, which were registered earlier at the top
    // level: every x fires before z. Node runs the same order. The filler
    // timers at the same deadline scramble the timer Vec enough that an
    // order-unaware selection cannot pass by luck.
    let r = run_js_with(
        &["--clock=virtual", "--timeout=5000"],
        "vclock-tie-nest",
        "const out = [];\n         for (let i = 0; i < 6; i++) setTimeout(() => out.push('x' + i + ' ' + Date.now()), 100);\n         setTimeout(() => { out.push('in-y ' + Date.now()); setTimeout(() => out.push('z ' + Date.now()), 50); }, 50);\n         setTimeout(() => console.log(out.join('\\n')), 200);",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout,
        "in-y 50\nx0 100\nx1 100\nx2 100\nx3 100\nx4 100\nx5 100\nz 100\n"
    );
}

#[test]
fn virtual_tied_deadlines_across_schedules_fire_in_registration_order() {
    // The review's regression: a timer registered at t=0 for 600 used to
    // fire after one registered at t=100 for +500 (same deadline 600).
    // Registration order wins; Node runs the same order. Fillers at the
    // shared deadline make an order-unaware selection visibly wrong.
    let r = run_js_with(
        &["--clock=virtual", "--timeout=5000"],
        "vclock-tie-cross",
        "const out = [];\n         for (let i = 0; i < 4; i++) setTimeout(() => out.push('a' + i + ' ' + Date.now()), 600);\n         setTimeout(() => { out.push('b ' + Date.now()); setTimeout(() => out.push('b1 ' + Date.now()), 500); }, 100);\n         setTimeout(() => console.log(out.join('\\n')), 700);",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "b 100\na0 600\na1 600\na2 600\na3 600\nb1 600\n");
}

#[test]
fn real_mode_timers_are_exact_neither_early_nor_padded() {
    // Round-2 regression: whole-millisecond deadlines rounded up made
    // every hop pay up to one extra millisecond (300 chained setTimeout(0)
    // measured 617 ms where main and Node take ~316; 100 x 5 ms measured
    // 601 against 502). Real deadlines are exact nanoseconds from the
    // run's clock now, so the two pinned properties are: never early (each
    // chain's total is at least its hops times its delay) and no padding.
    // Padding is judged by the upper median gap between hops, not by the
    // total: a loaded host delays some hops by milliseconds, which moves a
    // total past any tight bound but leaves the median where the delay put
    // it, while the old +1 ms padding shifted every gap.
    let r = run_js(
        "vclock-exact",
        r#"
        const done = [];
        const median = (xs) => [...xs].sort((a, b) => a - b)[xs.length >> 1];
        function report(label, t0, gaps) {
          done.push(label, (performance.now() - t0).toFixed(3), median(gaps).toFixed(3));
        }
        function chain(label, delay, hops) {
          const t0 = performance.now(), gaps = [];
          let last = t0;
          const step = () => {
            const now = performance.now();
            gaps.push(now - last); last = now;
            if (gaps.length < hops) setTimeout(step, delay);
            else report(label, t0, gaps);
          };
          setTimeout(step, delay);
        }
        chain("chain0", 0, 300);
        chain("chain5", 5, 100);
        const ti = performance.now(), ticks = [];
        let lastTick = ti;
        const iv = setInterval(() => {
          const now = performance.now();
          ticks.push(now - lastTick); lastTick = now;
          if (ticks.length === 30) { clearInterval(iv); report("iv30", ti, ticks); }
        }, 10);
        setTimeout(() => console.log(done.join(" ")), 4000);
        "#,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    // Completion order varies with the three chains' lengths; find each
    // measurement by its label: the total span, then the median gap.
    let toks: Vec<&str> = r.stdout.split_whitespace().collect();
    let labeled = |want: &str| -> (f64, f64) {
        let i = toks
            .iter()
            .position(|t| *t == want)
            .unwrap_or_else(|| panic!("label {} missing from {:?}", want, toks));
        (toks[i + 1].parse().unwrap(), toks[i + 2].parse().unwrap())
    };
    // Each 0-delay hop waits its clamped 1 ms; the padded timeline paid
    // about 2.06 ms per hop.
    let (chain0_ms, chain0_gap) = labeled("chain0");
    assert!(chain0_ms >= 300.0, "chain0 {} ms", chain0_ms);
    assert!(chain0_gap < 1.5, "chain0 median gap {} ms", chain0_gap);
    // 100 hops of a full 5 ms each; the padded timeline paid ~6.01 ms.
    let (chain5_ms, chain5_gap) = labeled("chain5");
    assert!(chain5_ms >= 500.0, "chain5 {} ms", chain5_ms);
    assert!(chain5_gap < 5.5, "chain5 median gap {} ms", chain5_gap);
    // 30 interval ticks at 10 ms; the padded timeline paid ~11.07 ms.
    let (iv30_ms, iv30_gap) = labeled("iv30");
    assert!(iv30_ms >= 300.0, "iv30 {} ms", iv30_ms);
    assert!(iv30_gap < 10.5, "iv30 median tick {} ms", iv30_gap);
}

#[test]
fn virtual_time_date_clamps_at_the_es_maximum() {
    // With the largest accepted epoch, advancing the timeline must keep
    // Date.now() inside the ECMAScript time range (it used to report the
    // saturated u64 as 18446744073709552000), while performance.now -
    // the plain timeline - keeps counting.
    let max = "8640000000000000";
    let r = run_js_with(
        &["--clock=virtual", &format!("--epoch={}", max), "--timeout=5000"],
        "vclock-date-clamp",
        "console.log(Date.now(), new Date().getTime());\n         setTimeout(() => console.log(Date.now(), performance.now(), new Date().getTime()), 1000);",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout,
        "8640000000000000 8640000000000000\n8640000000000000 1000 8640000000000000\n"
    );
}

#[test]
fn virtual_time_flags_are_validated() {
    let path = script("vclock-flags", "");
    let entry = path.to_str().unwrap();
    assert_eq!(tardigrade(&["--clock=real", entry], b"").code, 2);
    assert_eq!(
        tardigrade(&["--clock=virtual", "--epoch=abc", entry], b"").code,
        2
    );
    assert_eq!(tardigrade(&["--epoch=5", entry], b"").code, 2);
    assert_eq!(
        tardigrade(&["--clock=virtual", "--epoch=0", entry], b"").code,
        0
    );
    // One above the ECMAScript maximum time value.
    assert_eq!(
        tardigrade(&["--clock=virtual", "--epoch=8640000000000001", entry], b"").code,
        2
    );
    assert_eq!(
        tardigrade(&["--clock=virtual", "--epoch=8640000000000000", entry], b"").code,
        0
    );
}

#[test]
fn real_time_is_the_default_and_reads_the_host_clock() {
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let r = run_js(
        "vclock-real-default",
        "console.log(Date.now(), Math.floor(performance.now()), typeof globalThis.__virtualDateMs, typeof Date());",
    );
    let after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let mut it = r.stdout.split_whitespace();
    let now: u64 = it.next().unwrap().parse().unwrap();
    let perf: u64 = it.next().unwrap().parse().unwrap();
    assert!(
        now >= before && now <= after,
        "Date.now is the host clock: {}",
        now
    );
    assert!(
        perf < 60_000,
        "performance.now started with the run: {}",
        perf
    );
    assert_eq!(it.collect::<Vec<_>>(), ["undefined", "string"]);
}

// ---- --sandbox (OS self-containment) ----

const SANDBOX_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

/// The flag scripts gate their skips on: exit 0 with a word when sandboxing
/// is possible and 2 with the reason otherwise. The kernel is queried
/// independently of the library under test, so a lying answer fails.
#[test]
fn sandbox_check_matches_the_kernel() {
    let r = tardigrade(&["--sandbox-check"], b"");
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if common::kernel_supports_sandbox() {
        assert_eq!(
            (r.code, r.stdout.trim()),
            (0, "supported"),
            "stderr: {}",
            r.stderr
        );
        return;
    }
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("--sandbox is not available"),
        "stderr: {}",
        r.stderr
    );
}

/// The probe flag only means something as the sole argument. Anywhere before
/// the entry it is a usage error, so a typo cannot turn a run into a check.
#[test]
fn sandbox_check_before_the_entry_or_with_other_flags_is_a_usage_error() {
    let path = script("sb-check-misplaced", "console.log('ran');");
    let entry = path.to_str().unwrap();
    for args in [
        vec!["--sandbox-check", "--bogus"],
        vec!["--sandbox-check", entry],
    ] {
        let r = tardigrade(&args, b"");
        assert_eq!(r.code, 2, "{:?} stderr: {}", args, r.stderr);
        assert_eq!(r.stdout, "", "{:?}: no probe answer and no guest run", args);
        assert!(
            r.stderr
                .contains("--sandbox-check must be the only argument"),
            "stderr: {}",
            r.stderr
        );
    }
    // After `--`, "--sandbox-check" is a positional: it names the entry
    // file, which cannot be opened. (`--` now ends option parsing; this
    // case used to be a misplaced-probe usage error.)
    let r = tardigrade(&["--", "--sandbox-check"], b"");
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("cannot open"), "stderr: {}", r.stderr);
}

/// After the entry (with or without `--`) it is an ordinary guest argument:
/// the script runs and sees it in vm.args.
#[test]
fn sandbox_check_after_the_entry_is_a_guest_argument() {
    let path = script("sb-check-arg", "console.log(JSON.stringify(vm.args));");
    let entry = path.to_str().unwrap();
    for args in [
        vec![entry, "--sandbox-check"],
        vec![entry, "--", "--sandbox-check"],
    ] {
        let r = tardigrade(&args, b"");
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
        let r = tardigrade_bounded(
            &["--sandbox", &dump_arg, path.to_str().unwrap()],
            b"",
            SANDBOX_LIMIT,
        );
        assert_eq!(r.code, 2);
        assert_eq!(r.stdout, "", "guest must not run");
        assert!(r.stderr.contains("sandbox"), "stderr: {}", r.stderr);
    }

    #[test]
    fn sandbox_refuses_a_dump_that_is_not_a_regular_file() {
        // The child's stdout is a pipe, so /dev/stdout is one too.
        let path = script("sb-pipe-dump", "console.log('ran');");
        let r = tardigrade_bounded(
            &["--sandbox", "--dump=/dev/stdout", path.to_str().unwrap()],
            b"",
            SANDBOX_LIMIT,
        );
        assert_eq!(r.code, 2);
        assert_eq!(r.stdout, "", "guest must not run");
        assert!(
            r.stderr.contains("--dump must be a regular file"),
            "stderr: {}",
            r.stderr
        );
    }
}

/// Whether this kernel can sandbox. Where it cannot, the test must then
/// check the refusal with `assert_refused` and return, and a skip notice is
/// printed. TARDIGRADE_REQUIRE_SANDBOX=1 makes such a kernel a failure instead.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
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

/// On a kernel without Landlock ABI 6 the contract is a clear exit 2 and no
/// guest run, never an unsandboxed run.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn assert_refused(args: &[&str], stdin: &[u8]) {
    let r = tardigrade_bounded(args, stdin, SANDBOX_LIMIT);
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "", "guest must not run");
    assert!(
        r.stderr.contains("tardigrade: --sandbox:"),
        "stderr: {}",
        r.stderr
    );
    assert!(r.stderr.contains("Landlock"), "stderr: {}", r.stderr);
}

/// Runs tardigrade under `--sandbox`; None means the kernel cannot sandbox and the
/// refusal has been checked.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn sandboxed(test: &str, args: &[&str], stdin: &[u8]) -> Option<Run> {
    let mut full = vec!["--sandbox"];
    full.extend_from_slice(args);
    if sandbox_available(test) {
        return Some(tardigrade_bounded(&full, stdin, SANDBOX_LIMIT));
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
        let Some(r) = sandboxed("sandbox_enforces_the_heap_cap_and_the_timeout", &args, b"") else {
            return;
        };
        assert_eq!(r.code, 137, "stderr: {}", r.stderr);
        assert!(
            r.stderr.contains("tardigrade: heap limit exceeded (32 MB)"),
            "stderr: {}",
            r.stderr
        );
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
        let Some(r) = sandboxed(
            "sandbox_runs_a_plain_guest_end_to_end",
            &[path.to_str().unwrap()],
            b"",
        ) else {
            return;
        };
        assert_eq!(r.code, 0, "stderr: {}", r.stderr);
        assert_eq!(r.stdout, "hello\ntick 100000\n");
    }

    #[test]
    fn sandbox_timeout_kills_a_spinning_guest_with_124() {
        let path = script("sb-spin", "for (;;) {}");
        let args = ["--timeout=100", path.to_str().unwrap()];
        let Some(r) = sandboxed(
            "sandbox_timeout_kills_a_spinning_guest_with_124",
            &args,
            b"",
        ) else {
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
            serde_json::from_str(&std::fs::read_to_string(&dump).expect("dump file"))
                .expect("dump json");
        assert_eq!(
            json,
            serde_json::json!({"dirs": ["/"], "files": {"/g": {"text": "v"}}, "version": 2})
        );
    }

    /// vm.net under the sandbox: the seccomp layer denies every socket
    /// syscall in this process, so the sidecar reply must come back over
    /// the kept fd with a plain read(2). A UnixStream reader would issue
    /// recvfrom(2), which the deny list refuses with EPERM; this test
    /// guards that regression (the guest socket syscalls stay denied).
    #[test]
    fn sandbox_net_request_reaches_the_granted_local_endpoint() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        // Bounded accept: a guest that never connects must fail the
        // assertions below, not hang the suite on join.
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                match listener.accept() {
                    Ok((mut conn, _)) => {
                        conn.set_nonblocking(false).unwrap();
                        conn.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                            .unwrap();
                        let mut buf = [0u8; 4096];
                        let _ = conn.read(&mut buf);
                        conn.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                        )
                        .unwrap();
                        return;
                    }
                    Err(_) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Err(_) => return,
                }
            }
        });
        let grant = format!("--allow-call=local=http://127.0.0.1:{port}/x,POST");
        let path = script(
            "sb-net-request",
            r#"(async () => {
                const res = await vm.net.request("local", "ping");
                console.log(res.status, res.text);
            })().catch(e => console.log("ERR", String(e && e.message || e)));
            "#,
        );
        let args = [
            grant.as_str(),
            "--net-allow-private",
            path.to_str().unwrap(),
        ];
        let Some(r) = sandboxed(
            "sandbox_net_request_reaches_the_granted_local_endpoint",
            &args,
            b"",
        ) else {
            return;
        };
        server.join().unwrap();
        assert_eq!(r.code, 0, "stderr: {}", r.stderr);
        assert_eq!(r.stdout, "200 hello\n", "stdout: {}", r.stdout);
    }

    /// Waits for the child to carry a seccomp filter, so a serve run that
    /// silently skipped the sandbox fails instead of passing on output alone.
    fn wait_for_seccomp(pid: u32) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let status =
                std::fs::read_to_string(format!("/proc/{}/status", pid)).unwrap_or_default();
            if status.contains("Seccomp:\t2") {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "serve never became sandboxed: {}",
                status
            );
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
        let r = tardigrade_bounded_started(&args, commands, SANDBOX_LIMIT, wait_for_seccomp);
        assert_eq!(r.code, 0, "stderr: {}", r.stderr);
        assert_eq!(r.stdout, "2\n5\n");
    }

    /// The virtual clock works under the sandbox layers too: the loop
    /// jump, the Date override and the epoch all behave the same.
    #[test]
    fn sandbox_virtual_time_runs_and_jumps() {
        let test = "sandbox_virtual_time_runs_and_jumps";
        let path = script(
            "sb-vclock",
            "const E = 4242;\n             console.log('start', Date.now() === E, performance.now());\n             setTimeout(() => { console.log('fired', Date.now() - E); vm.exit(0); }, 10000);",
        );
        let args = [
            "--clock=virtual",
            "--epoch=4242",
            "--timeout=5000",
            path.to_str().unwrap(),
        ];
        let Some(r) = sandboxed(test, &args, b"") else {
            return;
        };
        assert_eq!(r.code, 0, "stderr: {}", r.stderr);
        assert_eq!(r.stdout, "start true 0\nfired 10000\n");
    }

    /// The web-platform globals work under the sandbox layers too (the
    /// TARDIGRADE_REQUIRE_SANDBOX gate runs this test for real).
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
        let Some(r) = sandboxed(test, &[path.to_str().unwrap()], b"") else {
            return;
        };
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
    let r = tardigrade_bounded(&["--sandbox", path.to_str().unwrap()], b"", SANDBOX_LIMIT);
    assert_eq!(r.code, 2);
    assert_eq!(r.stdout, "", "guest must not run");
    assert!(
        r.stderr
            .contains("--sandbox is only supported on Linux x86_64"),
        "stderr: {}",
        r.stderr
    );
}

// --- TypeScript agent loop (examples/agent-loop-ts.sh) ---

/// TARDIGRADE_REQUIRE_TS=1 turns a missing TypeScript toolchain from a skip into
/// a failure, mirroring TARDIGRADE_REQUIRE_SANDBOX for the kernel checks. The
/// tests that use these helpers run only on Linux x86_64 (where the
/// bounded runner is), so they are gated the same way and stay dead-code
/// clean on other targets.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn ts_required() -> bool {
    std::env::var("TARDIGRADE_REQUIRE_TS").is_ok_and(|v| v == "1")
}

/// A skip notice written straight to stderr, so it survives libtest's
/// capture and a skipped test is never mistaken for a pass.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn ts_skipped(reason: &str, test: &str) {
    let _ = writeln!(std::io::stderr(), "SKIPPED ({}): {}", reason, test);
}

/// True when `tool --version` runs successfully: an executable the loop
/// can use, not just a name on PATH.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn runs_on_host(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success())
}

/// The pinned tsc-rs package, the same default examples/ts-pin.sh reads
/// through TARDIGRADE_TSC_RS_PKG; every test passes it on to the scripts
/// through that variable, so the probes here and the loop can never
/// disagree about which checker runs.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const TSC_RS_PKG: &str = "tsc-rs@0.3.0";

/// The primary checker command built from the pin, exactly as
/// examples/agent-loop-ts.sh builds it - including the
/// TARDIGRADE_TSC_RS_PKG override, so a test forced onto another pin
/// probes and runs the same checker the scripts would.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn tsc_rs_cmd() -> String {
    let pkg = std::env::var("TARDIGRADE_TSC_RS_PKG").unwrap_or_else(|_| TSC_RS_PKG.to_string());
    format!("npx -y {pkg}")
}

/// The first checker command that really runs - the same bounded probe
/// and the same order (tsc-rs fetched by npx, then tsc) as the loop, so a
/// test gated here runs the checker that would actually decide. None
/// means no usable checker: a skip, not a confusing later failure.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn usable_checker(primary: &str, fallback: &str) -> Option<String> {
    let probe = |cmd: &str| {
        Command::new("timeout")
            .arg("60")
            .args(cmd.split_whitespace())
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if probe(primary) {
        Some(primary.to_string())
    } else if probe(fallback) {
        Some(fallback.to_string())
    } else {
        None
    }
}

/// bun for the loop: $BUN, then PATH, then the default install location -
/// the same order examples/ts-smoke.sh probes.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn find_bun() -> Option<String> {
    if let Ok(bun) = std::env::var("BUN") {
        if runs_on_host(&bun) {
            return Some(bun);
        }
    }
    if runs_on_host("bun") {
        return Some("bun".to_string());
    }
    let home = std::env::var("HOME").ok()?;
    let bun = format!("{}/.bun/bin/bun", home.trim_end_matches('/'));
    runs_on_host(&bun).then_some(bun)
}

/// The smoke check must skip loudly when the toolchain is missing, never
/// pass silently: exit 77 (the automake skip code the Makefile recipe
/// turns into TS-SKIPPED), the reason on stderr, and no TS-OK anywhere.
/// An emptied PATH and HOME make the probes fail even on a host that has
/// the tools, so the skip path itself is what is tested.
#[test]
fn ts_smoke_skips_loudly_without_a_toolchain() {
    let home = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ts-smoke-empty-home");
    std::fs::create_dir_all(&home).expect("create fake home");
    // /bin/bash, not "bash": the emptied child PATH is applied before the
    // exec, so a PATH lookup for the shell itself would fail. The require
    // flag is scrubbed: this test owns it, and an inherited
    // TARDIGRADE_REQUIRE_TS=1 (the documented strict mode) would turn the
    // skip into exit 1 and make strict mode unable to ever pass.
    let out = Command::new("/bin/bash")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/ts-smoke.sh"))
        .env("OUT", env!("CARGO_BIN_EXE_tardigrade"))
        .env("BUN", "")
        .env("HOME", &home)
        .env("PATH", "")
        .env_remove("TARDIGRADE_REQUIRE_TS")
        .output()
        .expect("run ts-smoke");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(77), "stderr: {stderr}");
    assert!(!stdout.contains("TS-OK"), "a skip must never print TS-OK");
    assert!(
        stderr.contains("SKIPPED (") && stderr.contains("TypeScript agent-loop smoke check"),
        "the skip must be loud, stderr: {stderr}"
    );
}

/// With the toolchain still missing, TARDIGRADE_REQUIRE_TS=1 makes that same
/// skip a failure naming the flag.
#[test]
fn ts_smoke_require_flag_fails_when_the_toolchain_is_missing() {
    let home = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ts-smoke-empty-home");
    std::fs::create_dir_all(&home).expect("create fake home");
    let out = Command::new("/bin/bash")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/ts-smoke.sh"))
        .env("OUT", env!("CARGO_BIN_EXE_tardigrade"))
        .env("BUN", "")
        .env("HOME", &home)
        .env("PATH", "")
        .env("TARDIGRADE_REQUIRE_TS", "1")
        .output()
        .expect("run ts-smoke");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        !stdout.contains("TS-OK"),
        "a failed skip must never print TS-OK"
    );
    assert!(
        stderr.contains("TARDIGRADE_REQUIRE_TS=1 but"),
        "the failure must name the flag, stderr: {stderr}"
    );
}

/// bun present but no usable checker is still the loud missing-toolchain
/// skip, not a broken run: PATH holds a stub bun that answers --version
/// and a stub npx that is present but fails (offline, nothing cached),
/// and tsc is unreachable.
#[test]
fn ts_smoke_skips_when_bun_cannot_be_paired_with_a_checker() {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ts-stub-bun");
    let _ = std::fs::remove_dir_all(&root);
    let bin = root.join("bin");
    let home = root.join("home");
    std::fs::create_dir_all(&bin).expect("mkdir stub bin");
    std::fs::create_dir_all(&home).expect("mkdir stub home");
    let stub = bin.join("bun");
    std::fs::write(&stub, "#!/bin/sh\necho 1.3.5\n").expect("write stub bun");
    make_executable(&stub);
    let dead_npx = bin.join("npx");
    std::fs::write(&dead_npx, "#!/bin/sh\nexit 1\n").expect("write dead stub npx");
    make_executable(&dead_npx);

    let run = |require: bool| {
        Command::new("/bin/bash")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/ts-smoke.sh"))
            .env("OUT", env!("CARGO_BIN_EXE_tardigrade"))
            .env("BUN", &stub)
            .env("HOME", &home)
            .env("PATH", &bin)
            .env("TARDIGRADE_REQUIRE_TS", if require { "1" } else { "" })
            .output()
            .expect("run ts-smoke")
    };
    let out = run(false);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(77), "stderr: {stderr}");
    assert!(!stdout.contains("TS-OK"));
    assert!(
        stderr.contains("SKIPPED (") && stderr.contains("no usable type checker"),
        "the skip must name the unusable checker, stderr: {stderr}"
    );
    let out = run(true);
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("TARDIGRADE_REQUIRE_TS=1 but"),
        "stderr: {stderr}"
    );
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn make_executable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(path).expect("stat stub").permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(path, perm).expect("chmod stub");
}

/// Runs a command to completion under a hard wall-clock bound; 180 s
/// covers npx fetching tsc-rs on a cold cache. Two invariants, from
/// kill(2), govern the timeout path: the process group a negative pid
/// names lives only while its leader is unreaped, and this child - whose
/// id is the asserted pgid - is reaped only after the signal, so the
/// signal can never name a reused or foreign pgid; and pgid > 1 keeps
/// init (and pid 0's own group) out of range by construction.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn output_within(mut cmd: Command, secs: u64) -> std::process::Output {
    use std::os::unix::process::CommandExt;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd.spawn().expect("spawn");
    let pgid = i32::try_from(child.id()).expect("pid fits in pid_t");
    assert!(pgid > 1, "refusing to signal pgid {pgid}");
    // The pipes are drained on their own threads; the Child itself stays
    // here so only this thread ever reaps it.
    let out_pipe = child.stdout.take().expect("stdout pipe");
    let err_pipe = child.stderr.take().expect("stderr pipe");
    // Readers deliver through channels, so waiting on them is bounded: a
    // descendant that left the group can hold the pipes open past the
    // leader's exit forever, and an unbounded join would hang the test.
    let out_rx = drain_pipe(out_pipe);
    let err_rx = drain_pipe(err_pipe);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Err(e) => panic!("wait failed: {e}"),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Ok(None) => {
                // Signal before reap: the leader's death would dissolve
                // the group and free the pgid for reuse.
                unsafe { libc::kill(-pgid, libc::SIGKILL) };
                let status = child.wait().expect("wait after kill");
                panic!(
                    "command did not exit within {secs} s (killed pgid {pgid}, status {status}) \
                     | stdout: {} | stderr: {}",
                    String::from_utf8_lossy(&recv_by(&out_rx, deadline)),
                    String::from_utf8_lossy(&recv_by(&err_rx, deadline))
                );
            }
        }
    };
    // Same bound on the normal path: whatever arrived by the deadline is
    // the output; a still-blocked reader contributes nothing rather than
    // hanging the suite.
    let stdout = recv_by(&out_rx, deadline);
    let stderr = recv_by(&err_rx, deadline);
    std::process::Output {
        status,
        stdout,
        stderr,
    }
}

/// Reads one pipe to its end on a thread and delivers the bytes through a
/// channel, so the wait for them can be bounded (see recv_by).
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn drain_pipe(mut pipe: impl std::io::Read + Send + 'static) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    rx
}

/// Receives one reader's buffer, bounded by `deadline`; empty when the
/// reader is still blocked on a pipe some descendant holds.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn recv_by(rx: &std::sync::mpsc::Receiver<Vec<u8>>, deadline: std::time::Instant) -> Vec<u8> {
    let Some(wait) = deadline.checked_duration_since(std::time::Instant::now()) else {
        return Vec::new();
    };
    rx.recv_timeout(wait).unwrap_or_default()
}

/// The full loop against the real tools: the seeded type errors end their
/// turn as FAIL with the checker's lines and the checker named, the
/// type-correct-but-wrong app FAILs through the driver, and the fixed app
/// prints PASS.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn agent_loop_ts_type_errors_fail_then_fixed_app_passes() {
    let test = "agent_loop_ts_type_errors_fail_then_fixed_app_passes";
    let Some(bun) = find_bun() else {
        assert!(
            !ts_required(),
            "TARDIGRADE_REQUIRE_TS=1 but bun is not installed"
        );
        ts_skipped("bun is not installed", test);
        return;
    };
    if usable_checker(&tsc_rs_cmd(), "tsc").is_none() {
        assert!(
            !ts_required(),
            "TARDIGRADE_REQUIRE_TS=1 but no usable type checker"
        );
        ts_skipped(
            "no usable type checker (npx cannot fetch tsc-rs, and tsc is absent)",
            test,
        );
        return;
    }
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("agent-loop-ts");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("mkdir loop scratch");
    let ws = root.join("ws.json");
    // A stale workspace file must not leak into the run: the loop starts
    // by removing it.
    std::fs::write(&ws, b"not json at all").expect("poison the stale workspace");
    let mut cmd = Command::new("/bin/bash");
    cmd.arg(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/agent-loop-ts.sh"
    ))
    .env("OUT", env!("CARGO_BIN_EXE_tardigrade"))
    .env("WS", &ws)
    .env("BUN", bun)
    // An isolated TMPDIR proves the loop's scratch dir is cleaned up.
    .env("TMPDIR", &root)
    .env("TARDIGRADE_TSC_RS_PKG", TSC_RS_PKG);
    let out = output_within(cmd, 180);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {stdout} stderr: {stderr}"
    );
    assert!(
        stdout.contains("FAIL: TypeScript type errors"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("error TS"), "stdout: {stdout}");
    assert!(stdout.contains("checker: "), "stdout: {stdout}");
    assert!(
        stdout.contains("FAIL: sum([1,2,3]) returned -6, want 6"),
        "stdout: {stdout}"
    );
    assert!(stdout.lines().any(|l| l == "PASS"), "stdout: {stdout}");
    let leftovers: Vec<String> = std::fs::read_dir(&root)
        .expect("list loop scratch")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("tardigrade-agent-ts."))
        .collect();
    assert!(
        leftovers.is_empty(),
        "loop scratch dirs left behind: {leftovers:?}"
    );
}

/// The in-guest TypeScript check (examples/ts-in-guest): when
/// TSC_RUST_WASM names a ts_rust.wasm build, the compiler runs inside the
/// guest - type errors exit 2 with tsc-style lines, a clean project
/// transpiles into the RAM fs. The artifact is never committed, so a
/// missing one is a loud skip; TARDIGRADE_REQUIRE_TS=1 fails instead.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn ts_wasm_checks_and_transpiles_inside_the_guest() {
    let test = "ts_wasm_checks_and_transpiles_inside_the_guest";
    let Ok(wasm) = std::env::var("TSC_RUST_WASM") else {
        assert!(
            !ts_required(),
            "TARDIGRADE_REQUIRE_TS=1 but TSC_RUST_WASM is not set to a ts_rust.wasm build"
        );
        ts_skipped(
            "TSC_RUST_WASM is not set (the wasm build is not committed)",
            test,
        );
        return;
    };
    if !PathBuf::from(&wasm).is_file() {
        assert!(
            !ts_required(),
            "TARDIGRADE_REQUIRE_TS=1 but TSC_RUST_WASM={} is not a file",
            wasm
        );
        ts_skipped("TSC_RUST_WASM does not name an existing wasm build", test);
        return;
    }
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ts-in-guest");
    let _ = std::fs::remove_dir_all(&root);
    // A broken project and a clean one with emit; the tsconfigs decide.
    let bad = root.join("bad");
    let good = root.join("good");
    std::fs::create_dir_all(bad.join("lib")).expect("mkdir bad");
    std::fs::create_dir_all(good.join("lib")).expect("mkdir good");
    std::fs::write(
        bad.join("app.ts"),
        "import { sum } from \"./lib/sum\";\nconst wrong: number = sum([\"a\", \"b\"]);\n",
    )
    .expect("write bad app");
    std::fs::write(
        bad.join("lib/sum.ts"),
        "export const sum = (xs: string[]): string => xs.join(\"\");\n",
    )
    .expect("write bad lib");
    std::fs::write(
        bad.join("tsconfig.json"),
        r#"{ "compilerOptions": { "strict": true, "noEmit": true }, "include": ["app.ts", "lib"] }"#,
    )
    .expect("write bad tsconfig");
    std::fs::write(
        good.join("app.ts"),
        "import { sum } from \"./lib/sum\";\nimport { two } from \"./lib/we\\\"ird\";\nconsole.log(sum([\"a\", \"b\"]) + two);\n",
    )
    .expect("write good app");
    std::fs::write(
        good.join("lib/sum.ts"),
        "export const sum = (xs: string[]): string => xs.join(\"\");\n",
    )
    .expect("write good lib");
    // A quote in a file name must travel as data: run.sh embeds every
    // project file as a base64 snapshot entry and never splices a path
    // into program text.
    std::fs::write(good.join("lib/we\"ird.ts"), "export const two = 2;\n")
        .expect("write oddly named lib");
    // A tab is a legal file name character and JSON forbids raw control
    // characters in strings, so the snapshot keys must escape it.
    std::fs::write(good.join("lib/ta\tb.ts"), "export const three = 3;\n")
        .expect("write tab-named lib");
    std::fs::write(
        good.join("app.ts"),
        "import { sum } from \"./lib/sum\";\nimport { two } from \"./lib/we\\\"ird\";\nimport { three } from \"./lib/ta\tb\";\nconsole.log(sum([\"a\", \"b\"]) + two + three);\n",
    )
    .expect("write good app with odd imports");
    std::fs::write(
        good.join("tsconfig.json"),
        r#"{ "compilerOptions": { "strict": true, "outDir": "out" }, "include": ["app.ts", "lib"] }"#,
    )
    .expect("write good tsconfig");

    let run = |proj: &std::path::Path, ws: &str| {
        let mut cmd = Command::new("/bin/bash");
        cmd.arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/ts-in-guest/run.sh"
        ))
        .env("OUT", env!("CARGO_BIN_EXE_tardigrade"))
        .env("TSC_RUST_WASM", &wasm)
        .arg(proj)
        .arg(ws);
        output_within(cmd, 180)
    };

    let out = run(&bad, root.join("bad-ws.json").to_str().unwrap());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(2), "stdout: {stdout}");
    assert!(
        stdout.contains("error TS2322") && stdout.contains("app.ts(2,7)"),
        "the type error must print tsc-style, stdout: {stdout}"
    );

    let good_ws = root.join("good-ws.json");
    // A trailing slash on the project directory must not leak into the
    // seeded /app paths.
    let slashed = format!("{}/", good.display());
    let out = run(std::path::Path::new(&slashed), good_ws.to_str().unwrap());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "stdout: {stdout}");
    let dump = std::fs::read_to_string(&good_ws).expect("read the workspace dump");
    assert!(
        dump.contains("/app/out/app.js"),
        "emit must land in vm.fs: {dump}"
    );
    assert!(
        dump.contains("/app/out/lib/sum.js") && dump.contains("export const sum = (xs)"),
        "the transpiled lib must be in the dump: {dump}"
    );
    assert!(
        dump.contains("/app/out/lib/we\\\"ird.js") && dump.contains("export const two = 2;"),
        "the oddly named module must emit too: {dump}"
    );
    assert!(
        dump.contains("/app/out/lib/ta\\tb.js") && dump.contains("export const three = 3;"),
        "the tab-named module must emit too: {dump}"
    );
    assert!(
        !root.join("good-ws.json.pre").exists(),
        "no stale .pre snapshot"
    );
}

/// Members named by examples/vm.d.ts: (scope, name) with scope "vm",
/// "vm.fs", .... A namespace line records the name and opens a scope, a
/// lone "}" closes it, and every declaration shape the file may use -
/// function, const, let, var, class, interface, type - names its scope's
/// members, so a member smuggled in under any of them is still compared
/// with the runtime. Doc comments never match those shapes.
/// Declaration shapes the parser names: the bool says whether the shape
/// opens a named block. Braces are counted per line, so a multi-line
/// interface or object type inside a namespace keeps the scopes balanced
/// (an anonymous block pushes an empty name that its closer pops).
const DTS_SHAPES: [(&str, bool); 9] = [
    ("namespace ", true),
    ("class ", true),
    ("interface ", true),
    ("enum ", true),
    ("function ", false),
    ("const ", false),
    ("let ", false),
    ("var ", false),
    ("type ", false),
];

fn vm_dts_members(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    for raw in text.lines() {
        let mut line = raw.trim();
        for prefix in ["declare ", "export ", "abstract "] {
            if let Some(rest) = line.strip_prefix(prefix) {
                line = rest.trim_start();
            }
        }
        let mut opens_block = false;
        let mut name = String::new();
        for (shape, opens) in DTS_SHAPES {
            let Some(rest) = line.strip_prefix(shape) else {
                continue;
            };
            opens_block = opens;
            name = rest
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .next()
                .unwrap_or_default()
                .to_string();
            break;
        }
        if !name.is_empty() {
            out.push((stack.join("."), name.clone()));
        }
        // The member's scope is the stack before its own block opens.
        let delta = line.matches('{').count() as i32 - line.matches('}').count() as i32;
        if delta > 0 {
            stack.push(if opens_block { name } else { String::new() });
        } else if delta < 0 {
            for _ in 0..(-delta) {
                stack.pop();
            }
        }
    }
    out
}

/// examples/vm.d.ts must name every vm.* and WebAssembly member a live
/// guest has, and the guest must have every member it names. vm.net.request
/// is conditional on an operator grant, so compare both the offline and
/// capability-enabled surfaces.
#[test]
fn vm_dts_members_match_the_runtime() {
    let source = r#"
const groups = [
  ["vm", vm], ["vm.fs", vm.fs], ["vm.net", vm.net], ["vm.stdin", vm.stdin],
  ["WebAssembly", WebAssembly],
];
for (const [name, obj] of groups) {
  console.log(name + ": " + Object.getOwnPropertyNames(obj).sort().join(" "));
}
(async () => {
  if (typeof vm.net.request === "function") {
    const response = await vm.net.request(
      { toString() { return "parity"; } },
      { toString() { return "ping"; } },
    );
    console.log("request-result: " + response.status + " " + response.text);
  }
})();
"#;

    let runtime_members = |flags: &[&str], name: &str| {
        let r = run_js_with(flags, name, source);
        assert_eq!(r.code, 0, "stderr: {}", r.stderr);
        let mut members = Vec::new();
        for line in r.stdout.lines() {
            let (scope, names) = line.split_once(": ").expect("scope: names line");
            if !["vm", "vm.fs", "vm.net", "vm.stdin", "WebAssembly"].contains(&scope) {
                continue;
            }
            for member in names.split(' ') {
                members.push((scope.to_string(), member.to_string()));
            }
        }
        (members, r)
    };

    let dts = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/vm.d.ts"))
        .expect("read examples/vm.d.ts");
    // Only the compared trees: the runtime side enumerates vm, its
    // namespaces and WebAssembly; the file also declares globals (console,
    // timers, ...) that live outside them.
    let compared =
        |scope: &str| scope == "vm" || scope.starts_with("vm.") || scope == "WebAssembly";
    let declared: Vec<(String, String)> = vm_dts_members(&dts)
        .into_iter()
        .filter(|(scope, _)| compared(scope))
        .collect();
    assert!(!declared.is_empty(), "vm.d.ts parsed to nothing");

    let request_member = ("vm.net".to_string(), "request".to_string());
    assert!(
        declared.contains(&request_member),
        "vm.d.ts must declare the capability-gated request API"
    );
    // TypeScript describes the available API surface; it cannot know whether
    // the operator supplied a grant for this particular run.
    let offline_declared: Vec<_> = declared
        .iter()
        .filter(|member| *member != &request_member)
        .cloned()
        .collect();
    let (offline_runtime, offline_run) = runtime_members(&[], "vm-dts-members-offline");
    assert!(
        !offline_runtime.contains(&request_member),
        "vm.net.request must not exist without an operator capability"
    );
    for (scope, member) in &offline_declared {
        assert!(
            offline_runtime.contains(&(scope.clone(), member.clone())),
            "offline vm.d.ts declares {scope}.{member} but the runtime does not have it"
        );
    }
    for (scope, member) in &offline_runtime {
        assert!(
            offline_declared.contains(&(scope.clone(), member.clone())),
            "offline runtime has {scope}.{member} but vm.d.ts does not declare it"
        );
    }
    assert!(
        !offline_run.stdout.contains("request-result:"),
        "an offline guest must not call vm.net.request"
    );

    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                        .unwrap();
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 1024];
                    let mut request_end = None;
                    while request_end.is_none_or(|end| request.len() < end) {
                        let n = stream.read(&mut chunk).unwrap();
                        if n == 0 {
                            break;
                        }
                        request.extend_from_slice(&chunk[..n]);
                        if request_end.is_none() {
                            if let Some(header_end) =
                                request.windows(4).position(|window| window == b"\r\n\r\n")
                            {
                                let headers = String::from_utf8_lossy(&request[..header_end]);
                                let body_len = headers
                                    .lines()
                                    .find_map(|line| {
                                        line.to_ascii_lowercase()
                                            .strip_prefix("content-length:")
                                            .and_then(|length| length.trim().parse::<usize>().ok())
                                    })
                                    .expect("request has Content-Length");
                                request_end = Some(header_end + 4 + body_len);
                            }
                        }
                    }
                    let body_start = request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|header_end| header_end + 4)
                        .expect("request has headers");
                    assert_eq!(&request[body_start..], b"ping", "ToString body coercion");
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nenabled")
                        .unwrap();
                    return true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return false;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("accept loopback test request: {error}"),
            }
        }
    });
    let grant = format!("--allow-call=parity=http://127.0.0.1:{port}/,POST");
    let (enabled_runtime, enabled_run) = runtime_members(
        &[grant.as_str(), "--net-allow-private"],
        "vm-dts-members-enabled",
    );
    assert!(
        server.join().expect("loopback test server"),
        "no request reached the controlled endpoint"
    );
    assert_eq!(
        enabled_run.stdout.lines().last(),
        Some("request-result: 200 enabled"),
        "enabled request returns its promised response"
    );
    for (scope, member) in &declared {
        assert!(
            enabled_runtime.contains(&(scope.clone(), member.clone())),
            "enabled vm.d.ts declares {scope}.{member} but the runtime does not have it"
        );
    }
    for (scope, member) in &enabled_runtime {
        assert!(
            declared.contains(&(scope.clone(), member.clone())),
            "enabled runtime has {scope}.{member} but vm.d.ts does not declare it"
        );
    }
}

/// The parser keeps scopes balanced across every declaration shape the
/// file may grow: a multi-line interface, enum or abstract class inside a
/// namespace must not swallow the namespace's later members, and export
/// prefixes must not hide a declaration.
#[test]
fn vm_dts_parser_scopes_block_declarations() {
    let text = "\
declare namespace vm {\n  namespace fs {\n    interface Stat {\n      type: string;\n      size: number;\n    }\n    export function stat(path: string): Stat;\n    enum Kind { A,\n      B }\n    abstract class Base {\n      abstract kind(): string;\n    }\n    export const KIND: Kind;\n  }\n  const version: string;\n}\n";
    let members = vm_dts_members(text);
    let expect = [
        ("", "vm"),
        ("vm", "fs"),
        ("vm.fs", "Stat"),
        ("vm.fs", "stat"),
        ("vm.fs", "Kind"),
        ("vm.fs", "Base"),
        ("vm.fs", "KIND"),
        ("vm", "version"),
    ];
    assert_eq!(members, expect.map(|(s, n)| (s.to_string(), n.to_string())));
}

/// Standard base64 (RFC 4648, padded) for crafting extraction lines and
/// payloads in the tests below; the guest encodes with btoa the same way.
fn b64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Writes `files` into a fresh workspace through one guest run (content as
/// one argv entry each), the way the loop seeds sources.
fn seed_workspace(ws: &std::path::Path, files: &[(&str, &str)]) {
    let _ = std::fs::remove_file(ws);
    let mut args: Vec<String> = vec!["-".to_string(), format!("--dump={}", ws.display())];
    let mut program = String::new();
    for (i, (path, content)) in files.iter().enumerate() {
        program.push_str(&format!("vm.fs.write(\"{path}\", vm.args[{i}]);\n"));
        args.push(content.to_string());
    }
    let argrefs: Vec<&str> = args.iter().map(String::as_str).collect();
    let r = tardigrade(&argrefs, program.as_bytes());
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
}

/// Runs examples/ts-extract.sh over `lines` into `dir`, like the loop does.
fn ts_extract(dir: &std::path::Path, lines: &str) -> Run {
    let mut child = Command::new("/bin/bash")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/ts-extract.sh"
        ))
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ts-extract");
    let mut pipe = child.stdin.take().unwrap();
    pipe.write_all(lines.as_bytes()).expect("feed ts-extract");
    drop(pipe);
    let out = child.wait_with_output().expect("wait ts-extract");
    Run {
        code: out.status.code().expect("exit code"),
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
    }
}

fn tree_files(root: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("read dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push(path.to_string_lossy().into_owned());
            }
        }
    }
    walk(root, &mut out);
    out.sort();
    out
}

/// Workspace content is data, never framing: a .ts file whose content
/// contains the old marker lines (and a ../ path inside them, which once
/// chose a host path) extracts verbatim, nothing appears outside the
/// extraction dir, and the parser rejects crafted bad paths loudly -
/// vm.fs itself normalizes paths, so those lines are fed to the parser
/// directly, the only place they could exist.
#[test]
fn ts_extraction_treats_workspace_content_as_data() {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ts-extract-case");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("mkdir root");
    let dir = root.join("out");
    std::fs::create_dir_all(&dir).expect("mkdir out");
    let ws = root.join("ws.json");

    let poison = "const x = 1;\n\
                  ===TARDIGRADE_TS_FILE /app/../../../../pwned.ts\n\
                  const evil = 2;\n\
                  ===TARDIGRADE_TS_END /app/../../../../pwned.ts\n";
    seed_workspace(
        &ws,
        &[
            ("/app/app.ts", poison),
            ("/app/lib/sum.ts", "export const s = 1;\n"),
        ],
    );

    let listing = tardigrade(
        &[
            format!("--preload={}", ws.display()).as_str(),
            concat!(env!("CARGO_MANIFEST_DIR"), "/examples/ts-listing.js"),
        ],
        b"",
    );
    assert_eq!(listing.code, 0, "stderr: {}", listing.stderr);
    let r = ts_extract(&dir, &listing.stdout);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        std::fs::read(dir.join("app.ts")).unwrap(),
        poison.as_bytes(),
        "the marker-laden file must extract byte for byte"
    );
    assert_eq!(
        std::fs::read(dir.join("lib/sum.ts")).unwrap(),
        b"export const s = 1;\n"
    );
    // Nothing outside the extraction dir: only the two files exist under
    // the isolated root besides the workspace itself.
    assert_eq!(
        tree_files(&root),
        vec![
            dir.join("app.ts").to_string_lossy().into_owned(),
            dir.join("lib/sum.ts").to_string_lossy().into_owned(),
            ws.to_string_lossy().into_owned(),
        ],
        "extraction must not write anywhere but its dir"
    );

    // Crafted lines the guest cannot produce (vm.fs normalizes paths):
    // each is rejected loudly, and nothing is written for it. The newline
    // case is the one grep's per-line matching let through: a first line
    // that is valid with an arbitrary second line riding along - vm.fs
    // itself accepts such a name, so it can reach the extractor.
    let cases: Vec<(&str, &str)> = vec![
        ("/app/../../pwned.ts", "..' segment"),
        ("/app/a/../b.ts", "'..' segment"),
        ("/etc/passwd", "not under /app"),
        ("/app/a b.ts", "characters outside"),
        (
            "/app/ok.ts\nwith space; $(touch PWNED_CMD).ts",
            "characters outside",
        ),
        ("/app/sub/x.js", "not a .ts/.tsx/.d.ts/.json file"),
    ];
    for (path, want) in cases {
        let fresh = root.join("reject");
        let _ = std::fs::remove_dir_all(&fresh);
        std::fs::create_dir_all(&fresh).expect("mkdir reject");
        let line = format!("{} {}\n", b64(path.as_bytes()), b64(b"content"));
        let r = ts_extract(&fresh, &line);
        assert_eq!(r.code, 1, "parser must reject {path}");
        assert!(
            r.stderr.contains("refusing") && r.stderr.contains(path) && r.stderr.contains(want),
            "rejection of {path} must name it and the reason: {}",
            r.stderr
        );
        assert!(
            tree_files(&fresh).is_empty(),
            "nothing may be written for {path}"
        );
    }
}

/// Bundles travel on stdin as base64, not as one argv entry (the kernel
/// caps a single argument near 128 KiB): a bundle well past that cap
/// loads through examples/ts-load-bundle.js exactly the way the loop's
/// build step does, and the loaded /app/app.js runs.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn ts_bundle_beyond_the_argv_limit_still_loads_and_runs() {
    let test = "ts_bundle_beyond_the_argv_limit_still_loads_and_runs";
    let Some(bun) = find_bun() else {
        assert!(
            !ts_required(),
            "TARDIGRADE_REQUIRE_TS=1 but bun is not installed"
        );
        ts_skipped("bun is not installed", test);
        return;
    };
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ts-big-bundle");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("mkdir big-bundle scratch");
    let literal: String = "A".repeat(240_000);
    std::fs::write(
        root.join("big.ts"),
        format!("export const big = \"{literal}\";\n(globalThis as any).bigLen = big.length;\n"),
    )
    .expect("write big fixture");
    let mut bun_cmd = Command::new(&bun);
    bun_cmd
        .args([
            "build",
            "big.ts",
            "--target=browser",
            "--format=iife",
            "--outfile=big.js",
        ])
        .current_dir(&root);
    let out = output_within(bun_cmd, 60);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let bundle = std::fs::read(root.join("big.js")).expect("read the bundle");
    assert!(bundle.len() > 200_000, "bundle only {} bytes", bundle.len());

    let ws = root.join("ws.json");
    let loader = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/ts-load-bundle.js");
    let encoded = format!("{}\n", b64(&bundle));
    let r = tardigrade(
        &[loader, &format!("--dump={}", ws.display())],
        encoded.as_bytes(),
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let probe = script(
        "ts-big-bundle-probe",
        "vm.run(\"/app/app.js\");\nconsole.log(globalThis.bigLen);",
    );
    let r = tardigrade(
        &[
            &format!("--preload={}", ws.display()),
            probe.to_str().unwrap(),
        ],
        b"",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout, "240000\n",
        "the full bundle must have loaded and run"
    );
}

/// The loop's fallback checker path: with tsc-rs forced to fail
/// (TARDIGRADE_TSC_RS=/bin/false), the pinned typescript tsc decides, the loop
/// still passes end to end, and its output names the checker that ran.
/// This is also where a checker-dependent tsconfig (an ES5 default target
/// breaks Set for-of, #private and bigint) shows up as a failure.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn agent_loop_ts_with_the_tsc_fallback_passes() {
    let test = "agent_loop_ts_with_the_tsc_fallback_passes";
    let Some(bun) = find_bun() else {
        assert!(
            !ts_required(),
            "TARDIGRADE_REQUIRE_TS=1 but bun is not installed"
        );
        ts_skipped("bun is not installed", test);
        return;
    };
    if usable_checker("/bin/false", "npx -y -p typescript@5.9.3 tsc").is_none() {
        assert!(
            !ts_required(),
            "TARDIGRADE_REQUIRE_TS=1 but the fallback tsc cannot be fetched"
        );
        ts_skipped("the pinned typescript tsc cannot be fetched", test);
        return;
    }
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("agent-loop-ts-fallback");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("mkdir fallback scratch");
    let mut cmd = Command::new("/bin/bash");
    cmd.arg(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/agent-loop-ts.sh"
    ))
    .env("OUT", env!("CARGO_BIN_EXE_tardigrade"))
    .env("WS", root.join("ws.json"))
    .env("BUN", bun)
    .env("TMPDIR", &root)
    // The primary is poisoned through the same pin variable the tests
    // use, so the fallback path and the pin plumbing are both real.
    .env("TARDIGRADE_TSC_RS_PKG", "tsc-rs@0.0.0-no-such-version")
    .env("TARDIGRADE_TSC", "npx -y -p typescript@5.9.3 tsc");
    let out = output_within(cmd, 240);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {stdout} stderr: {stderr}"
    );
    assert!(
        stdout.contains("checker: npx -y -p typescript@5.9.3 tsc"),
        "the fallback that ran must be named, stdout: {stdout}"
    );
    assert!(stdout.lines().any(|l| l == "PASS"), "stdout: {stdout}");
}

/// vm.d.ts pins signatures, not just member names: misusing the guest API
/// fails a real type-check (encodeInto takes a Uint8Array - the runtime
/// throws for other views - and a net handler is (payload, from) => void),
/// while the WebAssembly declarations type-check real use under lib esnext.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn vm_dts_rejects_api_misuse() {
    let test = "vm_dts_rejects_api_misuse";
    if usable_checker(&tsc_rs_cmd(), "tsc").is_none() {
        assert!(
            !ts_required(),
            "TARDIGRADE_REQUIRE_TS=1 but no usable type checker"
        );
        ts_skipped(
            "no usable type checker (npx cannot fetch tsc-rs, and tsc is absent)",
            test,
        );
        return;
    }
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("vm-dts-misuse");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("mkdir misuse scratch");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/examples/vm.d.ts"),
        root.join("vm.d.ts"),
    )
    .expect("copy vm.d.ts");
    std::fs::write(
        root.join("tsconfig.json"),
        r#"{ "compilerOptions": { "strict": true, "noEmit": true, "lib": ["esnext"] }, "include": ["**/*.ts"] }"#,
    )
    .expect("write tsconfig");
    std::fs::write(
        root.join("good.ts"),
        "const bytes = new TextEncoder().encode(\"{}\");\n\
         const mod = new WebAssembly.Module(bytes);\n\
         const valid: boolean = WebAssembly.validate(bytes);\n\
         const mem = new WebAssembly.Memory({ initial: 1 });\n\
         const grown: number = mem.grow(1);\n\
         const buf: ArrayBuffer = mem.buffer;\n\
         const requestResponse: Promise<{ status: number; text: string }> =\n\
           vm.net.request({ toString: () => \"local\" }, 42);\n\
         console.log(valid, grown, buf.byteLength, mod instanceof WebAssembly.Module);\n",
    )
    .expect("write good fixture");
    std::fs::write(
        root.join("bad.ts"),
        "const enc = new TextEncoder();\n\
         enc.encodeInto(\"ab\", new DataView(new ArrayBuffer(8)));\n\
         vm.net.listen(1, (p: number) => p);\n\
         const wrongRequestStatus: string =\n\
           (null as Awaited<ReturnType<typeof vm.net.request>>).status;\n",
    )
    .expect("write bad fixture");

    let checker = usable_checker(&tsc_rs_cmd(), "tsc").expect("gated above");
    let mut parts = checker.split_whitespace();
    let mut cmd = Command::new(parts.next().expect("checker program"));
    cmd.args(parts).args(["-p", "."]).current_dir(&root);
    let out = output_within(cmd, 180);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(2), "misuse must fail: {text}");
    assert!(
        text.contains("bad.ts"),
        "errors must point at bad.ts: {text}"
    );
    assert!(
        text.contains("error TS2345")
            && text.contains("error TS2322")
            && text.matches("error TS").count() >= 3,
        "encodeInto and listen misuse, and a non-number request status, must be type errors: {text}"
    );
    assert!(
        !text.contains("good.ts"),
        "the WebAssembly typing must hold: {text}"
    );
}

/// A workspace file the extractor rejects FAILs the turn with the
/// rejection message only: never a partial tree type-checked into
/// misleading TS2307 errors. Drives the loop's real build_app by sourcing
/// the script (its main() narrative runs only when executed).
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn agent_loop_ts_rejected_extraction_fails_the_turn_cleanly() {
    let test = "agent_loop_ts_rejected_extraction_fails_the_turn_cleanly";
    let Some(bun) = find_bun() else {
        assert!(
            !ts_required(),
            "TARDIGRADE_REQUIRE_TS=1 but bun is not installed"
        );
        ts_skipped("bun is not installed", test);
        return;
    };
    if usable_checker(&tsc_rs_cmd(), "tsc").is_none() {
        assert!(
            !ts_required(),
            "TARDIGRADE_REQUIRE_TS=1 but no usable type checker"
        );
        ts_skipped(
            "no usable type checker (npx cannot fetch tsc-rs, and tsc is absent)",
            test,
        );
        return;
    }
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ts-reject-extract");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("mkdir reject scratch");
    // A space is a legal vm.fs path character and the listing offers the
    // file (it ends in .ts); the extractor's charset is what rejects it.
    seed_workspace(
        &root.join("ws.json"),
        &[
            (
                "/app/app.ts",
                "import { sum } from \"./lib/sum\";\n(globalThis as any).sum = sum;\n",
            ),
            (
                "/app/lib/sum.ts",
                "export const sum = (xs: number[]): number => xs.reduce((a, b) => a + b, 0);\n",
            ),
            ("/app/bad name.ts", "const x: number = 1;\n"),
        ],
    );
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/agent-loop-ts.sh");
    let mut cmd = Command::new("/bin/bash");
    cmd.arg("-c")
        .arg(format!(
            "source '{script}' && if build_app; then echo UNEXPECTED; exit 1; fi"
        ))
        .env("OUT", env!("CARGO_BIN_EXE_tardigrade"))
        .env("WS", root.join("ws.json"))
        .env("BUN", bun)
        .env("TMPDIR", &root)
        .env("TARDIGRADE_TSC_RS_PKG", TSC_RS_PKG);
    let out = output_within(cmd, 120);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0), "the expected FAIL turn: {text}");
    assert!(
        text.contains("FAIL: extraction rejected a workspace file"),
        "the turn must FAIL with the rejection: {text}"
    );
    assert!(
        text.contains("ts-extract: refusing") && text.contains("bad name.ts"),
        "the extractor's message must name the file: {text}"
    );
    assert!(
        !text.contains("error TS"),
        "no partial tree may be type-checked into misleading errors: {text}"
    );
}

/// The bounded runner gives up on readers at the deadline: a descendant
/// that left the process group but still holds the output pipes cannot
/// hang the suite - whatever arrived by the deadline is the output. (The
/// descendant here exits on its own; nothing is signalled.)
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn bounded_runner_gives_up_on_pipes_a_descendant_holds() {
    let mut cmd = Command::new("/bin/bash");
    // The background sleep inherits stdout and holds it open for 3 s
    // after the leader exits at once.
    cmd.arg("-c").arg("sleep 3 >&1 & exit 0");
    let start = std::time::Instant::now();
    let out = output_within(cmd, 1);
    assert_eq!(out.status.code(), Some(0), "the leader exits immediately");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "the drain must give up at the 1 s deadline, took {:?}",
        start.elapsed()
    );
    // The held pipe means no EOF: the reader was still blocked, and the
    // output is what had arrived - nothing.
    assert!(out.stdout.is_empty());
}
