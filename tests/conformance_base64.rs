//! Focused guest-level checks for the documented forgiving-base64 behavior.

fn assert_guest(name: &str, source: &str) {
    let config = tardigrade::Config {
        timeout_ms: 5_000,
        heap_mb: 32,
        ..tardigrade::Config::new()
    };
    assert_eq!(
        tardigrade::run(source, name, &config),
        0,
        "guest assertion failed: {name}"
    );
}

#[test]
fn atob_ignores_ascii_whitespace_at_every_position() {
    assert_guest(
        "base64-whitespace.js",
        r#"
        const cases = [["Zm8=", "fo"], ["Zg==", "f"]];
        const whitespace = ["\t", "\n", "\f", "\r", " "];
        for (const ws of whitespace) {
          for (const [compact, expected] of cases) {
            for (let position = 0; position <= compact.length; position++) {
              const input = compact.slice(0, position) + ws + compact.slice(position);
              if (atob(input) !== expected) {
                throw new Error("ASCII whitespace was not ignored at position " + position);
              }
            }
          }
        }
        "#,
    );
}

#[test]
fn atob_accepts_only_two_and_three_symbol_unpadded_tails() {
    assert_guest(
        "base64-tails.js",
        r#"
        const valid = [["Zg", "f"], ["Zm8", "fo"]];
        for (const [input, expected] of valid) {
          if (atob(input) !== expected) throw new Error("wrong unpadded tail: " + input);
        }

        function expectInvalidCharacter(input) {
          let caught;
          try { atob(input); } catch (error) { caught = error; }
          if (!caught || caught.name !== "InvalidCharacterError") {
            throw new Error("expected InvalidCharacterError for " + JSON.stringify(input));
          }
        }
        for (const input of ["A", "AAAAA", "Zm9vY", " \tA\n "]) {
          expectInvalidCharacter(input);
        }
        "#,
    );
}

#[test]
fn atob_rejects_misplaced_and_excess_padding() {
    assert_guest(
        "base64-padding.js",
        r#"
        function expectInvalidCharacter(input) {
          let caught;
          try { atob(input); } catch (error) { caught = error; }
          if (!caught || caught.name !== "InvalidCharacterError") {
            throw new Error("expected InvalidCharacterError for " + JSON.stringify(input));
          }
        }
        for (const input of ["=Zg==", "Z=g=", "Zg=A", "Zg=", "Zg===", "Zm8==", "AAAA="]) {
          expectInvalidCharacter(input);
        }
        "#,
    );
}

#[test]
fn atob_accepts_nonzero_trailing_bits() {
    assert_guest(
        "base64-trailing-bits.js",
        r#"
        const cases = [
          ["Zh==", "f"], ["Zh", "f"],
          ["Zm9=", "fo"], ["Zm9", "fo"]
        ];
        for (const [input, expected] of cases) {
          if (atob(input) !== expected) {
            throw new Error("nonzero trailing bits were rejected or decoded incorrectly: " + input);
          }
        }
        "#,
    );
}

#[test]
fn btoa_accepts_latin1_and_rejects_wider_code_units() {
    assert_guest(
        "base64-latin1.js",
        r#"
        const valid = [
          ["", ""], ["\u0080", "gA=="], ["\u00e9", "6Q=="], ["\u00ff", "/w=="]
        ];
        for (const [input, expected] of valid) {
          if (btoa(input) !== expected) throw new Error("wrong Latin-1 encoding");
        }

        for (const input of ["\u0100", "\u{1f600}", "\uD800", "\uDC00"]) {
          let caught;
          try { btoa(input); } catch (error) { caught = error; }
          if (!caught || caught.name !== "InvalidCharacterError") {
            throw new Error("expected InvalidCharacterError for non-Latin-1 input");
          }
        }
        "#,
    );
}
