//! The byte level of TextEncoder and TextDecoder: the JS classes live in
//! prelude.js and add only per-instance streaming state. Everything
//! byte-shaped is a small pure function here so it can be unit-tested
//! without V8.

use std::cell::Cell;

use crate::diag::throw;
use crate::guest_cb;

// ---- v8 object helpers (same shapes as api.rs) ----

fn v8_str<'s>(scope: &mut v8::HandleScope<'s>, s: &str) -> v8::Local<'s, v8::String> {
    v8::String::new(scope, s).unwrap_or_else(|| v8::String::empty(scope))
}

fn set_prop(obj: v8::Local<v8::Object>, scope: &mut v8::HandleScope, name: &str, value: v8::Local<v8::Value>) {
    let key = v8_str(scope, name);
    obj.set(scope, key.into(), value);
}

fn put_fn(obj: v8::Local<v8::Object>, scope: &mut v8::HandleScope, name: &str, cb: v8::FunctionCallback) {
    let f = v8::Function::new_raw(scope, cb).expect("Function::new_raw cannot fail");
    set_prop(obj, scope, name, f.into());
}

fn throw_type(scope: &mut v8::HandleScope, msg: &str) {
    let m = v8_str(scope, msg);
    let exc = v8::Exception::type_error(scope, m);
    scope.throw_exception(exc);
}

// ---- TextEncoder byte level ----

/// Builds a Uint8Array owning its own copy of the bytes (same pattern as
/// vm.fs.readBytes).
fn u8_view<'s>(scope: &mut v8::HandleScope<'s>, bytes: Vec<u8>) -> Option<v8::Local<'s, v8::Uint8Array>> {
    let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
    let len = store.byte_length();
    let buffer = v8::ArrayBuffer::with_backing_store(scope, &store);
    v8::Uint8Array::new(scope, buffer, 0, len)
}

guest_cb!(encode_utf8, |args, rv, scope| {
    let Some(s) = args.get(0).to_string(scope) else { return };
    // V8's own UTF-8 writer replaces each unpaired surrogate with U+FFFD,
    // which is exactly what the WHATWG UTF-8 encoder prescribes.
    let bytes = s.to_rust_string_lossy(scope).into_bytes();
    match u8_view(scope, bytes) {
        Some(v) => rv.set(v.into()),
        None => throw(scope, "encoded data too large"),
    }
});

/// One code point of the source, pre-encoded. `units` counts the UTF-16
/// code units it occupies; an unpaired surrogate becomes U+FFFD over one
/// unit, per the WHATWG encoder.
struct Utf8Step {
    units: usize,
    len: usize,
    bytes: [u8; 4],
}

fn utf8_step(unit: &impl Fn(usize) -> u16, i: usize) -> Utf8Step {
    let c = unit(i);
    let (cp, units) = if (0xd800..0xdc00).contains(&c) {
        match unit(i + 1) {
            t if (0xdc00..0xe000).contains(&t) => (surrogate_pair(c, t), 2),
            _ => (0xfffd, 1),
        }
    } else if (0xdc00..0xe000).contains(&c) {
        (0xfffd, 1)
    } else {
        (u32::from(c), 1)
    };
    let (bytes, enc_len) = encode_code_point(cp);
    Utf8Step { units, len: enc_len, bytes }
}

fn surrogate_pair(lead: u16, trail: u16) -> u32 {
    0x10000 + ((u32::from(lead) - 0xd800) << 10) + (u32::from(trail) - 0xdc00)
}

fn encode_code_point(cp: u32) -> ([u8; 4], usize) {
    let cont = |b: u32| 0x80 | (b & 0x3f) as u8;
    if cp < 0x80 {
        ([cp as u8, 0, 0, 0], 1)
    } else if cp < 0x800 {
        ([0xc0 | (cp >> 6) as u8, cont(cp), 0, 0], 2)
    } else if cp < 0x10000 {
        ([0xe0 | (cp >> 12) as u8, cont(cp >> 6), cont(cp), 0], 3)
    } else {
        ([0xf0 | (cp >> 18) as u8, cont(cp >> 12), cont(cp >> 6), cont(cp)], 4)
    }
}

