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
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll v8vm") {
            break status;
        }
        if start.elapsed() > LIMIT {
            child.kill().expect("kill hung v8vm");
            child.wait().expect("reap v8vm");
            panic!("v8vm still running after {:?}", LIMIT);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    feeder.join().unwrap();
    let out = child.wait_with_output().expect("collect v8vm output");
    Run {
        code: status.code().expect("exit code, not a signal"),
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
    }
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
fn structured_clone_round_trips_builtins_and_cyles() {
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

/// Spawns v8vm under an address-space cap (prlimit execs timeout, which
/// execs v8vm; both inherit the cap), still bounded by the same 60 s
/// kill as every other spawn.
fn v8vm_capped(as_bytes: u64, args: &[&str], stdin: &[u8]) -> Run {
    let mut capped = vec![
        "prlimit".to_string(),
        format!("--as={}", as_bytes),
        "--".to_string(),
        "timeout".to_string(),
        "60".to_string(),
        env!("CARGO_BIN_EXE_v8vm").to_string(),
    ];
    capped.extend(args.iter().map(|s| s.to_string()));
    let refs: Vec<&str> = capped.iter().map(|s| s.as_str()).collect();
    spawn(&refs, stdin)
}

/// The spawn/wait/kill core of `v8vm`, for any command line.
fn spawn(argv: &[&str], stdin: &[u8]) -> Run {
    let mut child = Command::new(argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn test command");
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

// ---- review round 1 ----

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
        "TypeError The \"input\" argument must be specified\nTypeError The \"input\" argument must be specified\nTypeError The \"The value argument must be specified\" argument must be specified\n",
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

#[test]
fn decode_of_a_huge_buffer_under_memory_pressure_throws_instead_of_aborting() {
    // 300 MB of ASCII under a 1.4 GB address-space cap: the input buffer
    // exists, but the Rust output reserve cannot. The fallible path turns
    // that into a guest RangeError; the old path aborted the process.
    let path = script(
        "decode-pressure",
        r#"const u = new Uint8Array(300 * 1048576).fill(65);
           try { new TextDecoder().decode(u); console.log("decoded"); }
           catch (e) { console.log(e.name, e.message); }"#,
    );
    let r = v8vm_capped(1_400_000_000, &[path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout, "RangeError decoded text too large\n",
        "stderr: {}",
        r.stderr
    );
}

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
