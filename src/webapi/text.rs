//! The byte level of TextEncoder and TextDecoder (UTF-8 only; the JS
//! classes live in prelude.js). Encoding follows the WHATWG UTF-8
//! encoder: an unpaired surrogate becomes one U+FFFD over one UTF-16
//! unit. Decoding follows the decoder: each maximal ill-formed subpart
//! becomes one U+FFFD (std's UTF-8 validation has the same semantics,
//! pinned by unit tests). Guest-sized inputs are never copied wholesale:
//! the decode input is read from the argument's backing store in bounded
//! pieces, and every output buffer grows through try_reserve so a
//! memory-constrained embedder sees a guest RangeError, not an abort.

use crate::guest_cb;
use crate::webapi::{throw_range, throw_type};

use std::cell::Cell;

// ---- TextEncoder ----

/// Builds a Uint8Array owning its own copy of the bytes (same pattern as
/// vm.fs.readBytes).
fn u8_view<'s>(
    scope: &mut v8::HandleScope<'s>,
    bytes: Vec<u8>,
) -> Option<v8::Local<'s, v8::Uint8Array>> {
    let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
    let len = store.byte_length();
    let buffer = v8::ArrayBuffer::with_backing_store(scope, &store);
    v8::Uint8Array::new(scope, buffer, 0, len)
}

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
    Utf8Step {
        units,
        len: enc_len,
        bytes,
    }
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
        (
            [
                0xf0 | (cp >> 18) as u8,
                cont(cp >> 12),
                cont(cp >> 6),
                cont(cp),
            ],
            4,
        )
    }
}

/// Why a decode step failed: invalid input under fatal semantics (the
/// platform TypeError) or a buffer that could not be allocated
/// (RangeError). Keeping them apart is what lets a memory-constrained
/// embedder fail cleanly instead of aborting.
#[derive(Clone, Copy)]
enum DecodeFail {
    Fatal,
    TooLarge,
}

/// Appends raw bytes to `out`, growing it fallibly: this is the one
/// allocation primitive every decode/encode output goes through, so no
/// guest-sized buffer can ever abort the process.
fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), DecodeFail> {
    out.try_reserve(bytes.len())
        .map_err(|_| DecodeFail::TooLarge)?;
    out.extend_from_slice(bytes);
    Ok(())
}

fn push_replacement(out: &mut Vec<u8>) -> Result<(), DecodeFail> {
    push_bytes(out, &[0xef, 0xbf, 0xbd])
}

/// Encodes `len` code units into `out`, growing it fallibly. V8's
/// `utf8_length` already reserved the exact final size for whole inputs,
/// so the per-step reserve is a no-op branch in the common case and a
/// fallible growth check if that ever undercounts.
fn push_units(len: usize, unit: impl Fn(usize) -> u16, out: &mut Vec<u8>) -> Result<(), ()> {
    let mut i = 0;
    while i < len {
        let step = utf8_step(&unit, i);
        push_bytes(out, &step.bytes[..step.len]).map_err(|_| ())?;
        i += step.units;
    }
    Ok(())
}

/// Encodes a guest string to UTF-8 bytes. The exact byte count comes
/// from V8 (it counts an unpaired surrogate as its 3-byte replacement),
/// and the buffer is reserved fallibly: an embedder short on memory gets
/// Err, which becomes a guest RangeError.
fn encode_to_vec(scope: &mut v8::HandleScope, s: v8::Local<v8::String>) -> Result<Vec<u8>, ()> {
    let mut out = Vec::new();
    out.try_reserve_exact(s.utf8_length(scope))
        .map_err(|_| ())?;
    let view = v8::ValueView::new(scope, s);
    // Rust-side allocation does not trigger V8 GC, so the view may stay
    // alive for the whole encode; nothing here runs V8 work.
    let result = match view.data() {
        v8::ValueViewData::OneByte(b) => {
            push_units(b.len(), |i| u16::from(*b.get(i).unwrap_or(&0)), &mut out)
        }
        v8::ValueViewData::TwoByte(u) => {
            push_units(u.len(), |i| u.get(i).copied().unwrap_or(0), &mut out)
        }
    };
    drop(view);
    result.map(|()| out)
}

