//! Guest-facing web-platform APIs: atob/btoa, the byte level of
//! TextEncoder and TextDecoder (the JS classes live in prelude.js and add
//! only per-instance streaming state), structuredClone over V8's own
//! ValueSerializer, performance.now on the real clock, and the seed-driven
//! Math.random. Everything byte-shaped is a small pure function here so it
//! can be unit-tested without V8.

use std::cell::Cell;
use std::sync::LazyLock;
use std::time::Instant;

use crate::diag::throw;
use crate::guest_cb;
use crate::world::world_mut;
use crate::Config;

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

/// Throws an Error whose `name` property is set: without a DOMException
/// class this keeps `catch (e) => e.name` checks against the platform
/// error names (DataCloneError, InvalidCharacterError) working.
fn throw_named(scope: &mut v8::HandleScope, name: &str, msg: &str) {
    let m = v8_str(scope, msg);
    let exc = v8::Exception::error(scope, m);
    match v8::Local::<v8::Object>::try_from(exc) {
        Ok(obj) => {
            let name_val = v8_str(scope, name);
            set_prop(obj, scope, "name", name_val.into());
            scope.throw_exception(obj.into());
        }
        Err(_) => {
            scope.throw_exception(exc);
        }
    }
}

// ---- base64 (btoa / atob) ----

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_value(byte: u8) -> Option<u8> {
    B64_ALPHABET.iter().position(|&c| c == byte).map(|i| i as u8)
}

/// Standard base64 with padding, the inverse of b64_decode_forgiving for
/// valid inputs.
fn b64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let b0 = u32::from(group[0]);
        let b1 = group.get(1).map_or(0, |&b| u32::from(b));
        let b2 = group.get(2).map_or(0, |&b| u32::from(b));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHABET[(n >> 18) as usize & 0x3f] as char);
        out.push(B64_ALPHABET[(n >> 12) as usize & 0x3f] as char);
        out.push(if group.len() > 1 { B64_ALPHABET[(n >> 6) as usize & 0x3f] as char } else { '=' });
        out.push(if group.len() > 2 { B64_ALPHABET[n as usize & 0x3f] as char } else { '=' });
    }
    out
}

