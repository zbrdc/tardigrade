//! atob / btoa: Latin-1 base64 per the platform contract. btoa accepts
//! one byte per code unit (each <= 0xFF); atob decodes forgiving-base64
//! (infra.spec.whatwg.org): ASCII whitespace is ignored, "=" pads only
//! the final quad (at most twice), and trailing bits are not checked.
//! Buffers grow through try_reserve and oversize results throw RangeError
//! instead of failing an allocation or returning an empty string.

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
/// InvalidCharacterError) or an output that cannot be allocated / cannot
/// fit a string (RangeError).
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

/// Forgiving-base64 decode; output grows fallibly. A stray "=" fails the
/// alphabet lookup, so only the length rule needs an explicit check.
fn b64_decode_forgiving(input: &str) -> Result<Vec<u8>, B64Err> {
    let stripped: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut data = stripped.as_slice();
    if data.len().is_multiple_of(4) {
        if let Some(rest) = data.strip_suffix(b"==") {
            data = rest;
        } else if let Some(rest) = data.strip_suffix(b"=") {
            data = rest;
        }
    }
    if data.len() % 4 == 1 {
        return Err(B64Err::Invalid);
    }
    let mut out = Vec::new();
    out.try_reserve(data.len().div_ceil(4) * 3)
        .map_err(|_| B64Err::TooLarge)?;
    let (chunks, remainder) = data.as_chunks::<4>();
    for group in chunks {
        let mut v = [0u8; 4];
        for (i, &b) in group.iter().enumerate() {
            v[i] = b64_value(b).ok_or(B64Err::Invalid)?;
        }
        out.extend_from_slice(&[
            v[0] << 2 | v[1] >> 4,
            v[1] << 4 | v[2] >> 2,
            v[2] << 6 | v[3],
        ]);
    }
    match remainder {
        [a, b] => out.push(
            b64_value(*a).ok_or(B64Err::Invalid)? << 2 | b64_value(*b).ok_or(B64Err::Invalid)? >> 4,
        ),
        [a, b, c] => {
            let (x, y, z) = (
                b64_value(*a).ok_or(B64Err::Invalid)?,
                b64_value(*b).ok_or(B64Err::Invalid)?,
                b64_value(*c).ok_or(B64Err::Invalid)?,
            );
            out.extend_from_slice(&[x << 2 | y >> 4, y << 4 | z >> 2]);
        }
        _ => {}
    }
    Ok(out)
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

fn throw_range_b64(scope: &mut v8::HandleScope) {
    throw_range(scope, "encoded data too large");
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
        Err(B64Err::TooLarge) => return throw_range_b64(scope),
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
        None => throw_range_b64(scope),
    }
});

guest_cb!(atob, |args, rv, scope| {
    if args.length() == 0 {
        return throw_type(scope, "The \"input\" argument must be specified");
    }
    let Some(s) = args.get(0).to_string(scope) else {
        return;
    };
    // Base64 input is ASCII, so the string's lossy UTF-8 form is exact
    // for anything that can decode; non-ASCII then simply fails lookup.
    let bytes = match b64_decode_forgiving(&s.to_rust_string_lossy(scope)) {
        Ok(bytes) => bytes,
        Err(B64Err::TooLarge) => return throw_range_b64(scope),
        Err(B64Err::Invalid) => {
            return throw_named(scope, "InvalidCharacterError", "atob: invalid base64 input")
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

    #[test]
    fn base64_round_trips_and_forgives_padding_shapes() {
        for bytes in [
            &b""[..],
            &[0u8],
            &[0, 1],
            &[0, 1, 2],
            &[255, 254, 253, 0, 200],
        ] {
            let decoded = b64_decode_forgiving(&b64_encode(bytes).unwrap()).unwrap();
            assert_eq!(decoded, bytes);
        }
        // Whitespace is ignored, "one =" and "==" both work, QR== is Q with
        // unchecked trailing bits, exactly like atob in browsers.
        assert_eq!(b64_decode_forgiving(" Q U\n==").unwrap(), b"A");
        assert_eq!(b64_decode_forgiving("QR==").unwrap(), b"A");
        assert_eq!(b64_decode_forgiving("QUJD").unwrap(), b"ABC");
    }

    #[test]
    fn base64_rejects_malformed_input() {
        for bad in ["a", "====", "A===", "Q==", "AB=C", "A!BC"] {
            assert_eq!(
                b64_decode_forgiving(bad),
                Err(B64Err::Invalid),
                "{:?} decoded",
                bad
            );
        }
        // "=" may also close a full quad's final chunk, per the algorithm.
        assert_eq!(b64_decode_forgiving("ABC=").unwrap(), [0x00, 0x10]);
        // The Latin-1 gate: a unit above 0xFF is invalid input for btoa.
        assert_eq!(latin1_units(&[0x41, 0x100]), Err(B64Err::Invalid));
    }
}
