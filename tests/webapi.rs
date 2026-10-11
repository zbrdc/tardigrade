//! End-to-end tests for the web-platform globals: TextEncoder /
//! TextDecoder, atob / btoa, structuredClone, performance.now, and the
//! --seed determinism knob. Every spawned v8vm process is bounded.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(60);

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

/// Spawns v8vm and kills it (failing the test) if it outlives LIMIT, so a
/// hang regression can never stick the suite.
fn v8vm(args: &[&str], stdin: &[u8]) -> Run {
    let mut argv = vec![env!("CARGO_BIN_EXE_v8vm")];
    argv.extend(args.iter().copied());
    spawn(&argv, stdin)
}

fn run_js(name: &str, source: &str) -> Run {
    let path = script(name, source);
    v8vm(&[path.to_str().unwrap()], b"")
}

/// Runs a guest that must exit 0 and print `want` exactly on stdout.
fn ok(name: &str, source: &str, want: &str) {
    let r = run_js(name, source);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, want, "stdout mismatch (stderr: {})", r.stderr);
}

// ---- TextEncoder ----

#[test]
fn text_encoder_encodes_utf8_and_replaces_lone_surrogates() {
    ok(
        "te-encode",
        r#"
        const e = new TextEncoder();
        const bytes = (a) => [...a].join(",");
        console.log(bytes(e.encode("")));
        console.log(bytes(e.encode("hi")));
        console.log(bytes(e.encode("h\u00e9\u{1F600}")));
        // Each unpaired surrogate becomes one U+FFFD (EF BF BD).
        console.log(bytes(e.encode("\uD800")), bytes(e.encode("\uDC00")));
        console.log(e.encode("x").constructor.name);
        // The internal helper is not left on the global object.
        console.log(typeof globalThis.__encodeUtf8, typeof globalThis.__decodeUtf8);
        "#,
        "\n104,105\n104,195,169,240,159,152,128\n239,191,189 239,191,189\nUint8Array\nundefined undefined\n",
    );
}

#[test]
fn text_encoder_encode_into_reports_counts_and_never_splits() {
    ok(
        "te-encode-into",
        r#"
        const e = new TextEncoder();
        const out = (r, u8) => console.log(r.read, r.written, [...u8].join(","));
        // One array each: the destination given to encodeInto is the one
        // printed back, so a lost write cannot hide behind a fresh array.
        const five = new Uint8Array(5);
        out(e.encodeInto("abc", five), five);
        const two = new Uint8Array(2);
        out(e.encodeInto("abc", two), two);
        // A surrogate pair is one code point: three free bytes stop before it.
        const p = new Uint8Array(3);
        out(e.encodeInto("\u{1F600}", p), p);
        // Two ASCII units fit, then the pair no longer does.
        const q = new Uint8Array(3);
        out(e.encodeInto("ab\u{1F600}", q), q);
        // The destination is a window at an offset, not the whole buffer.
        const buf = new ArrayBuffer(8);
        const view = new Uint8Array(buf, 4, 2);
        out(e.encodeInto("abcd", view), new Uint8Array(buf));
        try { e.encodeInto("a", new Int8Array(2)); } catch (err) {
          console.log("bad dest:", err.constructor.name);
        }
        const ab = new ArrayBuffer(4);
        const gone = new Uint8Array(ab);
        if (typeof ab.transfer === "function") {
          ab.transfer();
          // Spreading a detached view throws, so only its slots are read.
          const r = e.encodeInto("ab", gone);
          console.log(r.read, r.written, gone[0]);
        } else {
          console.log(0, 0, "no detach support");
        }
        "#,
        "3 3 97,98,99,0,0\n2 2 97,98\n0 0 0,0,0\n2 2 97,98,0\n2 2 0,0,0,0,97,98,0,0\nbad dest: TypeError\n0 0 undefined\n",
    );
}

// ---- TextDecoder ----

#[test]
fn text_decoder_decodes_and_replaces_invalid_sequences() {
    ok(
        "td-decode",
        r#"
        const d = new TextDecoder();
        console.log(JSON.stringify(d.decode()));
        console.log(JSON.stringify(d.decode(new Uint8Array([104,105]))));
        // E1 80 is a valid prefix cut by C0: two maximal bad subparts.
        console.log(JSON.stringify(d.decode(new Uint8Array([0xE1,0x80,0xC0]))));
        console.log(JSON.stringify(d.decode(new Uint8Array([0xFF]))));
        // A view decodes only its window.
        const buf = new Uint8Array([97,98,99,100]);
        console.log(JSON.stringify(d.decode(new Uint8Array(buf.buffer, 1, 2))));
        console.log(JSON.stringify(d.decode(buf.buffer)));
        "#,
        "\"\"\n\"hi\"\n\"\u{fffd}\u{fffd}\"\n\"\u{fffd}\"\n\"bc\"\n\"abcd\"\n",
    );
}

#[test]
fn text_decoder_fatal_throws_on_invalid_utf8() {
    ok(
        "td-fatal",
        r#"
        const d = new TextDecoder("utf-8", {fatal: true});
        console.log(JSON.stringify(d.decode(new Uint8Array([104,105]))));
        try { d.decode(new Uint8Array([0xC0,0xAF])); } catch (err) {
          console.log(err.constructor.name, err.message);
        }
        "#,
        "\"hi\"\nTypeError The encoded data was not valid for encoding utf-8\n",
    );
}