guest_cb!(encode_utf8, |args, rv, scope| {
    let Some(s) = args.get(0).to_string(scope) else {
        return;
    };
    match encode_to_vec(scope, s) {
        Ok(bytes) => match u8_view(scope, bytes) {
            Some(v) => rv.set(v.into()),
            None => throw_range(scope, "encoded data too large"),
        },
        // RangeError, like the platform's allocation failures.
        Err(()) => throw_range(scope, "encoded data too large to allocate"),
    }
});

/// Fills `out` (the destination view's window) with the UTF-8 encoding of
/// `len` code units, stopping before any code point that would not fit
/// whole: encodeInto never writes a partial sequence. Returns (UTF-16
/// units consumed, bytes written).
fn encode_into_cells(len: usize, unit: impl Fn(usize) -> u16, out: &[Cell<u8>]) -> (usize, usize) {
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

/// The typeof name of a value, for ERR_INVALID_ARG_TYPE-shaped messages.
fn type_name(scope: &mut v8::HandleScope, v: &v8::Local<v8::Value>) -> String {
    let ty = v.type_of(scope);
    ty.to_rust_string_lossy(scope)
}

fn read_written_obj<'s>(
    scope: &mut v8::HandleScope<'s>,
    read: usize,
    written: usize,
) -> v8::Local<'s, v8::Object> {
    let obj = v8::Object::new(scope);
    let r = v8::Number::new(scope, read as f64);
    crate::api::set(obj, scope, "read", r.into());
    let w = v8::Number::new(scope, written as f64);
    crate::api::set(obj, scope, "written", w.into());
    obj
}

/// encodeInto: writes through the view's backing store, honoring the
/// view's offset and length (a detached or zero-length view has zero
/// capacity, so nothing is ever read or written). No allocation beyond
/// the tiny result object.
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
    // ValueView forbids V8 work while alive, so the writes below (through
    // the backing store only) come first and the result object is built
    // after the view is dropped.
    let view = v8::ValueView::new(scope, src);
    let (read, written) = match view.data() {
        v8::ValueViewData::OneByte(b) => {
            encode_into_cells(b.len(), |i| u16::from(*b.get(i).unwrap_or(&0)), window)
        }
        v8::ValueViewData::TwoByte(u) => {
            encode_into_cells(u.len(), |i| u.get(i).copied().unwrap_or(0), window)
        }
    };
    drop(view);
    read_written_obj(scope, read, written).into()
}

guest_cb!(encode_into_utf8, |args, rv, scope| {
    if !args.get(1).is_uint8_array() {
        return throw_type(scope, "encodeInto destination must be a Uint8Array");
    }
    // Node requires a string source here (no ToString coercion); the type
    // name rides along in the message like its ERR_INVALID_ARG_TYPE.
    if !args.get(0).is_string() {
        let ty = type_name(scope, &args.get(0));
        return throw_type(
            scope,
            &format!(
                "The \"src\" argument must be of type string. Received type {}",
                ty
            ),
        );
    }
    let Some(src) = args.get(0).to_string(scope) else {
        return;
    };
    if let Ok(dest) = v8::Local::<v8::ArrayBufferView>::try_from(args.get(1)) {
        return rv.set(encode_into(scope, src, dest));
    }
    throw_type(scope, "encodeInto destination must be a Uint8Array");
});

// ---- TextDecoder ----

/// Per-instance decode state (the JS class holds it between calls): the
/// streaming carry (always at most three bytes) and the undecided
/// leading BOM.
pub(crate) struct DecodeState {
    carry: Vec<u8>,
    bom_pending: bool,
}

/// The per-call decode options; carried as one group because they all
/// describe the same decode step. `bom_pending` only seeds the state.
#[derive(Clone, Copy)]
pub(crate) struct DecodeOpts {
    fatal: bool,
    ignore_bom: bool,
    bom_pending: bool,
    flush: bool,
}

impl DecodeOpts {
    fn read(scope: &mut v8::HandleScope, v: &v8::Local<v8::Value>) -> DecodeOpts {
        let defaults = DecodeOpts {
            fatal: false,
            ignore_bom: false,
            bom_pending: false,
            flush: true,
        };
        let Ok(obj) = v8::Local::<v8::Object>::try_from(*v) else {
            return defaults;
        };
        DecodeOpts {
            fatal: opt_bool(scope, obj, "fatal"),
            ignore_bom: opt_bool(scope, obj, "ignoreBOM"),
            bom_pending: opt_bool(scope, obj, "bomPending"),
            flush: opt_bool(scope, obj, "flush"),
        }
    }
}

