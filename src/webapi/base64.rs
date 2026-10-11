//! atob / btoa: Latin-1 base64 per the platform contract. btoa accepts
//! one byte per code unit (each <= 0xFF); atob decodes forgiving-base64
//! (infra.spec.whatwg.org): ASCII whitespace is skipped, "=" pads only
//! the final quad (at most twice), and trailing bits are not checked.
//! Nothing guest-sized is copied on the way in: atob walks the string's
//! own units through a ValueView, fails on the first non-alphabet unit
//! before any output allocation, and grows the output fallibly so a
//! memory-constrained embedder sees a guest RangeError, not an abort.

use crate::guest_cb;
use crate::webapi::{throw_named, throw_range, throw_type};

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_value(byte: u8) -> Option<u8> {
    B64_ALPHABET
        .iter()
        .position(|&c| c == byte)
        .map(|i| i as u8)
}

/// Why a decode attempt failed: malformed input (the platform's
/// InvalidCharacterError) or an output that cannot be allocated /
/// cannot fit a string (RangeError).
#[derive(Debug, PartialEq)]
enum B64Err {
    Invalid,
    TooLarge,
}

/// Standard base64 with padding; grows the output fallibly.
fn b64_encode(bytes: &[u8]) -> Result<String, B64Err> {
    let mut out = String::new();
    out.try_reserve(bytes.len().div_ceil(3) * 4)
        .map_err(|_| B64Err::TooLarge)?;
    for group in bytes.chunks(3) {
        let b0 = u32::from(group[0]);
        let b1 = group.get(1).map_or(0, |&b| u32::from(b));
        let b2 = group.get(2).map_or(0, |&b| u32::from(b));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHABET[(n >> 18) as usize & 0x3f] as char);
        out.push(B64_ALPHABET[(n >> 12) as usize & 0x3f] as char);
        out.push(if group.len() > 1 {
            B64_ALPHABET[(n >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if group.len() > 2 {
            B64_ALPHABET[n as usize & 0x3f] as char
        } else {
            '='
        });
    }
    Ok(out)
}

/// Where the padding state machine stands: open (data may come), waiting
/// for the second "=" of a two-pad tail, or finished (only whitespace
/// may follow).
#[derive(Clone, Copy, PartialEq)]
enum PadState {
    Open,
    WantSecond,
    Done,
}

/// Streaming forgiving-base64 decoder over UTF-16 code units. Feeding
/// the units one at a time keeps the whole input out of Rust memory:
/// whitespace is skipped in place, and the first unit that cannot be
/// part of valid base64 fails before a single output byte is reserved.
struct B64Stream {
    quad: [u8; 4],
    len: usize,
    out: Vec<u8>,
    pads: PadState,
}

impl B64Stream {
    fn new() -> B64Stream {
        B64Stream {
            quad: [0; 4],
            len: 0,
            out: Vec::new(),
            pads: PadState::Open,
        }
    }

    fn emit(&mut self, bytes: &[u8]) -> Result<(), B64Err> {
        self.out
            .try_reserve(bytes.len())
            .map_err(|_| B64Err::TooLarge)?;
        self.out.extend_from_slice(bytes);
        Ok(())
    }

    fn feed(&mut self, unit: u16) -> Result<(), B64Err> {
        if matches!(unit, 0x09 | 0x0a | 0x0c | 0x0d | 0x20) {
            return Ok(());
        }
        if self.pads == PadState::Done {
            return Err(B64Err::Invalid);
        }
        if unit == u16::from(b'=') {
            return self.feed_pad();
        }
        if self.pads == PadState::WantSecond {
            return Err(B64Err::Invalid);
        }
        // Only Latin-1 units can be base64; anything wider is malformed.
        let byte = u8::try_from(unit).map_err(|_| B64Err::Invalid)?;
        self.quad[self.len] = b64_value(byte).ok_or(B64Err::Invalid)?;
        self.len += 1;
        if self.len == 4 {
            let v = self.quad;
            self.emit(&[
                v[0] << 2 | v[1] >> 4,
                v[1] << 4 | v[2] >> 2,
                v[2] << 6 | v[3],
            ])?;
            self.len = 0;
        }
        Ok(())
    }

    /// "=" may close a quad missing one byte (single pad) or, when two
    /// data units are pending, the first of a two-pad tail; any other
    /// position is malformed.
    fn feed_pad(&mut self) -> Result<(), B64Err> {
        if self.pads == PadState::WantSecond {
            self.pads = PadState::Done;
            return Ok(());
        }
        match self.len {
            3 => {
                let v = self.quad;
                self.emit(&[v[0] << 2 | v[1] >> 4, v[1] << 4 | v[2] >> 2])?;
                self.pads = PadState::Done;
            }
            2 => {
                let v = self.quad;
                self.emit(&[v[0] << 2 | v[1] >> 4])?;
                self.pads = PadState::WantSecond;
            }
            _ => return Err(B64Err::Invalid),
        }
        self.len = 0;
        Ok(())
    }

    /// The end of input: a lone data unit left over, or a two-pad tail
    /// that never got its second "=", is malformed.
    fn finish(&mut self) -> Result<(), B64Err> {
        match (self.len, self.pads) {
            (0, PadState::Open) | (_, PadState::Done) => Ok(()),
            (0, PadState::WantSecond) | (1, _) => Err(B64Err::Invalid),
            _ => Ok(()),
        }
    }
}

/// Drives the stream decoder over `len` code units.
fn b64_decode_units(len: usize, unit: impl Fn(usize) -> u16) -> Result<Vec<u8>, B64Err> {
    let mut stream = B64Stream::new();
    for i in 0..len {
        stream.feed(unit(i))?;
    }
    stream.finish()?;
    Ok(stream.out)
}

/// btoa's input domain: one byte per code unit, each <= 0xFF. V8 one-byte
/// strings are Latin-1 by construction, so only two-byte strings need the
/// per-unit check.
fn latin1_units(units: &[u16]) -> Result<Vec<u8>, B64Err> {
    let mut out = Vec::new();
    out.try_reserve(units.len()).map_err(|_| B64Err::TooLarge)?;
    for &u in units {
        out.push(u8::try_from(u).map_err(|_| B64Err::Invalid)?);
    }
    Ok(out)
}

fn throw_range_b64(scope: &mut v8::HandleScope, msg: &str) {
    throw_range(scope, msg);
}

guest_cb!(btoa, |args, rv, scope| {
    if args.length() == 0 {
        return throw_type(scope, "The \"input\" argument must be specified");
    }
    let Some(s) = args.get(0).to_string(scope) else {
        return;
    };
    // The one-byte (Latin-1) case borrows the string's own bytes; only
    // two-byte strings go through the checked copy.
    let view = v8::ValueView::new(scope, s);
    let result = match view.data() {
        v8::ValueViewData::OneByte(bytes) => b64_encode(bytes),
        v8::ValueViewData::TwoByte(units) => latin1_units(units).and_then(|b| b64_encode(&b)),
    };
    drop(view);
    let b64 = match result {
        Ok(b64) => b64,
        Err(B64Err::TooLarge) => return throw_range_b64(scope, "encoded data too large"),
        Err(B64Err::Invalid) => {
            return throw_named(
                scope,
                "InvalidCharacterError",
                "btoa: string contains characters outside of Latin-1",
            )
        }
    };
    match v8::String::new(scope, &b64) {
        // Past V8's maximum string length: RangeError, not "".
        Some(out) => rv.set(out.into()),
        None => throw_range_b64(scope, "encoded data too large"),
    }
});

guest_cb!(atob, |args, rv, scope| {
    if args.length() == 0 {
        return throw_type(scope, "The \"input\" argument must be specified");
    }
    let Some(s) = args.get(0).to_string(scope) else {
        return;
    };
    // Walk the string's own units: no Rust-side copy of the input, and a
    // malformed unit fails before the output is ever allocated.
    let view = v8::ValueView::new(scope, s);
    let result = match view.data() {
        v8::ValueViewData::OneByte(b) => {
            b64_decode_units(b.len(), |i| u16::from(*b.get(i).unwrap_or(&0)))
        }
        v8::ValueViewData::TwoByte(u) => {
            b64_decode_units(u.len(), |i| u.get(i).copied().unwrap_or(0))
        }
    };
    drop(view);
    let bytes = match result {
        Ok(bytes) => bytes,
        Err(B64Err::TooLarge) => return throw_range_b64(scope, "decoded data too large"),
        Err(B64Err::Invalid) => {
            return throw_named(
                scope,
                "InvalidCharacterError",
                "The string to be decoded is not correctly encoded.",
            )
        }
    };
    match v8::String::new_from_one_byte(scope, &bytes, v8::NewStringType::Normal) {
        Some(out) => rv.set(out.into()),
        None => throw_range_b64(scope, "decoded data too large"),
    }
});

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives the stream decoder over a &str the way atob drives it over
    /// string units (one ASCII unit per byte).
    fn dec(input: &str) -> Result<Vec<u8>, B64Err> {
        let b = input.as_bytes();
        b64_decode_units(b.len(), |i| u16::from(b[i]))
    }

    #[test]
    fn base64_round_trips_and_forgives_padding_shapes() {
        for bytes in [
            &b""[..],
            &[0u8],
            &[0, 1],
            &[0, 1, 2],
            &[255, 254, 253, 0, 200],
        ] {
            let decoded = dec(&b64_encode(bytes).unwrap()).unwrap();
            assert_eq!(decoded, bytes);
        }
        // Whitespace is ignored, "one =" and "==" both work, QR== is Q with
        // unchecked trailing bits, exactly like atob in browsers.
        assert_eq!(dec(" Q U\n==").unwrap(), b"A");
        assert_eq!(dec("QR==").unwrap(), b"A");
        assert_eq!(dec("QUJD").unwrap(), b"ABC");
        // Whitespace inside the padding tail and after it.
        assert_eq!(dec("QU=\t=").unwrap(), b"A");
        assert_eq!(dec("QU==  ").unwrap(), b"A");
    }

    #[test]
    fn base64_rejects_malformed_input() {
        for bad in [
            "a",
            "====",
            "A===",
            "Q=",
            "Q==",
            "AB=C",
            "A!BC",
            "AB= =C",
            "QU=Q",
            "QU==Q",
            "\u{100}B==",
        ] {
            assert_eq!(dec(bad), Err(B64Err::Invalid), "{:?} decoded", bad);
        }
        // "=" may also close a full quad's final chunk, per the algorithm.
        assert_eq!(dec("ABC=").unwrap(), [0x00, 0x10]);
        // The Latin-1 gate: a unit above 0xFF is invalid input for btoa.
        assert_eq!(latin1_units(&[0x41, 0x100]), Err(B64Err::Invalid));
        // A bad unit after valid data still fails (padding end state).
        assert_eq!(dec("QUJD!"), Err(B64Err::Invalid));
    }

    #[test]
    fn base64_stream_fails_before_allocating_output() {
        // The very first unit is malformed: no quad can ever complete, so
        // nothing is emitted (the reserve is structurally unreachable).
        let mut stream = B64Stream::new();
        assert_eq!(stream.feed(b'!' as u16), Err(B64Err::Invalid));
        assert!(stream.out.is_empty());
        // A tail-only failure after valid groups also leaves a prefix.
        let mut stream = B64Stream::new();
        for u in b"QUJD" {
            stream.feed(u16::from(*u)).unwrap();
        }
        assert_eq!(stream.feed(b'?' as u16), Err(B64Err::Invalid));
        assert_eq!(stream.out, b"ABC");
    }
}