#[test]
fn text_decoder_handles_boms() {
    ok(
        "td-bom",
        r#"
        console.log(JSON.stringify(new TextDecoder().decode(new Uint8Array([0xEF,0xBB,0xBF,97]))));
        console.log(JSON.stringify(new TextDecoder("utf-8", {ignoreBOM: true}).decode(new Uint8Array([0xEF,0xBB,0xBF,97]))));
        // Only a leading BOM is stripped; mid-stream U+FEFF is data.
        console.log(JSON.stringify(new TextDecoder().decode(new Uint8Array([97,0xEF,0xBB,0xBF]))));
        "#,
        "\"a\"\n\"\u{feff}a\"\n\"a\u{feff}\"\n",
    );
}

#[test]
fn text_decoder_streams_across_calls() {
    ok(
        "td-stream",
        r#"
        const d = new TextDecoder();
        const parts = [[0x61,0xF0,0x9F], [0x98], [0x80,0x62]];
        let s = "";
        for (const p of parts) s += d.decode(new Uint8Array(p), {stream: true});
        s += d.decode();
        console.log(JSON.stringify(s));
        // A BOM split across stream chunks is still the leading BOM.
        const b = new TextDecoder();
        console.log(JSON.stringify(b.decode(new Uint8Array([0xEF]), {stream: true})),
                    JSON.stringify(b.decode(new Uint8Array([0xBB,0xBF,97]))));
        // A lone BOM prefix flushed at the end is ordinary (bad) input.
        const c = new TextDecoder();
        console.log(JSON.stringify(c.decode(new Uint8Array([0xEF]), {stream: true}) + c.decode()));
        "#,
        "\"a\u{1f600}b\"\n\"\" \"a\"\n\"\u{fffd}\"\n",
    );
}

#[test]
fn text_decoder_rejects_other_labels_and_bad_input() {
    ok(
        "td-labels",
        r#"
        console.log(new TextDecoder().encoding, new TextDecoder("UTF8").encoding,
                    new TextDecoder("unicode-1-1-utf-8").encoding);
        console.log(new TextDecoder("utf-8", {fatal: true}).fatal,
                    new TextDecoder().fatal, new TextDecoder().ignoreBOM);
        for (const label of ["latin1", "utf-16", ""]) {
          try { new TextDecoder(label); } catch (err) {
            console.log(label === "" ? "empty" : label, err.constructor.name);
          }
        }
        try { new TextDecoder().decode("text"); } catch (err) {
          console.log("input:", err.constructor.name);
        }
        "#,
        "utf-8 utf-8 utf-8\ntrue false false\nlatin1 RangeError\nutf-16 RangeError\nempty RangeError\ninput: TypeError\n",
    );
}

// ---- atob / btoa ----

#[test]
fn atob_and_btoa_round_trip_latin1_bytes() {
    ok(
        "b64-roundtrip",
        r#"
        for (const s of ["", "f", "fo", "foo", "foob", "fooba", "foobar"]) {
          console.log(JSON.stringify(atob(btoa(s))), JSON.stringify(btoa(s)));
        }
        // Every Latin-1 byte survives.
        let all = "";
        for (let i = 0; i < 256; i++) all += String.fromCharCode(i);
        console.log(all === atob(btoa(all)), all.length);
        "#,
        "\"\" \"\"\n\"f\" \"Zg==\"\n\"fo\" \"Zm8=\"\n\"foo\" \"Zm9v\"\n\"foob\" \"Zm9vYg==\"\n\"fooba\" \"Zm9vYmE=\"\n\"foobar\" \"Zm9vYmFy\"\ntrue 256\n",
    );
}

#[test]
fn atob_and_btoa_fail_like_browsers() {
    ok(
        "b64-failures",
        r#"
        console.log(JSON.stringify(atob(" Q U\n==")), JSON.stringify(atob("QR==")));
        for (const bad of ["a", "====", "A===", "Q==", "AB=C", "A!BC"]) {
          try { atob(bad); console.log(bad, "decoded"); } catch (err) {
            console.log(err.name);
          }
        }
        for (const bad of ["\u0100", "\u{1F600}", "\uD800"]) {
          try { btoa(bad); } catch (err) { console.log(err.name); }
        }
        "#,
        "\"A\" \"A\"\nInvalidCharacterError\nInvalidCharacterError\nInvalidCharacterError\nInvalidCharacterError\nInvalidCharacterError\nInvalidCharacterError\nInvalidCharacterError\nInvalidCharacterError\nInvalidCharacterError\n",
    );
}

// ---- structuredClone ----