fn opt_bool(scope: &mut v8::HandleScope, obj: v8::Local<v8::Object>, name: &str) -> bool {
    let key = crate::webapi::v8_str(scope, name);
    obj.get(scope, key.into())
        .is_some_and(|v| v.boolean_value(scope))
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

/// Decodes every complete code point of `input` into `out`, returning
/// the input offset where an incomplete trailing sequence starts
/// (== input.len() when there is none).
fn decode_complete(input: &[u8], fatal: bool, out: &mut Vec<u8>) -> Result<usize, DecodeFail> {
    let mut rest = input;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                push_bytes(out, s.as_bytes())?;
                return Ok(input.len());
            }
            Err(e) => match decode_error(rest, &e, fatal, out)? {
                Some(consumed) => rest = &rest[consumed..],
                None => return Ok(input.len() - rest.len() + e.valid_up_to()),
            },
        }
    }
    Ok(input.len())
}

/// Handles one ill-formed sequence: appends the valid prefix, then
/// either the replacement (skipping the maximal ill-formed subpart) or
/// nothing when fatal, and says where to resume. None means the error
/// was an incomplete tail, which belongs to the carry instead.
fn decode_error(
    rest: &[u8],
    e: &std::str::Utf8Error,
    fatal: bool,
    out: &mut Vec<u8>,
) -> Result<Option<usize>, DecodeFail> {
    let valid = e.valid_up_to();
    // Cannot fail (valid_up_to marks a valid prefix); the map_err arm
    // only keeps this panic-free.
    let prefix = std::str::from_utf8(&rest[..valid]).map_err(|_| DecodeFail::Fatal)?;
    push_bytes(out, prefix.as_bytes())?;
    let Some(bad) = e.error_len() else {
        return Ok(None);
    };
    if fatal {
        return Err(DecodeFail::Fatal);
    }
    push_replacement(out)?;
    Ok(Some(valid + bad))
}

/// Decodes one chunk into `out`, updating the state's carry and BOM
/// decision. `flush` forces a trailing incomplete sequence to become
/// U+FFFD (or the fatal error) and settles an undecided BOM prefix.
fn decode_chunk(
    state: &mut DecodeState,
    chunk: &[u8],
    opts: &DecodeOpts,
    out: &mut Vec<u8>,
) -> Result<(), DecodeFail> {
    // Bounded by construction: the carry is at most three bytes and the
    // chunk is one decode piece (at most 64 KiB).
    let mut joined = Vec::new();
    if !state.carry.is_empty() {
        joined
            .try_reserve(state.carry.len() + chunk.len())
            .map_err(|_| DecodeFail::TooLarge)?;
        joined.extend_from_slice(&state.carry);
        joined.extend_from_slice(chunk);
    }
    let input: &[u8] = if joined.is_empty() { chunk } else { &joined };
    if state.bom_pending && !opts.ignore_bom {
        match classify_bom(input) {
            Bom::Stripped => {
                state.bom_pending = false;
                state.carry.clear();
                return decode_tail(state, &input[3..], opts, out);
            }
            Bom::Keep => state.bom_pending = false,
            // The final call cannot wait for more bytes, so an undecided
            // prefix is ordinary input after all.
            Bom::Pending if opts.flush => state.bom_pending = false,
            Bom::Pending => {
                state.carry = input.to_vec();
                return Ok(());
            }
        }
    }
    decode_tail(state, input, opts, out)
}

/// Decodes settled input and stores the (tiny) incomplete tail as the
/// new carry, flushing it when this is the final call.
fn decode_tail(
    state: &mut DecodeState,
    input: &[u8],
    opts: &DecodeOpts,
    out: &mut Vec<u8>,
) -> Result<(), DecodeFail> {
    let tail = decode_complete(input, opts.fatal, out)?;
    state.carry = input[tail..].to_vec();
    if opts.flush && !state.carry.is_empty() {
        if opts.fatal {
            return Err(DecodeFail::Fatal);
        }
        state.carry.clear();
        push_replacement(out)?;
    }
    Ok(())
}

