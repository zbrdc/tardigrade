//! atob / btoa: Latin-1 base64 per the platform contract. btoa accepts
//! one byte per code unit (each <= 0xFF); atob decodes forgiving-base64
//! (infra.spec.whatwg.org): ASCII whitespace is skipped, "=" pads only
//! the final quad (at most twice), unpadded tails decode to their whole
//! bytes, and trailing bits are not checked. Nothing guest-sized is
//! copied on the way in: atob walks the string's own units through a
//! ValueView in two passes - pass 1 validates the whole grammar and
//! computes the exact output length with no allocation (so a malformed
//! unit anywhere fails before anything is reserved), pass 2 makes one
//! fallible exact reservation and decodes, so a memory-constrained
//! embedder sees a guest RangeError, not an abort.

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
    /// Pass 2 starts from the exact length pass 1 computed, reserved
    /// fallibly up front, so the per-emit reserves are no-op branches.
    fn with_capacity(cap: usize) -> Result<B64Stream, B64Err> {
        let mut out = Vec::new();
        out.try_reserve_exact(cap).map_err(|_| B64Err::TooLarge)?;
        Ok(B64Stream {
            quad: [0; 4],
            len: 0,
            out,
            pads: PadState::Open,
        })
    }

    fn emit(&mut self, bytes: &[u8]) -> Result<(), B64Err> {
        self.out
            .try_reserve(bytes.len())
            .map_err(|_| B64Err::TooLarge)?;
        self.out.extend_from_slice(bytes);
        Ok(())
    }

    fn feed(&mut self, unit: u16) -> Result<(), B64Err> {
        if is_ws(unit) {
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

    /// The end of input: an unpadded tail still holds whole bytes (two
    /// symbols make one, three make two) and is emitted here; a lone
    /// symbol, or a two-pad tail that never got its second "=", is
    /// malformed.
    fn finish(&mut self) -> Result<(), B64Err> {
        let v = self.quad;
        match (self.len, self.pads) {
            (0, PadState::Open) | (_, PadState::Done) => Ok(()),
            (2, PadState::Open) => self.emit(&[v[0] << 2 | v[1] >> 4]),
            (3, PadState::Open) => self.emit(&[v[0] << 2 | v[1] >> 4, v[1] << 4 | v[2] >> 2]),
            _ => Err(B64Err::Invalid),
        }
    }
}

/// Pass 1 of atob: walks the UTF-16 units, validates the
/// forgiving-base64 grammar (whitespace skipped in place, padding only
/// closing the final quad), and computes the exact output length - with
/// no allocation, so a malformed unit anywhere in a guest-sized string
/// fails before a single output byte is reserved.
fn b64_scan(len: usize, unit: impl Fn(usize) -> u16) -> Result<usize, B64Err> {
    let mut out = 0usize;
    let mut quad = 0usize;
    let mut pads = PadState::Open;
    for i in 0..len {
        let unit = unit(i);
        if is_ws(unit) {
            continue;
        }
        if pads == PadState::Done {
            return Err(B64Err::Invalid);
        }
        if unit == u16::from(b'=') {
            match (quad, pads) {
                (3, PadState::Open) => {
                    out += 2;
                    quad = 0;
                    pads = PadState::Done;
                }
                (2, PadState::Open) => {
                    out += 1;
                    quad = 0;
                    pads = PadState::WantSecond;
                }
                (0, PadState::WantSecond) => pads = PadState::Done,
                _ => return Err(B64Err::Invalid),
            }
            continue;
        }
        if pads == PadState::WantSecond {
            return Err(B64Err::Invalid);
        }
        // Only Latin-1 units can be base64; anything wider is malformed.
        let byte = u8::try_from(unit).map_err(|_| B64Err::Invalid)?;
        b64_value(byte).ok_or(B64Err::Invalid)?;
        quad += 1;
        if quad == 4 {
            out += 3;
            quad = 0;
        }
    }
    // A lone symbol or a half-finished two-pad tail is malformed; the
    // remaining arm combinations cannot occur (quad resets at four, and
    // padding always resets it).
    match (quad, pads) {
        (2, PadState::Open) => Ok(out + 1),
        (3, PadState::Open) => Ok(out + 2),
        (0, PadState::Open) | (_, PadState::Done) => Ok(out),
        _ => Err(B64Err::Invalid),
    }
}

/// True for the infra spec's ASCII whitespace (tab, LF, FF, CR, space;
/// notably not vertical tab).
fn is_ws(unit: u16) -> bool {
    matches!(unit, 0x09 | 0x0a | 0x0c | 0x0d | 0x20)
}

/// Drives the two passes over `len` code units: validate and size, then
/// reserve exactly and decode (partial quads included).
fn b64_decode_units(len: usize, unit: impl Fn(usize) -> u16) -> Result<Vec<u8>, B64Err> {
    let cap = b64_scan(len, &unit)?;
    let mut stream = B64Stream::with_capacity(cap)?;
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
        Err(B64Err::TooLarge) => return throw_range(scope, "encoded data too large"),
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
        None => throw_range(scope, "encoded data too large"),
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
        Err(B64Err::TooLarge) => return throw_range(scope, "decoded data too large"),
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
        None => throw_range(scope, "decoded data too large"),
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

    /// A straightforward reference decoder (the infra algorithm on a
    /// byte vector) for the exhaustive comparison below.
    fn reference(input: &str) -> Result<Vec<u8>, ()> {
        let stripped: Vec<u8> = input.bytes().filter(|b| !is_ws(u16::from(*b))).collect();
        let mut data = stripped.as_slice();
        if data.len().is_multiple_of(4) {
            if let Some(rest) = data.strip_suffix(b"==") {
                data = rest;
            } else if let Some(rest) = data.strip_suffix(b"=") {
                data = rest;
            }
        }
        if data.len() % 4 == 1 || data.contains(&b'=') {
            return Err(());
        }
        let val = |b: u8| b64_value(b).ok_or(());
        let mut out = Vec::new();
        let (chunks, tail) = data.as_chunks::<4>();
        for g in chunks {
            let v = [val(g[0])?, val(g[1])?, val(g[2])?, val(g[3])?];
            out.extend_from_slice(&[
                v[0] << 2 | v[1] >> 4,
                v[1] << 4 | v[2] >> 2,
                v[2] << 6 | v[3],
            ]);
        }
        match tail {
            [a, b] => out.push(val(*a)? << 2 | val(*b)? >> 4),
            [a, b, c] => {
                let (x, y, z) = (val(*a)?, val(*b)?, val(*c)?);
                out.extend_from_slice(&[x << 2 | y >> 4, y << 4 | z >> 2]);
            }
            _ => {}
        }
        Ok(out)
    }

    /// Exhaustive comparison over every string up to length 5 from the
    /// probe alphabet: two-pass atob must agree with the reference on
    /// both the bytes and whether the input decodes at all.
    #[test]
    fn base64_two_pass_matches_a_reference_decoder_exhaustively() {
        const ALPHA: [char; 7] = ['A', 'Q', '/', '=', ' ', '!', '\u{100}'];
        fn check(s: &str, depth: usize, seen: &mut usize) {
            let units: Vec<u16> = s.encode_utf16().collect();
            let mine = b64_decode_units(units.len(), |i| units[i]);
            let want = reference(s);
            *seen += 1;
            match want {
                Ok(w) => assert_eq!(mine, Ok(w), "atob({:?})", s),
                Err(()) => assert_eq!(mine, Err(B64Err::Invalid), "atob({:?})", s),
            }
            if depth == 0 {
                return;
            }
            for a in ALPHA {
                check(&format!("{}{}", s, a), depth - 1, seen);
            }
        }
        let mut seen = 0;
        check("", 5, &mut seen);
        assert_eq!(seen, 1 + 7 + 49 + 343 + 2401 + 16807);
    }

    #[test]
    fn base64_scan_validates_and_sizes_before_any_allocation() {
        // Unpadded tails hold whole bytes: two symbols make one, three
        // make two (the round-3 blocker).
        assert_eq!(b64_scan(2, |i| u16::from(b"QQ"[i])), Ok(1));
        assert_eq!(b64_scan(3, |i| u16::from(b"QUI"[i])), Ok(2));
        // A malformed unit anywhere - first, middle, or tail of a
        // guest-sized string - is found by the scan, which allocates
        // nothing before it returns.
        let bad_tail = |n: usize| {
            move |i: usize| {
                if i + 1 == n {
                    u16::from(b'!')
                } else {
                    u16::from(b'A')
                }
            }
        };
        assert_eq!(
            b64_scan(1_000_000, bad_tail(1_000_000)),
            Err(B64Err::Invalid)
        );
        let bad_first = |i: usize| {
            if i == 0 {
                u16::from(b'!')
            } else {
                u16::from(b'A')
            }
        };
        assert_eq!(b64_scan(1_000_000, bad_first), Err(B64Err::Invalid));
        // The exact size feeds pass 2's single reservation.
        assert_eq!(b64_scan(4, |i| u16::from(b"QUJD"[i])), Ok(3));
        assert_eq!(b64_scan(5, |i| u16::from(b" Q U "[i])), Ok(1));
    }
}