#[test]
fn structured_clone_round_trips_builtins_and_cycles() {
    ok(
        "sc-builtins",
        r#"
        const shared = { id: 1 };
        const orig = {
          map: new Map([[1, "a"], [true, shared]]),
          set: new Set(["x", 2]),
          date: new Date(1234567890),
          re: /ab+c/gi,
          u8: new Uint8Array([9, 8, 7]),
          f64: new Float64Array([1.5]),
          buf: new ArrayBuffer(4),
          arr: [1, "two", null],
          nested: { deep: { deeper: true } },
        };
        const c = structuredClone(orig);
        console.log(c.map.get(1), c.map.get(true) === shared, c.map.size);
        console.log([...c.set].join("|"), c.date.getTime());
        console.log(c.re.source, c.re.flags, c.re instanceof RegExp);
        console.log(c.u8.constructor.name, [...c.u8].join(","), c.u8.byteOffset);
        console.log([...c.f64].join(","), c.buf.byteLength);
        console.log(JSON.stringify(c.arr), c.nested.deep.deeper);
        // Deep: mutations never reach the original.
        c.map.set(2, "new");
        c.nested.deep.deeper = false;
        c.u8[0] = 0;
        console.log(orig.map.size, orig.nested.deep.deeper, orig.u8[0]);
        // Shared references stay shared inside the clone (one copy each).
        const orig2 = { a: shared, b: shared, v: new Uint8Array(new ArrayBuffer(6), 2, 3) };
        const c2 = structuredClone(orig2);
        console.log(c2.a === c2.b, c2.a.id, c2.v.byteOffset, c2.v.byteLength);
        // Cycles clone without recursing forever, and identity holds.
        const cyc = { name: "cyc" };
        cyc.self = cyc;
        const cc = structuredClone(cyc);
        console.log(cc.self === cc, cc === cyc, cc.name);
        "#,
        "a false 2\nx|2 1234567890\nab+c gi true\nUint8Array 9,8,7 0\n1.5 4\n[1,\"two\",null] true\n2 true 9\ntrue 1 2 3\ntrue false cyc\n",
    );
}

#[test]
fn structured_clone_refuses_functions_symbols_and_transfer() {
    ok(
        "sc-refuses",
        r#"
        for (const bad of [() => 1, function f() {}, Symbol("s"), { fn: () => 1 }]) {
          try { structuredClone(bad); console.log("cloned"); } catch (err) {
            console.log(err.name, err instanceof Error, err instanceof TypeError);
          }
        }
        const ab = new ArrayBuffer(8);
        try { structuredClone(ab, { transfer: [ab] }); } catch (err) {
          console.log("transfer:", err.name);
        }
        // Symbol-keyed properties are dropped (the structured clone
        // algorithm skips them); symbol VALUES refuse, string keys survive.
        console.log(structuredClone({ [Symbol.for("k")]: "v", k: 1 })[Symbol.for("k")]);
        console.log(structuredClone({ k: 1 }).k);
        "#,
        "DataCloneError true false\nDataCloneError true false\nDataCloneError true false\nDataCloneError true false\ntransfer: DataCloneError\nundefined\n1\n",
    );
}

// ---- performance.now ----

#[test]
fn performance_now_is_monotonic() {
    ok(
        "perf-now",
        r#"
        const a = performance.now();
        let s = 0;
        for (let i = 0; i < 5e7; i++) s += i;
        const b = performance.now();
        const c = performance.now();
        console.log(typeof a, a >= 0, b > a, c >= b, s > 0);
        "#,
        "number true true true true\n",
    );
}

// ---- --seed ----

const DRAW_SEQ: &str =
    "console.log([0,0,0,0,0].map(() => Math.random().toFixed(17)).join(\"\\n\"));";

#[test]
fn same_seed_prints_identical_sequences_in_two_processes() {
    let path = script("seed-seq", DRAW_SEQ);
    let a = v8vm(&["--seed=1234567890", path.to_str().unwrap()], b"");
    let b = v8vm(&["--seed=1234567890", path.to_str().unwrap()], b"");
    let c = v8vm(&["--seed=987654321", path.to_str().unwrap()], b"");
    assert_eq!(a.code, 0, "stderr: {}", a.stderr);
    assert_eq!(b.code, 0, "stderr: {}", b.stderr);
    assert_eq!(c.code, 0, "stderr: {}", c.stderr);
    assert_eq!(a.stdout.lines().count(), 5, "five draws: {}", a.stdout);
    assert_eq!(a.stdout, b.stdout, "same seed must reproduce");
    assert_ne!(a.stdout, c.stdout, "different seeds must differ");
    // All draws are in [0, 1).
    for line in a.stdout.lines() {
        let v: f64 = line.parse().expect("draw is a number");
        assert!((0.0..1.0).contains(&v), "draw out of range: {}", v);
    }
}

#[test]
fn seed_zero_is_a_seed_and_bad_seeds_are_usage_errors() {
    let path = script("seed-zero", DRAW_SEQ);
    let a = v8vm(&["--seed=0", path.to_str().unwrap()], b"");
    let b = v8vm(&["--seed=0", path.to_str().unwrap()], b"");
    let unseeded = v8vm(&[path.to_str().unwrap()], b"");
    assert_eq!(a.code, 0, "stderr: {}", a.stderr);
    assert_eq!(a.stdout, b.stdout);
    assert_eq!(unseeded.code, 0, "unseeded runs keep V8's Math.random");
    let bad = v8vm(&["--seed=oops", path.to_str().unwrap()], b"");
    assert_eq!(bad.code, 2);
    assert!(bad.stderr.contains("bad --seed"), "stderr: {}", bad.stderr);
}