/// Fills `out` (the destination view's window) with the UTF-8 encoding of
/// `len` code units, stopping before any code point that would not fit
/// whole: encodeInto never writes a partial sequence. Returns (UTF-16
/// units consumed, bytes written).
fn encode_into_units(len: usize, unit: impl Fn(usize) -> u16, out: &[Cell<u8>]) -> (usize, usize) {
    let (mut read, mut written) = (0, 0);
    while read < len {
        let step = utf8_step(&unit, read);
        if written + step.len > out.len() {
            break;
        }
        for k in 0..step.len {
            out[written + k].set(step.bytes[k]);
        }
        written += step.len;
        read += step.units;
    }
    (read, written)
}

fn read_written_obj<'s>(
    scope: &mut v8::HandleScope<'s>,
    read: usize,
    written: usize,
) -> v8::Local<'s, v8::Object> {
    let obj = v8::Object::new(scope);
    let r = v8::Number::new(scope, read as f64);
    set_prop(obj, scope, "read", r.into());
    let w = v8::Number::new(scope, written as f64);
    set_prop(obj, scope, "written", w.into());
    obj
}

/// encodeInto: writes through the view's backing store, honoring the
/// view's offset and length (a detached or zero-length view has zero
/// capacity, so nothing is ever read or written).
fn encode_into<'s>(
    scope: &mut v8::HandleScope<'s>,
    src: v8::Local<'s, v8::String>,
    dest: v8::Local<'s, v8::ArrayBufferView>,
) -> v8::Local<'s, v8::Value> {
    let Some(store) = dest.get_backing_store() else {
        return read_written_obj(scope, 0, 0).into();
    };
    let cells: &[Cell<u8>] = &store;
    let start = dest.byte_offset();
    let end = start.saturating_add(dest.byte_length());
    let Some(window) = cells.get(start..end.min(cells.len())) else {
        return read_written_obj(scope, 0, 0).into();
    };
    // ValueView forbids GC while alive, so the writes below (through the
    // backing store only) come first and the result object is built after.
    let view = v8::ValueView::new(scope, src);
    let (read, written) = match view.data() {
        v8::ValueViewData::OneByte(b) => {
            encode_into_units(b.len(), |i| u16::from(*b.get(i).unwrap_or(&0)), window)
        }
        v8::ValueViewData::TwoByte(u) => {
            encode_into_units(u.len(), |i| u.get(i).copied().unwrap_or(0), window)
        }
    };
    drop(view);
    read_written_obj(scope, read, written).into()
}

guest_cb!(encode_into_utf8, |args, rv, scope| {
    if !args.get(1).is_uint8_array() {
        return throw_type(scope, "encodeInto destination must be a Uint8Array");
    }
    let Some(src) = args.get(0).to_string(scope) else { return };
    if let Ok(dest) = v8::Local::<v8::ArrayBufferView>::try_from(args.get(1)) {
        return rv.set(encode_into(scope, src, dest));
    }
    throw_type(scope, "encodeInto destination must be a Uint8Array");
});
// ---- TextDecoder byte level ----

/// The per-call decode options; carried as one group because they all
/// describe the same decode step.
struct DecodeOpts {
    fatal: bool,
    ignore_bom: bool,
    bom_pending: bool,
    flush: bool,
}

fn opt_bool(scope: &mut v8::HandleScope, obj: v8::Local<v8::Object>, name: &str) -> bool {
    let key = v8_str(scope, name);
    obj.get(scope, key.into()).is_some_and(|v| v.boolean_value(scope))
}

