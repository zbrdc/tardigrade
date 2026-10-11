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
        "\"hi\"\nTypeError The encoded data is not valid.\n",
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
