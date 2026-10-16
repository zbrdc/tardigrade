//! Focused UTF-8 conformance checks for the supported TextEncoder/TextDecoder
//! surface, following the Encoding Standard:
//! https://encoding.spec.whatwg.org/

use tardigrade::Config;

fn run_guest(source: &str, entry: &str) {
    let config = Config {
        timeout_ms: 5_000,
        heap_mb: 32,
        stdin_consumed: true,
        ..Config::new()
    };
    assert_eq!(
        tardigrade::run(source, entry, &config),
        0,
        "guest assertions failed in {entry}"
    );
}

#[test]
fn decoder_handles_every_single_split_of_mixed_utf8_with_a_bom() {
    run_guest(
        r#"
        const bytes = [
          0xEF, 0xBB, 0xBF,             // leading BOM
          0x41,                         // ASCII
          0xC3, 0xA9,                   // two-byte code point
          0xE2, 0x82, 0xAC,             // three-byte code point
          0xF0, 0x9F, 0x98, 0x80,       // four-byte code point
          0x5A,
        ];
        const expected = "A\u00E9\u20AC\u{1F600}Z";
        for (let split = 0; split <= bytes.length; split++) {
          const decoder = new TextDecoder("utf-8");
          const first = decoder.decode(new Uint8Array(bytes.slice(0, split)),
                                       {stream: true});
          const second = decoder.decode(new Uint8Array(bytes.slice(split)),
                                        {stream: true});
          const last = decoder.decode();
          if (first + second + last !== expected)
            throw new Error("wrong decode at byte split " + split);
        }
        "#,
        "utf8-every-split.js",
    );
}

#[test]
fn encode_into_honors_offset_and_stops_before_a_surrogate_pair() {
    run_guest(
        r#"
        const backing = new Uint8Array(10);
        backing.fill(0xCC);
        // The view starts inside its backing store. After "A", only three
        // bytes remain, so the four-byte surrogate pair must not be split.
        const destination = new Uint8Array(backing.buffer, 3, 4);
        const result = new TextEncoder().encodeInto("A\u{1F600}B", destination);
        if (result.read !== 1 || result.written !== 1)
          throw new Error("wrong encodeInto counts: " +
                          result.read + "/" + result.written);
        const expected = [0xCC, 0xCC, 0xCC, 0x41, 0xCC,
                          0xCC, 0xCC, 0xCC, 0xCC, 0xCC];
        for (let i = 0; i < expected.length; i++) {
          if (backing[i] !== expected[i])
            throw new Error("wrong backing byte at " + i + ": " + backing[i]);
        }
        "#,
        "utf8-encode-into-offset.js",
    );
}