impl DecodeOpts {
    fn read(scope: &mut v8::HandleScope, v: &v8::Local<v8::Value>) -> DecodeOpts {
        let defaults = DecodeOpts { fatal: false, ignore_bom: false, bom_pending: false, flush: true };
        let Ok(obj) = v8::Local::<v8::Object>::try_from(*v) else { return defaults };
        DecodeOpts {
            fatal: opt_bool(scope, obj, "fatal"),
            ignore_bom: opt_bool(scope, obj, "ignoreBOM"),
            bom_pending: opt_bool(scope, obj, "bomPending"),
            flush: opt_bool(scope, obj, "flush"),
        }
    }
}

/// Bytes of a BufferSource argument (ArrayBuffer or any view; views honor
/// their offset and length), empty for undefined, None for anything else.
/// Never runs guest code, so it cannot throw.
fn opt_bytes(scope: &mut v8::HandleScope, v: &v8::Local<v8::Value>) -> Option<Vec<u8>> {
    if v.is_undefined() {
        return Some(Vec::new());
    }
    if v.is_array_buffer_view() {
        let view = v8::Local::<v8::ArrayBufferView>::try_from(*v).ok()?;
        let mut buf = vec![0u8; view.byte_length()];
        view.copy_contents(&mut buf);
        return Some(buf);
    }
    if v.is_array_buffer() {
        let buffer = v8::Local::<v8::ArrayBuffer>::try_from(*v).ok()?;
        let view = v8::Uint8Array::new(scope, buffer, 0, buffer.byte_length())?;
        let mut buf = vec![0u8; view.byte_length()];
        view.copy_contents(&mut buf);
        return Some(buf);
    }
    None
}

enum Bom {
    Stripped,
    Keep,
    Pending,
}

/// Where a leading U+FEFF stands: complete (strip it), impossible (keep
/// everything), or still undecided because the input could yet complete a
/// BOM prefix.
fn classify_bom(input: &[u8]) -> Bom {
    if input.starts_with(&[0xef, 0xbb, 0xbf]) {
        Bom::Stripped
    } else if input.is_empty() || input == [0xef] || input == [0xef, 0xbb] {
        Bom::Pending
    } else {
        Bom::Keep
    }
}

struct DecodeOut {
    text: String,
    carry: Vec<u8>,
    bom_pending: bool,
}

/// One streaming UTF-8 decode step over carry++chunk. Replacement and
/// validation semantics are std's (each maximal ill-formed subpart becomes
/// one U+FFFD, as the WHATWG decoder prescribes); an incomplete trailing
/// sequence is carried to the next call, or flushed as U+FFFD/error.
fn decode_step(carry: &[u8], chunk: &[u8], opts: &DecodeOpts) -> Result<DecodeOut, ()> {
    let mut input = carry.to_vec();
    input.extend_from_slice(chunk);
    let mut bom_pending = opts.bom_pending && !opts.ignore_bom;
    if bom_pending {
        match classify_bom(&input) {
            Bom::Stripped => {
                input.drain(0..3);
                bom_pending = false;
            }
            Bom::Keep => bom_pending = false,
            // The final call cannot wait for more bytes, so an undecided
            // prefix is ordinary input after all.
            Bom::Pending if opts.flush => bom_pending = false,
            Bom::Pending => {
                return Ok(DecodeOut { text: String::new(), carry: input, bom_pending: true });
            }
        }
    }
    let (mut text, mut rest) = decode_complete(&input, opts.fatal)?;
    if opts.flush && !rest.is_empty() {
        if opts.fatal {
            return Err(());
        }
        text.push('\u{fffd}');
        rest.clear();
    }
    Ok(DecodeOut { text, carry: rest, bom_pending })
}