fn decode_result<'s>(
    scope: &mut v8::HandleScope<'s>,
    text: Vec<u8>,
    state: &DecodeState,
) -> Option<v8::Local<'s, v8::Object>> {
    let obj = v8::Object::new(scope);
    // Only valid UTF-8 is ever appended; a violation is the guest error,
    // not a panic across extern "C".
    let text = String::from_utf8(text).ok()?;
    let text = v8::String::new_from_utf8(scope, text.as_bytes(), v8::NewStringType::Normal)?;
    crate::api::set(obj, scope, "text", text.into());
    let carry = u8_view(scope, state.carry.clone())?;
    crate::api::set(obj, scope, "carry", carry.into());
    let bom = v8::Boolean::new(scope, state.bom_pending);
    crate::api::set(obj, scope, "bomPending", bom.into());
    Some(obj)
}

/// Piece size for reading a guest-sized decode input out of its backing
/// store: large enough to keep the loop cheap, small enough that the
/// scratch copy is irrelevant next to any guest allocation.
const DECODE_PIECE: usize = 64 * 1024;

/// The decode input, borrowed rather than copied: a window into the
/// argument's backing store (view or whole buffer).
enum ChunkSrc {
    Empty,
    Store {
        cells: v8::SharedRef<v8::BackingStore>,
        start: usize,
        len: usize,
    },
}

impl ChunkSrc {
    /// None when the argument is not a BufferSource (decode then throws
    /// TypeError). A detached buffer reads as empty.
    fn from_arg(v: &v8::Local<v8::Value>) -> Option<ChunkSrc> {
        // The JS class applies its default before calling, so undefined
        // does not normally reach here; the arm keeps a direct helper
        // call reading as empty input rather than a type error.
        if v.is_undefined() {
            return Some(ChunkSrc::Empty);
        }
        if v.is_array_buffer_view() {
            let view = v8::Local::<v8::ArrayBufferView>::try_from(*v).ok()?;
            let store = view.get_backing_store()?;
            let start = view.byte_offset();
            return Some(ChunkSrc::Store {
                cells: store,
                start,
                len: view.byte_length(),
            });
        }
        if v.is_array_buffer() {
            let buffer = v8::Local::<v8::ArrayBuffer>::try_from(*v).ok()?;
            let store = buffer.get_backing_store();
            let len = store.byte_length();
            return Some(ChunkSrc::Store {
                cells: store,
                start: 0,
                len,
            });
        }
        // A raw SharedArrayBuffer is a BufferSource too, like in browsers.
        if v.is_shared_array_buffer() {
            let buffer = v8::Local::<v8::SharedArrayBuffer>::try_from(*v).ok()?;
            let store = buffer.get_backing_store();
            let len = store.byte_length();
            return Some(ChunkSrc::Store {
                cells: store,
                start: 0,
                len,
            });
        }
        None
    }

    fn total_len(&self) -> usize {
        match self {
            ChunkSrc::Empty => 0,
            ChunkSrc::Store { cells, start, len } => (*len).min(cells.len().saturating_sub(*start)),
        }
    }

    fn pieces(&self) -> impl Iterator<Item = &[Cell<u8>]> {
        let cells: &[Cell<u8>] = match self {
            ChunkSrc::Empty => &[],
            ChunkSrc::Store { cells, start, len } => {
                let begin = (*start).min(cells.len());
                let end = begin.saturating_add(*len).min(cells.len());
                &cells[begin..end]
            }
        };
        cells.chunks(DECODE_PIECE)
    }
}

guest_cb!(decode_utf8, |args, rv, scope| {
    // The carry is this module's own Uint8Array (at most three bytes);
    // reading it through a bounded copy costs nothing.
    let carry = carry_bytes(&args.get(0));
    // Options first: a getter may resize or detach the input buffer, and
    // no guest-reachable call may run after the backing store is borrowed.
    let opts = DecodeOpts::read(scope, &args.get(2));
    let Some(chunk) = ChunkSrc::from_arg(&args.get(1)) else {
        return throw_type(
            scope,
            "decode input must be an ArrayBuffer or a typed array",
        );
    };
    let mut state = DecodeState {
        carry,
        bom_pending: opts.bom_pending && !opts.ignore_bom,
    };
    let mut out = Vec::new();
    let mut piece = vec![0u8; DECODE_PIECE.min(chunk.total_len())];
    let mut step = DecodeOpts {
        flush: false,
        ..opts
    };
    let result = (|| {
        for cells in chunk.pieces() {
            for (i, c) in cells.iter().enumerate() {
                piece[i] = c.get();
            }
            decode_chunk(&mut state, &piece[..cells.len()], &step, &mut out)?;
        }
        step.flush = opts.flush;
        decode_chunk(&mut state, &[], &step, &mut out)
    })();
    match result {
        Ok(()) => match decode_result(scope, out, &state) {
            Some(obj) => rv.set(obj.into()),
            None => throw_range(scope, "decoded text too large"),
        },
        Err(DecodeFail::Fatal) => {
            throw_type(scope, "The encoded data was not valid for encoding utf-8")
        }
        Err(DecodeFail::TooLarge) => throw_range(scope, "decoded text too large"),
    }
});