/// Spawns v8vm with RLIMIT_AS set to `as_bytes` bytes in a pre_exec hook
/// (the repo's file-size-cap pattern): no external prlimit/timeout on
/// PATH, and the same 60 s kill bound as every other spawn applies to
/// the child itself.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn v8vm_capped(as_bytes: u64, args: &[&str], stdin: &[u8]) -> Run {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_v8vm"));
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: the closure only makes async-signal-safe libc calls.
    unsafe {
        cmd.pre_exec(move || {
            let cap = libc::rlimit {
                rlim_cur: as_bytes,
                rlim_max: as_bytes,
            };
            if libc::setrlimit(libc::RLIMIT_AS, &cap) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().expect("spawn capped v8vm");
    finish(child, stdin)
}

/// The spawn/wait/kill core of `v8vm`, for any command line.
fn spawn(argv: &[&str], stdin: &[u8]) -> Run {
    let child = Command::new(argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn test command");
    finish(child, stdin)
}

/// Feeds stdin, waits with the shared 60 s kill bound, collects output.
fn finish(mut child: std::process::Child, stdin: &[u8]) -> Run {
    let mut pipe = child.stdin.take().unwrap();
    let input = stdin.to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = pipe.write_all(&input);
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll test command") {
            break status;
        }
        if start.elapsed() > LIMIT {
            child.kill().expect("kill hung test command");
            child.wait().expect("reap test command");
            panic!("test command still running after {:?}", LIMIT);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    feeder.join().unwrap();
    let out = child
        .wait_with_output()
        .expect("collect test command output");
    Run {
        code: status.code().expect("exit code, not a signal"),
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
    }
}

// ---- decoder stream state, validation, and memory behavior ----

#[test]
fn text_decoder_resets_bom_and_carry_after_a_flushing_decode() {
    ok(
        "td-bom-reset",
        r#"
        const d = new TextDecoder();
        console.log(JSON.stringify(d.decode(new Uint8Array([97]))));
        // A non-stream decode ends the stream: the next call starts fresh,
        // so a leading BOM goes (Node prints "b" here too).
        console.log(JSON.stringify(d.decode(new Uint8Array([0xEF,0xBB,0xBF,98]))));
        const e = new TextDecoder();
        e.decode(new Uint8Array(0), {stream:true});
        console.log(JSON.stringify(e.decode(new Uint8Array([0xEF,0xBB,0xBF,97]))));
        // A throwing (fatal) decode resets as well.
        const f = new TextDecoder("utf-8", {fatal:true});
        try { f.decode(new Uint8Array([0xEF,0xBB,0xBF,0xFF])); } catch (err) { console.log(err.name); }
        console.log(JSON.stringify(f.decode(new Uint8Array([0xEF,0xBB,0xBF,97]))));
        // A stream sequence still flushes its carry at the final call.
        const s = new TextDecoder();
        console.log(JSON.stringify(s.decode(new Uint8Array([0xF0,0x9F,0x98]), {stream:true})));
        console.log(JSON.stringify(s.decode(new Uint8Array([0x80]))));
        "#,
        "\"a\"\n\"b\"\n\"a\"\nTypeError\n\"a\"\n\"\"\n\"\u{1f600}\"\n",
    );
}

#[test]
fn text_decoder_trims_labels_and_treats_null_options_as_empty() {
    ok(
        "td-label-trim",
        r#"
        console.log(new TextDecoder(" utf-8 ").encoding, new TextDecoder("\tutf8\n").encoding);
        console.log(new TextDecoder("utf-8", null).fatal, new TextDecoder("utf-8").fatal);
        for (const bad of [1, "x"]) {
          try { new TextDecoder("utf-8", bad); } catch (err) { console.log("ctor:", err.constructor.name); }
        }
        try { new TextDecoder("utf-8", { get fatal() { throw new Error("g"); } }); }
        catch (err) { console.log("getter:", err.message); }
        const d = new TextDecoder();
        console.log(JSON.stringify(d.decode(new Uint8Array([97]), null)));
        try { d.decode(new Uint8Array([97]), 5); } catch (err) { console.log("decode opts:", err.constructor.name); }
        "#,
        "utf-8 utf-8\nfalse false\nctor: TypeError\nctor: TypeError\ngetter: g\n\"a\"\ndecode opts: TypeError\n",
    );
}

#[test]
fn decoder_and_encoder_state_is_private_and_tagged() {
    ok(
        "webapi-hygiene",
        r#"
        const d = new TextDecoder("utf-8", {fatal: true});
        console.log(JSON.stringify(Object.keys(d)), JSON.stringify(Object.getOwnPropertyNames(d)));
        // The old internal names are inert now.
        d.__carry = "garbage"; d.__bom = 1;
        console.log(JSON.stringify(d.decode(new Uint8Array([0xEF,0xBB,0xBF,97]))));
        const e = new TextEncoder();
        console.log(e.encoding, new TextDecoder().encoding, Object.keys(e).length);
        console.log(Object.prototype.toString.call(e), Object.prototype.toString.call(d));
        console.log(d.fatal, d.ignoreBOM);
        class Sub extends TextDecoder { }
        console.log(JSON.stringify(new Sub().decode(new Uint8Array([104]))));
        try { TextDecoder("utf-8"); } catch (err) { console.log("no new:", err.constructor.name); }
        "#,
        "[] []\n\"a\"\nutf-8 utf-8 0\n[object TextEncoder] [object TextDecoder]\ntrue false\n\"h\"\nno new: TypeError\n",
    );
}

#[test]
fn atob_btoa_and_structured_clone_require_an_argument() {
    ok(
        "webapi-no-arg",
        r#"
        try { atob(); } catch (err) { console.log(err.constructor.name, err.message); }
        try { btoa(); } catch (err) { console.log(err.constructor.name, err.message); }
        try { structuredClone(); } catch (err) { console.log(err.constructor.name, err.message); }
        "#,
        "TypeError The \"input\" argument must be specified\nTypeError The \"input\" argument must be specified\nTypeError structuredClone requires a value argument\n",
    );
}

#[test]
fn structured_clone_validates_options_and_rejects_shared_buffers() {
    ok(
        "sc-options",
        r#"
        console.log(structuredClone(1, null), structuredClone(1, undefined), structuredClone(1, {}));
        for (const bad of [2, "x", true]) {
          try { structuredClone(1, bad); } catch (err) { console.log("opts:", err.constructor.name); }
        }
        try { structuredClone(1, { transfer: {} }); } catch (err) { console.log("transfer:", err.constructor.name); }
        console.log(structuredClone(1, { transfer: [] }));
        try { structuredClone(1, { get transfer() { throw new Error("tg"); } }); }
        catch (err) { console.log("getter:", err.message); }
        try { structuredClone(new SharedArrayBuffer(8)); } catch (err) { console.log("sab:", err.name); }
        const sab = new SharedArrayBuffer(8);
        try { structuredClone(new Uint8Array(sab)); } catch (err) { console.log("sab view:", err.name); }
        "#,
        "1 1 1\nopts: TypeError\nopts: TypeError\nopts: TypeError\ntransfer: TypeError\n1\ngetter: tg\nsab: DataCloneError\nsab view: DataCloneError\n",
    );
}

/// Memory-heavy (a 300 MB guest): run with `make test-heavy`.
#[ignore]
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn decode_of_a_huge_buffer_under_memory_pressure_throws_instead_of_aborting() {
    // 300 MB of ASCII under a 2 GB address-space cap: the input buffer
    // exists, but the Rust output reserve cannot. The fallible path turns
    // that into a guest RangeError; the old path aborted the process. The
    // cap is raised so the 300 MB input fits the ArrayBuffer budget.
    let path = script(
        "decode-pressure",
        r#"const u = new Uint8Array(300 * 1048576).fill(65);
           try { new TextDecoder().decode(u); console.log("decoded"); }
           catch (e) { console.log(e.name, e.message); }"#,
    );
    let r = v8vm_capped(
        2_000_000_000,
        &["--memory=400", path.to_str().unwrap()],
        b"",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout, "RangeError decoded text too large\n",
        "stderr: {}",
        r.stderr
    );
}

/// Memory-heavy (a 420 MB string): run with `make test-heavy`.
#[ignore]
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn btoa_past_the_string_limit_throws_range_error() {
    // 420 MB of "a" encodes to 560 MB of base64, past V8's ~536 M-char
    // string limit: RangeError, not an empty string or an abort.
    let path = script(
        "btoa-huge",
        r#"try { btoa("a".repeat(420 * 1048576)); console.log("no throw"); }
           catch (e) { console.log(e.name, e.message); }"#,
    );
    let r = v8vm_capped(
        3_000_000_000,
        &["--memory=800", path.to_str().unwrap()],
        b"",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(r.stdout.starts_with("RangeError"), "stdout: {}", r.stdout);
}

#[test]
fn performance_now_starts_near_zero_each_run() {
    ok(
        "perf-origin",
        r#"
        const a = performance.now();
        console.log(a >= 0 && a < 5000, typeof performance.timeOrigin);
        let b = 0; for (let i = 0; i < 1e6; i++) b += i;
        const c = performance.now();
        console.log(c >= a, b > 0);
        "#,
        "true undefined\ntrue true\n",
    );
}

// ---- piece boundaries, option getters, and global hygiene ----

#[test]
fn decode_crosses_the_piece_boundary_with_multibyte_and_boms() {
    // 128 KiB of ASCII with a multibyte sequence set at every offset
    // around the 64 KiB piece seam, plus BOMs placed at and across it;
    // streamed and unstreamed results must equal the built-from-parts
    // string, which is what the platform decodes these bytes to.
    ok(
        "td-piece-boundary",
        r#"
        const N = 131072;
        const seqs = {
          "\u{1f600}": [0xF0,0x9F,0x98,0x80],
          "\u{20ac}": [0xE2,0x82,0xAC],
          "\u{fffd}": [0xF0,0x9F,0x98],        // cut by the next 0x41
          "\u{fffd}\u{fffd}": [0xE1,0x80,0xC0],
          "\u{feff}": [0xEF,0xBB,0xBF],        // mid-stream BOM survives
        };
        const check = (bytes, expected, note) => {
          const d = new TextDecoder();
          if (d.decode(bytes) !== expected) { console.log("FAIL", note); vm.exit(1); }
          const s = new TextDecoder();
          const mid = 65536;
          const streamed = s.decode(bytes.subarray(0, mid), {stream: true})
                         + s.decode(bytes.subarray(mid), {stream: true})
                         + s.decode();
          if (streamed !== expected) { console.log("FAIL stream", note); vm.exit(1); }
        };
        for (const [form, seq] of Object.entries(seqs)) {
          for (const off of [65533, 65534, 65535, 65536, 65537]) {
            const v = new Uint8Array(N).fill(0x41);
            v.set(seq, off);
            const expected = "A".repeat(off) + form + "A".repeat(N - off - seq.length);
            check(v, expected, form + "@" + off);
          }
        }
        // A leading BOM split by the seam is still stripped.
        const b = new Uint8Array(N).fill(0x41);
        b.set([0xEF, 0xBB, 0xBF], 0);
        for (const mid of [1, 2]) {
          const s2 = new TextDecoder();
          const out = s2.decode(b.subarray(0, mid), {stream: true})
                    + s2.decode(b.subarray(mid), {stream: true})
                    + s2.decode();
          if (out !== "A".repeat(N - 3)) { console.log("FAIL bom split", mid); vm.exit(1); }
        }
        console.log("bom split ok");
        "#,
        "bom split ok\n",
    );
}

#[test]
fn decode_input_shrunk_or_detached_by_an_options_getter_reads_the_new_shape() {
    ok(
        "td-opt-getter-mutation",
        r#"
        const d = new TextDecoder();
        // Length-tracking view over a resizable buffer: the getter shrinks
        // it, the decode sees four bytes (Node prints "AAAA").
        const rab = new ArrayBuffer(16, { maxByteLength: 1 << 20 });
        const u = new Uint8Array(rab); u.fill(65);
        console.log(JSON.stringify(d.decode(u, { get stream() { rab.resize(4); return false; } })));
        // A fixed view left out of bounds by a shrink reads as empty.
        const rab2 = new ArrayBuffer(16, { maxByteLength: 64 });
        const v = new Uint8Array(rab2, 2, 10); v.fill(66);
        console.log(JSON.stringify(d.decode(v, { get stream() { rab2.resize(5); return false; } })));
        // Detached mid-options: empty, and later decodes still work.
        const ab = new ArrayBuffer(8); new Uint8Array(ab).fill(67);
        console.log(JSON.stringify(d.decode(new Uint8Array(ab), { get stream() { ab.transfer(); return false; } })));
        console.log(JSON.stringify(d.decode(ab)));
        try { d.decode(new Uint8Array(ab)); } catch (err) { console.log("view:", err.constructor.name); }
        // A raw SharedArrayBuffer is a BufferSource, like in browsers.
        console.log(JSON.stringify(d.decode(new SharedArrayBuffer(2))));
        // A bad input mid-stream throws before touching the state: the
        // pending BOM still strips on the next call.
        const e = new TextDecoder();
        e.decode(new Uint8Array([0xEF]), { stream: true });
        try { e.decode("text"); } catch (err) { console.log(err.constructor.name); }
        console.log(JSON.stringify(e.decode(new Uint8Array([0xBB, 0xBF, 97]))));
        // Options convert before the input type is checked: the stream
        // getter runs and the input TypeError still wins.
        const f = new TextDecoder();
        const order = [];
        try { f.decode({ get length() { order.push("len"); return 0; } },
                       { get stream() { order.push("stream"); return false; } }); }
        catch (err) { console.log(err.constructor.name, order.join(",")); }
        "#,
        "\"AAAA\"\n\"\"\n\"\"\n\"\"\nview: TypeError\n\"\\u0000\\u0000\"\nTypeError\n\"a\"\nTypeError stream\n",
    );
}

#[test]
fn decoder_options_labels_and_types_convert_like_webidl() {
    ok(
        "td-webidl-conversion",
        r#"
        // Function options are callable objects: valid dictionaries.
        console.log(new TextDecoder("utf-8", function () {}).fatal,
                    new TextDecoder().decode(new Uint8Array([97]), function () {}));
        // A symbol label cannot convert to a string.
        try { new TextDecoder(Symbol()); } catch (err) { console.log(err.constructor.name); }
        // stream converts as a WebIDL boolean, not by identity.
        const d = new TextDecoder();
        console.log(JSON.stringify(d.decode(new Uint8Array([0xF0]), { stream: 1 })),
                    JSON.stringify(d.decode()));
        const e = new TextDecoder();
        console.log(JSON.stringify(e.decode(new Uint8Array([0xF0]), { stream: {} })),
                    JSON.stringify(e.decode()));
        // A label converts with the ToString hint: toString beats valueOf.
        console.log(new TextDecoder({ valueOf() { return "x"; }, toString() { return "utf-8"; } }).encoding);
        // The constant encoding getters brand-check their receiver.
        for (const C of [TextEncoder, TextDecoder]) {
          const get = Object.getOwnPropertyDescriptor(C.prototype, "encoding").get;
          try { get.call({}); console.log("no brand"); } catch (err) { console.log(err.constructor.name); }
        }
        "#,
        "false a\nTypeError\n\"\" \"\u{fffd}\"\n\"\" \"\u{fffd}\"\nutf-8\nTypeError\nTypeError\n",
    );
}

#[test]
fn a_getter_error_during_clone_keeps_its_identity() {
    ok(
        "sc-getter-error-identity",
        r#"
        const show = (tag, f) => { try { f(); console.log(tag, "no throw"); }
                                  catch (e) { console.log(tag, e && e.name, e instanceof Error); } };
        show("plain", () => structuredClone({ get a() { throw new Error("x could not be cloned"); } }));
        show("custom", () => { const mine = new Error("boom"); mine.name = "Custom";
                               try { structuredClone({ get a() { throw mine; } }); }
                               catch (e) { console.log("inner", e.name, e === mine); } });
        const frozen = Object.freeze(new Error("frozen could not be cloned"));
        show("frozen", () => structuredClone({ get a() { throw frozen; } }));
        console.log("kept", frozen.name, Object.keys(frozen).join(","));
        // The SAB refusal itself is still named.
        try { structuredClone(new SharedArrayBuffer(4)); } catch (e) { console.log("sab", e.name); }
        "#,
        "plain Error true\ninner Custom true\ncustom no throw\nfrozen Error true\nkept Error \nsab DataCloneError\n",
    );
}

#[test]
fn overridden_globals_do_not_reach_the_decoder_internals() {
    ok(
        "td-captured-intrinsics",
        r#"
        const Real = Uint8Array;
        globalThis.Uint8Array = function () { throw new Error("hijacked"); };
        const d = new TextDecoder();
        const e = new TextEncoder();
        globalThis.Uint8Array = Real;
        console.log(JSON.stringify(d.decode(new Real([97]))), e.encode("h").length);
        // A bad BufferSource still throws after the globals were swapped.
        try { d.decode("text"); } catch (err) { console.log(err.constructor.name); }
        "#,
        "\"a\" 1\nTypeError\n",
    );
}

#[test]
fn encode_into_requires_a_string_source() {
    ok(
        "te-encode-into-src",
        r#"
        for (const bad of [12345, null, {}, [], new Number(5)]) {
          try { new TextEncoder().encodeInto(bad, new Uint8Array(4)); console.log("no throw"); }
          catch (err) { console.log(err.constructor.name); }
        }
        "#,
        "TypeError\nTypeError\nTypeError\nTypeError\nTypeError\n",
    );
}

/// Memory-heavy (a 300 MB string): run with `make test-heavy`.
#[ignore]
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn atob_under_memory_pressure_throws_instead_of_aborting() {
    // 300 MB of valid base64 under a 2.2 GB cap with room for the V8
    // heap: the streaming decoder walks the string's own units, so the
    // only Rust allocation is the fallible output.
    let path = script(
        "atob-pressure",
        r#"const s = "A".repeat(300 * 1048576);
           try { console.log("atob", atob(s).length); }
           catch (e) { console.log("caught", e.name, e.message); }"#,
    );
    let r = v8vm_capped(
        2_200_000_000,
        &["--memory=1024", path.to_str().unwrap()],
        b"",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stdout.starts_with("atob") || r.stdout.starts_with("caught RangeError"),
        "stdout: {}",
        r.stdout
    );
}

/// Memory-heavy (a 300 MB buffer): run with `make test-heavy`.
#[ignore]
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn cloning_a_buffer_beyond_memory_reports_data_clone_error() {
    // V8's own wire allocation fails; it raises the clone error through
    // the delegate, so the guest sees DataCloneError, not a truncation
    // or an abort.
    let path = script(
        "clone-pressure",
        r#"const a = new ArrayBuffer(300 * 1048576); new Uint8Array(a).fill(1);
           try { const c = structuredClone(a); console.log("clone", c.byteLength); }
           catch (e) { console.log("caught", e.name, e.message); }"#,
    );
    let r = v8vm_capped(
        2_200_000_000,
        &["--memory=1024", path.to_str().unwrap()],
        b"",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stdout.starts_with("caught DataCloneError"),
        "stdout: {}",
        r.stdout
    );
}

#[test]
fn encoding_past_the_buffer_budget_names_the_allocation() {
    // The budget refusal message matches vm.fs.readBytes's, so every
    // budget refusal reads alike (it used to say "encoded data too large").
    let path = script(
        "encode-budget",
        r#"try { new TextEncoder().encode("x".repeat(9 * 1048576)); console.log("encoded"); }
           catch (e) { console.log("caught", e.name, e.message); }"#,
    );
    let r = v8vm(&["--memory=8", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout, "caught RangeError Array buffer allocation failed\n",
        "stderr: {}",
        r.stderr
    );
}

#[test]
fn deep_values_stack_overflow_keeps_its_range_error() {
    // Regression: deserialize_failure used to rename every native Error
    // to DataCloneError, so a nesting depth that overflows V8's
    // deserializer recursion surfaced as "DataCloneError: Maximum call
    // stack size exceeded". Only the budget refusal below is a
    // DataCloneError; V8's own stack-overflow RangeError keeps its
    // identity. (Past roughly depth 4000 the serializer overflows
    // first, which was always kept; where exactly a given depth lands
    // depends on the thread stack, so both non-renamed outcomes pass.)
    let path = script(
        "clone-depth",
        r#"let v = { leaf: 1 };
           for (let i = 0; i < 3000; i++) v = { a: v };
           try { structuredClone(v); console.log("cloned"); }
           catch (e) { console.log("caught", e.name, e.message); }"#,
    );
    let r = v8vm(&[path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stdout == "cloned\n" || r.stdout.starts_with("caught RangeError"),
        "the stack-overflow error must keep its identity, stdout: {}",
        r.stdout
    );
}

#[test]
fn cloning_past_the_buffer_budget_is_a_data_clone_error() {
    // The rename that must still happen: the run's ArrayBuffer budget
    // refuses the clone's copy of a live 7 MB buffer under an 8 MB cap,
    // and that refusal - not V8's own errors - is the DataCloneError.
    let path = script(
        "clone-budget",
        r#"const a = new ArrayBuffer(7 * 1048576);
           try { structuredClone(a); console.log("cloned"); }
           catch (e) { console.log("caught", e.name, e.message); }"#,
    );
    let r = v8vm(&["--memory=8", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stdout.starts_with("caught DataCloneError"),
        "a budget refusal is the one clone failure that is a DataCloneError, stdout: {}",
        r.stdout
    );
}

#[test]
fn every_intrinsic_the_decoder_uses_survives_being_replaced() {
    // The ov2 probe shape: swap each intrinsic the prelude classes touch
    // for a throwing boom, run the whole decoder surface, restore, and
    // require the same results as with the real ones.
    ok(
        "td-intrinsic-overrides",
        r#"
        const DP = Object.defineProperty, GOPD = Object.getOwnPropertyDescriptor;
        const boom = function () { throw new Error("HIJACKED"); };
        const R = Uint8Array;
        const d = new TextDecoder();
        const df = new TextDecoder("utf-8", { fatal: true });
        const e = new TextEncoder();
        const bytes = new R([0xF0, 0x9F, 0x98]);
        const rest = new R([0x80, 0x41]);
        const bad = new R([0xFF]);
        const ascii = new R([104, 105]);
        const targets = [
          [globalThis, "Uint8Array"], [globalThis, "ArrayBuffer"], [globalThis, "SharedArrayBuffer"],
          [globalThis, "String"], [globalThis, "TypeError"], [globalThis, "RangeError"],
          [globalThis, "Symbol"], [globalThis, "Object"], [globalThis, "Array"], [globalThis, "Set"],
          [Object, "defineProperty"], [Object, "getOwnPropertyNames"],
          [Object, "getOwnPropertyDescriptor"], [Array, "isArray"], [ArrayBuffer, "isView"],
          [String.prototype, "replace"], [String.prototype, "toLowerCase"], [Set.prototype, "has"],
        ];
        const run = () => {
          const out = [];
          const t = (n, f) => { try { out.push(n + "=" + JSON.stringify(f())); }
                               catch (x) { out.push(n + "!" + (x && x.message)); } };
          t("s1", () => d.decode(bytes, { stream: true }));
          t("s2", () => d.decode(rest));
          t("fatal", () => df.decode(bad));
          t("badinput", () => d.decode("text"));
          t("enc", () => e.encode("h").length);
          t("encInto", () => { const u = new R(4); return e.encodeInto("ab", u).written; });
          t("ctor", () => new TextDecoder("utf-8").encoding);
          t("lbl", () => new TextDecoder("latin1"));
          return out.join(" ");
        };
        const base = run();
        if (base.includes("HIJACKED")) { console.log("baseline broken:", base); vm.exit(1); }
        for (const [o, k] of targets) {
          const desc = GOPD(o, k);
          DP(o, k, { value: boom, configurable: true, writable: true });
          const now = run();
          if (desc) DP(o, k, desc); else delete o[k];
          if (now !== base) { console.log("DIFFERS under", k, ":", now, "|", base); vm.exit(1); }
        }
        console.log("stable", base.length > 0);
        "#,
        "stable true\n",
    );
}

#[test]
fn atob_decodes_unpadded_input() {
    ok(
        "atob-unpadded",
        r#"
        console.log(JSON.stringify(atob("QQ")), JSON.stringify(atob("QUI")));
        console.log(JSON.stringify(atob("AA")), JSON.stringify(atob("AAA")));
        console.log(JSON.stringify(atob(" Q Q ")));
        const e = new TextEncoder();
        // An unpadded round trip: strip the padding, decode, compare bytes.
        for (const s of ["f", "fo", "foo", "foob"]) {
          const b64 = btoa(s).replace(/=+$/, "");
          if (Buffer_check(b64, s)) console.log(s, b64);
        }
        function Buffer_check(b64, s) {
          const bytes = atob(b64);
          const want = e.encode(s);
          if (bytes.length !== want.length) return false;
          for (let i = 0; i < bytes.length; i++)
            if (bytes.charCodeAt(i) !== want[i]) return false;
          return true;
        }
        "#,
        "\"A\" \"AB\"\n\"\\u0000\" \"\\u0000\\u0000\"\n\"A\"\nf Zg\nfo Zm8\nfoo Zm9v\nfoob Zm9vYg\n",
    );
}

/// Memory-heavy (a 300 MB string): run with `make test-heavy`. A
/// malformed unit first: pass 1 rejects it before any allocation, so
/// even under a tight address-space cap the guest sees
/// InvalidCharacterError, never a RangeError or an abort.
#[ignore]
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn atob_with_a_bad_first_unit_fails_before_allocating() {
    let path = script(
        "atob-bad-first",
        r#"const s = "!" + "A".repeat(300 * 1048576);
           try { console.log("atob", atob(s).length); }
           catch (e) { console.log("caught", e.name, e.message); }"#,
    );
    let r = v8vm_capped(
        1_800_000_000,
        &["--memory=1024", path.to_str().unwrap()],
        b"",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stdout.starts_with("caught InvalidCharacterError"),
        "stdout: {}",
        r.stdout
    );
}

/// Memory-heavy (a 300 MB string): run with `make test-heavy`. The same
/// holds when the malformed unit is the last one.
#[ignore]
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn atob_with_a_bad_last_unit_fails_before_allocating() {
    let path = script(
        "atob-bad-last",
        r#"const s = "A".repeat(300 * 1048576) + "!";
           try { console.log("atob", atob(s).length); }
           catch (e) { console.log("caught", e.name, e.message); }"#,
    );
    let r = v8vm_capped(
        1_800_000_000,
        &["--memory=1024", path.to_str().unwrap()],
        b"",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stdout.starts_with("caught InvalidCharacterError"),
        "stdout: {}",
        r.stdout
    );
}