/// Decodes every complete code point; the incomplete trailing sequence
/// (if any) is returned as the remainder instead of being replaced.
fn decode_complete(input: &[u8], fatal: bool) -> Result<(String, Vec<u8>), ()> {
    let mut text = String::new();
    let mut rest = input;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                text.push_str(s);
                rest = &[];
            }
            Err(e) => {
                let valid = e.valid_up_to();
                // Cannot fail: valid_up_to marks a valid UTF-8 prefix.
                text.push_str(std::str::from_utf8(&rest[..valid]).map_err(|_| ())?);
                rest = &rest[valid..];
                match e.error_len() {
                    Some(bad) => {
                        if fatal {
                            return Err(());
                        }
                        text.push('\u{fffd}');
                        rest = &rest[bad..];
                    }
                    None => break,
                }
            }
        }
    }
    Ok((text, rest.to_vec()))
}

fn decode_result<'s>(scope: &mut v8::HandleScope<'s>, out: &DecodeOut) -> Option<v8::Local<'s, v8::Object>> {
    let obj = v8::Object::new(scope);
    let text = v8::String::new_from_utf8(scope, out.text.as_bytes(), v8::NewStringType::Normal)?;
    set_prop(obj, scope, "text", text.into());
    let carry = u8_view(scope, out.carry.clone())?;
    let bom = v8::Boolean::new(scope, out.bom_pending);
    set_prop(obj, scope, "carry", carry.into());
    set_prop(obj, scope, "bomPending", bom.into());
    Some(obj)
}

guest_cb!(decode_utf8, |args, rv, scope| {
    let carry = opt_bytes(scope, &args.get(0));
    let chunk = opt_bytes(scope, &args.get(1));    let (Some(carry), Some(chunk)) = (carry, chunk) else {
        return throw_type(scope, "decode input must be an ArrayBuffer or a typed array");
    };
    let opts = DecodeOpts::read(scope, &args.get(2));
    match decode_step(&carry, &chunk, &opts) {
        Ok(out) => match decode_result(scope, &out) {
            Some(obj) => rv.set(obj.into()),
            None => throw(scope, "decoded text too large"),
        },
        Err(()) => throw_type(scope, "The encoded data is not valid."),
    }
});
// ---- installation ----