/// Reads the JS-side carry (this module's own Uint8Array) or an empty
/// one for anything else. A carry is only ever an incomplete trailing
/// sequence or a possible BOM prefix, so anything longer than three
/// bytes cannot be ours and reads as empty: a guest-sized value here
/// must never reach a Rust allocation.
fn carry_bytes(v: &v8::Local<v8::Value>) -> Vec<u8> {
    let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(*v) else {
        return Vec::new();
    };
    if view.byte_length() > MAX_CARRY {
        return Vec::new();
    }
    let mut buf = vec![0u8; view.byte_length()];
    view.copy_contents(&mut buf);
    buf
}

/// The carry is at most three bytes of a cut-off UTF-8 sequence.
const MAX_CARRY: usize = 3;

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(fatal: bool, ignore_bom: bool, flush: bool) -> DecodeOpts {
        DecodeOpts {
            fatal,
            ignore_bom,
            bom_pending: false,
            flush,
        }
    }

    fn decode_all(state: &mut DecodeState, chunk: &[u8], o: &DecodeOpts) -> Result<String, ()> {
        let mut out = Vec::new();
        decode_chunk(state, chunk, o, &mut out).map_err(|_| ())?;
        String::from_utf8(out).map_err(|_| ())
    }

    #[test]
    fn decode_replaces_carries_and_flushes() {
        let plain = opts(false, false, true);
        let mut st = DecodeState {
            carry: Vec::new(),
            bom_pending: false,
        };
        assert_eq!(decode_all(&mut st, b"hi", &plain).unwrap(), "hi");
        // One U+FFFD per maximal ill-formed subpart, as WHATWG prescribes.
        st = DecodeState {
            carry: Vec::new(),
            bom_pending: false,
        };
        assert_eq!(
            decode_all(&mut st, &[0xe1, 0x80, 0xc0], &plain).unwrap(),
            "\u{fffd}\u{fffd}"
        );
        st = DecodeState {
            carry: Vec::new(),
            bom_pending: false,
        };
        assert_eq!(decode_all(&mut st, &[0xc0], &plain).unwrap(), "\u{fffd}");
        // A complete prefix plus an incomplete tail: tail is carried.
        let stream = opts(false, false, false);
        st = DecodeState {
            carry: Vec::new(),
            bom_pending: false,
        };
        assert_eq!(
            decode_all(&mut st, &[b'a', 0xf0, 0x9f, 0x98], &stream).unwrap(),
            "a"
        );
        assert_eq!(st.carry, [0xf0, 0x9f, 0x98]);
        assert_eq!(decode_all(&mut st, &[0x80], &plain).unwrap(), "\u{1f600}");
        // The same bytes flushed alone become one U+FFFD.
        st = DecodeState {
            carry: vec![0xf0, 0x9f, 0x98],
            bom_pending: false,
        };
        assert_eq!(decode_all(&mut st, b"", &plain).unwrap(), "\u{fffd}");
        // A chunk boundary inside an invalid sequence keeps the counts.
        st = DecodeState {
            carry: Vec::new(),
            bom_pending: false,
        };
        assert_eq!(decode_all(&mut st, &[0xe1, 0x80], &stream).unwrap(), "");
        assert_eq!(
            decode_all(&mut st, &[0xc0], &plain).unwrap(),
            "\u{fffd}\u{fffd}"
        );
    }

    #[test]
    fn decode_fatal_throws_instead_of_replacing() {
        let fatal = opts(true, false, true);
        let mut st = DecodeState {
            carry: Vec::new(),
            bom_pending: false,
        };
        assert!(decode_all(&mut st, &[0xff], &fatal).is_err());
        st = DecodeState {
            carry: Vec::new(),
            bom_pending: false,
        };
        assert_eq!(decode_all(&mut st, b"ok", &fatal).unwrap(), "ok");
        // An incomplete tail at flush time is an error too. 0xe1 0x80 is a
        // valid prefix (unlike 0xe0 0x80, which 0xe0's range rules reject).
        let fatal_stream = opts(true, false, false);
        st = DecodeState {
            carry: Vec::new(),
            bom_pending: false,
        };
        assert!(decode_all(&mut st, &[0xe1, 0x80], &fatal_stream).is_ok());
        st = DecodeState {
            carry: vec![0xe1, 0x80],
            bom_pending: false,
        };
        assert!(decode_all(&mut st, b"", &fatal).is_err());
    }

    #[test]
    fn decode_strips_only_a_leading_complete_bom() {
        let mut st = DecodeState {
            carry: Vec::new(),
            bom_pending: true,
        };
        assert_eq!(
            decode_all(
                &mut st,
                &[0xef, 0xbb, 0xbf, b'a'],
                &opts(false, false, true)
            )
            .unwrap(),
            "a"
        );
        // A BOM arriving split across stream chunks is still leading.
        let stream = opts(false, false, false);
        st = DecodeState {
            carry: Vec::new(),
            bom_pending: true,
        };
        assert_eq!(decode_all(&mut st, &[0xef], &stream).unwrap(), "");
        assert!(st.bom_pending);
        // The completing chunk strips the BOM and decodes what follows.
        assert_eq!(
            decode_all(&mut st, &[0xbb, 0xbf, b'a'], &stream).unwrap(),
            "a"
        );
        // An undecided BOM prefix flushed alone is ordinary input.
        st = DecodeState {
            carry: vec![0xef],
            bom_pending: true,
        };
        assert_eq!(
            decode_all(&mut st, b"", &opts(false, false, true)).unwrap(),
            "\u{fffd}"
        );
        // Mid-stream U+FEFF survives, and ignoreBOM keeps a leading one.
        st = DecodeState {
            carry: Vec::new(),
            bom_pending: true,
        };
        assert_eq!(
            decode_all(
                &mut st,
                &[b'a', 0xef, 0xbb, 0xbf],
                &opts(false, false, true)
            )
            .unwrap(),
            "a\u{feff}"
        );
        st = DecodeState {
            carry: Vec::new(),
            bom_pending: true,
        };
        assert_eq!(
            decode_all(&mut st, &[0xef, 0xbb, 0xbf, b'a'], &opts(false, true, true)).unwrap(),
            "\u{feff}a"
        );
    }

    #[test]
    fn encode_counts_units_and_stops_whole() {
        let cells = |n: usize| vec![Cell::new(0u8); n];
        let ascii = |s: &'static str| move |i: usize| u16::from(s.as_bytes()[i]);

        let (read, written) = encode_into_cells(2, ascii("ab"), &cells(2));
        assert_eq!((read, written), (2, 2));
        let (read, written) = encode_into_cells(2, ascii("ab"), &cells(1));
        assert_eq!((read, written), (1, 1));

        // A surrogate pair consumes two units as one code point.
        let pair: Vec<u16> = "\u{1f600}".encode_utf16().collect();
        let (read, written) = encode_into_cells(pair.len(), |i| pair[i], &cells(4));
        assert_eq!((read, written), (2, 4));
        let (read, written) = encode_into_cells(pair.len(), |i| pair[i], &cells(3));
        assert_eq!((read, written), (0, 0));

        // Unpaired surrogates encode as U+FFFD (3 bytes), one unit each.
        let (read, written) = encode_into_cells(1, |_| 0xd800, &cells(3));
        assert_eq!((read, written), (1, 3));
        let (read, written) = encode_into_cells(1, |_| 0xdc00, &cells(4));
        assert_eq!((read, written), (1, 3));

        // push_units mirrors the same steps into a growing buffer.
        let mut out = Vec::new();
        push_units(3, ascii("abc"), &mut out).unwrap();
        assert_eq!(out, b"abc");
        let surrogate = [0xd800u16];
        push_units(1, |i| surrogate.get(i).copied().unwrap_or(0), &mut out).unwrap();
        assert_eq!(out, b"abc\xef\xbf\xbd");
    }
}