/// Forgiving-base64 decode (infra.spec.whatwg.org#forgiving-base64):
/// ASCII whitespace is ignored, "=" is accepted only as the final quad's
/// padding (at most two of it), and a remnant of one character is
/// malformed. Trailing bits are not checked, matching atob.
fn b64_decode_forgiving(input: &str) -> Result<Vec<u8>, ()> {
    let stripped: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
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
    let mut out = Vec::with_capacity(data.len().div_ceil(4) * 3);
    let (chunks, remainder) = data.as_chunks::<4>();
    for group in chunks {
        let mut v = [0u8; 4];
        for (i, &b) in group.iter().enumerate() {
            v[i] = b64_value(b).ok_or(())?;
        }
        out.extend_from_slice(&[v[0] << 2 | v[1] >> 4, v[1] << 4 | v[2] >> 2, v[2] << 6 | v[3]]);
    }
    match remainder {
        [a, b] => out.push(b64_value(*a).ok_or(())? << 2 | b64_value(*b).ok_or(())? >> 4),
        [a, b, c] => {
            let (x, y, z) = (
                b64_value(*a).ok_or(())?,
                b64_value(*b).ok_or(())?,
                b64_value(*c).ok_or(())?,
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
fn latin1_units(scope: &mut v8::HandleScope, s: v8::Local<v8::String>) -> Result<Vec<u8>, ()> {
    let view = v8::ValueView::new(scope, s);
    match view.data() {
        v8::ValueViewData::OneByte(bytes) => Ok(bytes.to_vec()),
        v8::ValueViewData::TwoByte(units) => {
            units.iter().map(|&u| u8::try_from(u).map_err(|_| ())).collect()
        }
    }
}

guest_cb!(btoa, |args, rv, scope| {
    let Some(s) = args.get(0).to_string(scope) else { return };
    match latin1_units(scope, s) {
        Ok(bytes) => {
            let b64 = b64_encode(&bytes);
            rv.set(v8_str(scope, &b64).into());
        }
        Err(()) => throw_named(
            scope,
            "InvalidCharacterError",
            "btoa: string contains characters outside of Latin-1",
        ),
    }
});

guest_cb!(atob, |args, rv, scope| {
    let Some(s) = args.get(0).to_string(scope) else { return };
    match b64_decode_forgiving(&s.to_rust_string_lossy(scope)) {
        Ok(bytes) => match v8::String::new_from_one_byte(scope, &bytes, v8::NewStringType::Normal) {
            Some(out) => rv.set(out.into()),
            None => throw(scope, "decoded data too large"),
        },
        Err(()) => throw_named(scope, "InvalidCharacterError", "atob: invalid base64 input"),
    }
});

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

// ---- structuredClone ----

/// Serializer delegate: V8 refuses functions, symbols and other
/// non-cloneable values by asking the delegate to raise the error.
struct CloneSerializer;

impl v8::ValueSerializerImpl for CloneSerializer {
    fn throw_data_clone_error<'s>(
        &self,
        scope: &mut v8::HandleScope<'s>,
        message: v8::Local<'s, v8::String>,
    ) {
        let exc = v8::Exception::error(scope, message);
        match v8::Local::<v8::Object>::try_from(exc) {
            Ok(obj) => {
                let name_val = v8_str(scope, "DataCloneError");
                set_prop(obj, scope, "name", name_val.into());
                scope.throw_exception(obj.into());
            }
            Err(_) => {
                scope.throw_exception(exc);
            }
        }
    }

    fn get_wasm_module_transfer_id(
        &self,
        scope: &mut v8::HandleScope<'_>,
        _module: v8::Local<'_, v8::WasmModuleObject>,
    ) -> Option<u32> {
        // There is no transfer registry to move a module within the run,
        // so it fails the way other uncloneables do.
        let msg = v8_str(scope, "WebAssembly.Module could not be cloned");
        self.throw_data_clone_error(scope, msg);
        None
    }
}

/// Deserializer delegate: the ids the defaults refuse (host objects,
/// shared buffers, wasm modules) are never written by CloneSerializer.
struct CloneDeserializer;
impl v8::ValueDeserializerImpl for CloneDeserializer {}

/// True when options carries a non-empty `transfer` list: nothing here can
/// be transferred (transfers would detach buffers across the clone), so
/// the call fails the way browsers fail unsupported transfers.
fn transfer_requested(scope: &mut v8::HandleScope, opts: &v8::Local<v8::Value>) -> bool {
    let Ok(obj) = v8::Local::<v8::Object>::try_from(*opts) else { return false };
    let key = v8_str(scope, "transfer");
    let list = obj.get(scope, key.into()).unwrap_or_else(|| v8::undefined(scope).into());
    v8::Local::<v8::Array>::try_from(list).is_ok_and(|a| a.length() > 0)
}

guest_cb!(structured_clone, |args, rv, scope| {
    if transfer_requested(scope, &args.get(1)) {
        return throw_named(scope, "DataCloneError", "transfer is not supported");
    }
    let value = args.get(0);
    let context = scope.get_current_context();
    let ser = v8::ValueSerializer::new(scope, Box::new(CloneSerializer));
    let mut tc = v8::TryCatch::new(scope);
    // The wire-format header must precede the value (the v8 crate tests
    // pair write_header with read_header).
    use v8::ValueSerializerHelper as _;
    ser.write_header();
    if ser.write_value(context, value) != Some(true) {
        // The delegate already threw DataCloneError; a TryCatch that drops
        // a caught exception swallows it, so it must be rethrown here.
        if !tc.has_caught() {
            throw_named(&mut tc, "DataCloneError", "value could not be cloned");
        }
        tc.rethrow();
        return;
    }
    let data = ser.release();
    let de = v8::ValueDeserializer::new(&mut tc, Box::new(CloneDeserializer), &data);
    // The header (wire-format version) precedes any value; read it first.
    use v8::ValueDeserializerHelper as _;
    if de.read_header(context) != Some(true) {
        return throw(&mut tc, "structuredClone: could not decode the serialized value");
    }
    match de.read_value(context) {
        Some(v) => rv.set(v),
        None => {
            if !tc.has_caught() {
                throw(&mut tc, "structuredClone: could not deserialize the value");
            }
            tc.rethrow();
        }
    }
});

// ---- performance.now ----

/// performance.now()'s zero point: process start, so values are monotonic
/// (Instant) and comparable across runs in one process.
static PERFORMANCE_ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);

guest_cb!(performance_now, |_args, rv, scope| {
    let ms = PERFORMANCE_ORIGIN.elapsed().as_secs_f64() * 1000.0;
    rv.set(v8::Number::new(scope, ms).into());
});

// ---- seeded Math.random ----

/// The splitmix64 increment: the golden-ratio constant that keeps
/// successive counter values well spread.
const GOLDEN_GAMMA: u64 = 0x9e3779b97f4a7c15;

fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}

/// One Math.random() draw for a seeded run: advance the counter, mix it,
/// and scale the top 53 bits to a uniform value in [0, 1). Returns the
/// next counter state with the draw so callers keep the state explicitly.
pub(crate) fn random_draw(state: u64) -> (u64, f64) {
    let next = state.wrapping_add(GOLDEN_GAMMA);
    let draw = (mix64(next) >> 11) as f64 * (1.0 / (1u64 << 53) as f64);
    (next, draw)
}

guest_cb!(math_random_seeded, |_args, rv, scope| {
    // Registered only for seeded runs (install below); the guard keeps
    // this callback non-panicking if that pairing ever breaks.
    if let Some(state) = world_mut(scope).random_state.as_mut() {
        let (next, draw) = random_draw(*state);
        *state = next;
        rv.set(v8::Number::new(scope, draw).into());
    }
});

// ---- installation ----

pub(crate) fn install(scope: &mut v8::HandleScope, cfg: &Config) {
    let global = scope.get_current_context().global(scope);

    put_fn(global, scope, "atob", atob);
    put_fn(global, scope, "btoa", btoa);
    put_fn(global, scope, "structuredClone", structured_clone);
    // Helper callbacks the prelude's TextEncoder/TextDecoder classes bind
    // and then remove from the global object.
    put_fn(global, scope, "__encodeUtf8", encode_utf8);
    put_fn(global, scope, "__encodeIntoUtf8", encode_into_utf8);
    put_fn(global, scope, "__decodeUtf8", decode_utf8);

    let performance = v8::Object::new(scope);
    put_fn(performance, scope, "now", performance_now);
    set_prop(global, scope, "performance", performance.into());

    // Without a seed Math.random stays V8's own; with one, runs become
    // reproducible (per-run state lives in the World).
    if cfg.seed.is_some() {
        let math_key = v8_str(scope, "Math");
        if let Some(math) = global
            .get(scope, math_key.into())
            .and_then(|v| v8::Local::<v8::Object>::try_from(v).ok())
        {
            put_fn(math, scope, "random", math_random_seeded);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_and_forgives_padding_shapes() {
        for bytes in [&b""[..], &[0u8], &[0, 1], &[0, 1, 2], &[255, 254, 253, 0, 200]] {
            let decoded = b64_decode_forgiving(&b64_encode(bytes)).unwrap();
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
            assert!(b64_decode_forgiving(bad).is_err(), "{:?} decoded", bad);
        }
        // "=" may also close a full quad's final chunk, per the algorithm.
        assert_eq!(b64_decode_forgiving("ABC=").unwrap(), [0x00, 0x10]);
    }

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

    #[test]
    fn splitmix64_draws_are_reproducible_and_uniform_shaped() {
        // Pinned against canonical splitmix64 (independently computed):
        // any change to the mixer, the increment or the scaling breaks this.
        let pinned: Vec<f64> = (0..4).scan(42u64, |s, _| {
            let (next, d) = random_draw(*s);
            *s = next;
            Some(d)
        })
        .collect();
        assert_eq!(
            pinned,
            vec![0.7415648787718233, 0.1599103928769201, 0.27860113025513866, 0.34419071652363753]
        );
        // Different seeds differ immediately.
        let (_, a) = random_draw(7);
        let (_, b) = random_draw(8);
        assert_ne!(a, b);
        // Range and integer-bit sanity over a stretch of draws.
        let mut s = 1u64;
        for _ in 0..10_000 {
            let (next, d) = random_draw(s);
            assert!((0.0..1.0).contains(&d));
            s = next;
        }
    }
}