pub(crate) fn install(scope: &mut v8::HandleScope) {
    let global = scope.get_current_context().global(scope);
    // Helper callbacks the prelude's TextEncoder/TextDecoder classes bind
    // and then remove from the global object.
    put_fn(global, scope, "__encodeUtf8", encode_utf8);
    put_fn(global, scope, "__encodeIntoUtf8", encode_into_utf8);
    put_fn(global, scope, "__decodeUtf8", decode_utf8);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_into_counts_units_and_stops_whole() {
        let cells = |n: usize| vec![Cell::new(0u8); n];
        let ascii = |s: &'static str| move |i: usize| u16::from(s.as_bytes()[i]);

        // Exact fit, and one short: the pair is never split.
        let (read, written) = encode_into_units(2, ascii("ab"), &cells(2));
        assert_eq!((read, written), (2, 2));
        let (read, written) = encode_into_units(2, ascii("ab"), &cells(1));
        assert_eq!((read, written), (1, 1));

        // A surrogate pair consumes two units as one code point.
        let pair: Vec<u16> = "\u{1f600}".encode_utf16().collect();
        let (read, written) = encode_into_units(pair.len(), |i| pair[i], &cells(4));
        assert_eq!((read, written), (2, 4));
        let (read, written) = encode_into_units(pair.len(), |i| pair[i], &cells(3));
        assert_eq!((read, written), (0, 0));

        // A two-unit prefix then a pair that no longer fits: pair untouched.
        let src: Vec<u16> = "a\u{1f600}".encode_utf16().collect();
        let (read, written) = encode_into_units(src.len(), |i| src[i], &cells(3));
        assert_eq!((read, written), (1, 1));

        // Unpaired surrogates encode as U+FFFD (3 bytes), one unit each.
        let (read, written) = encode_into_units(1, |_| 0xd800, &cells(3));
        assert_eq!((read, written), (1, 3));
        let (read, written) = encode_into_units(1, |_| 0xdc00, &cells(4));
        assert_eq!((read, written), (1, 3));

        // An empty source into an empty view: nothing read, nothing written.
        let (read, written) = encode_into_units(0, |_| 0, &cells(0));
        assert_eq!((read, written), (0, 0));
    }

    fn opts(fatal: bool, ignore_bom: bool, bom_pending: bool, flush: bool) -> DecodeOpts {
        DecodeOpts { fatal, ignore_bom, bom_pending, flush }
    }

    #[test]
    fn decode_step_decodes_replaces_and_carries() {
        let plain = opts(false, false, false, true);
        // Plain bytes.
        assert_eq!(decode_step(b"", b"hi", &plain).unwrap().text, "hi");
        // One U+FFFD per maximal ill-formed subpart, as WHATWG prescribes.
        assert_eq!(decode_step(b"", &[0xe1, 0x80, 0xc0], &plain).unwrap().text, "\u{fffd}\u{fffd}");
        assert_eq!(decode_step(b"", &[0xc0], &plain).unwrap().text, "\u{fffd}");
        // A complete prefix plus an incomplete tail: tail is carried.
        let out = decode_step(b"", &[b'a', 0xf0, 0x9f, 0x98], &opts(false, false, false, false)).unwrap();
        assert_eq!(out.text, "a");
        assert_eq!(out.carry, [0xf0, 0x9f, 0x98]);
        // The carried tail completes with the next chunk.
        assert_eq!(decode_step(&out.carry, &[0x80], &plain).unwrap().text, "\u{1f600}");
        // The same bytes flushed alone become one U+FFFD.
        assert_eq!(decode_step(&[0xf0, 0x9f, 0x98], b"", &plain).unwrap().text, "\u{fffd}");
        // A chunk boundary inside an invalid sequence keeps the counts.
        let a = decode_step(b"", &[0xe1, 0x80], &opts(false, false, false, false)).unwrap();
        let b = decode_step(&a.carry, &[0xc0], &plain).unwrap();
        assert_eq!(b.text, "\u{fffd}\u{fffd}");
    }

    #[test]
    fn decode_step_fatal_throws_instead_of_replacing() {
        let fatal = opts(true, false, false, true);
        assert!(decode_step(b"", &[0xff], &fatal).is_err());
        assert!(decode_step(b"", b"ok", &fatal).is_ok());
        // An incomplete tail at flush time is an error too. 0xe1 0x80 is a
        // valid prefix (unlike 0xe0 0x80, which 0xe0's range rules reject).
        let fatal_stream = opts(true, false, false, false);
        assert!(decode_step(b"", &[0xe1, 0x80], &fatal_stream).is_ok());
        assert!(decode_step(&[0xe1, 0x80], b"", &fatal).is_err());
    }

    #[test]
    fn decode_step_strips_only_a_leading_complete_bom() {
        let start = opts(false, false, true, true);
        assert_eq!(decode_step(b"", &[0xef, 0xbb, 0xbf, b'a'], &start).unwrap().text, "a");
        // A BOM arriving split across stream chunks is still leading.
        let pending = opts(false, false, true, false);
        let a = decode_step(b"", &[0xef], &pending).unwrap();
        assert_eq!(a.text, "");
        assert!(a.bom_pending);
        let b = decode_step(&a.carry, &[0xbb, 0xbf, b'a'], &pending).unwrap();
        assert_eq!(b.text, "a");
        // An undecided BOM prefix flushed alone is ordinary input.
        let flushed = decode_step(&[0xef], b"", &opts(false, false, true, true)).unwrap();
        assert_eq!(flushed.text, "\u{fffd}");
        // Mid-stream U+FEFF survives, and ignoreBOM keeps a leading one.
        assert_eq!(decode_step(b"", &[b'a', 0xef, 0xbb, 0xbf], &start).unwrap().text, "a\u{feff}");
        let keep = opts(false, true, true, true);
        assert_eq!(decode_step(b"", &[0xef, 0xbb, 0xbf, b'a'], &keep).unwrap().text, "\u{feff}a");
    }
}
