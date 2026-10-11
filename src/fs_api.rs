//! Guest-facing vm.fs callbacks: argument conversion in, results and
//! errors out. Every conversion is fallible where V8 can refuse (strings
//! past the engine's maximum length, toString that throws, detached
//! buffers), and none of them panic: a panic inside an extern "C"
//! callback would abort the whole process. Paths containing lone
//! surrogates are rejected here, where the raw UTF-16 is still visible;
//! a genuine U+FFFD character is a perfectly legal path byte.

use crate::diag::{throw, to_rust_string};
use crate::fs::Stat;
use crate::guest_cb;
use crate::snapshot;
use crate::world::world_mut;

/// Getting a guest string can end three ways: it converts, the
/// conversion itself threw (a toString override can raise - the caller
/// returns and leaves the pending exception alone), or it is a path
/// containing an unpaired surrogate, which Rust cannot represent and
/// which must not silently become U+FFFD and alias another path.
enum PathArg {
    Text(String),
    Threw,
    LoneSurrogate,
}

fn arg_path(scope: &mut v8::HandleScope, v: &v8::Value) -> PathArg {
    let s = match v.to_string(scope) {
        Some(s) => s,
        None => return PathArg::Threw,
    };
    if has_lone_surrogate(&s, scope) {
        return PathArg::LoneSurrogate;
    }
    PathArg::Text(s.to_rust_string_lossy(scope))
}

/// UTF-16 units per scan chunk: bounds the scratch buffer so a huge
/// path argument cannot allocate unboundedly outside --memory.
const SURROGATE_CHUNK: usize = 4096;

/// True when the raw UTF-16 code units contain an unpaired surrogate.
/// Read through String::write, which reports the code units verbatim
/// (the lossy Rust conversion would already have replaced them), in
/// fixed-size chunks from `start`.
fn has_lone_surrogate(s: &v8::String, scope: &mut v8::HandleScope) -> bool {
    let len = s.length();
    let mut scan = SurrogateScan::default();
    let mut buf = vec![0u16; SURROGATE_CHUNK.min(len)];
    let mut start = 0;
    while start < len {
        let take = SURROGATE_CHUNK.min(len - start);
        let wrote = s.write(scope, &mut buf[..take], start, v8::WriteOptions::NO_NULL_TERMINATION);
        if scan.feed(&buf[..wrote]) {
            return true;
        }
        start += wrote;
    }
    // A lead dangling at the very end has no next chunk to pair with.
    scan.pending_lead
}

/// Streaming surrogate scan: a lead surrogate must be followed by a
/// trail surrogate, and a trail must be preceded by a lead. A lead at a
/// chunk boundary stays pending until the next chunk decides.
#[derive(Default)]
struct SurrogateScan {
    pending_lead: bool,
}

impl SurrogateScan {
    fn feed(&mut self, units: &[u16]) -> bool {
        let mut i = 0;
        if self.pending_lead {
            match units.first() {
                Some(next) if (0xdc00..0xe000).contains(next) => i = 1,
                _ => return true,
            }
            self.pending_lead = false;
        }
        while i < units.len() {
            let u = units[i];
            if (0xd800..0xdc00).contains(&u) {
                match units.get(i + 1) {
                    Some(next) if (0xdc00..0xe000).contains(next) => i += 1,
                    None => self.pending_lead = true,
                    _ => return true,
                }
            } else if (0xdc00..0xe000).contains(&u) {
                return true;
            }
            i += 1;
        }
        false
    }
}

/// One-shot scan over a whole unit slice: a lead dangling at the very
/// end is lone, because there is no next chunk to pair it with.
/// (Test-facing; the callback path streams through SurrogateScan.)
#[cfg(test)]
fn lone_surrogate_in(units: &[u16]) -> bool {
    let mut scan = SurrogateScan::default();
    scan.feed(units) || scan.pending_lead
}

/// Reads a path argument, propagating a thrown conversion and rejecting
/// lone surrogates with a clear error.
macro_rules! path_arg {
    ($scope:ident, $args:ident, $i:expr) => {
        match arg_path($scope, &$args.get($i)) {
            PathArg::Text(p) => p,
            PathArg::Threw => return,
            PathArg::LoneSurrogate => {
                return throw($scope, "path contains lone surrogates")
            }
        }
    };
}

/// Converts a non-path argument (write data, restore payload) to a Rust
/// string; `None` means the conversion threw and the pending guest
/// exception should propagate.
fn arg_text(scope: &mut v8::HandleScope, v: &v8::Value) -> Option<String> {
    let s = v.to_string(scope)?;
    Some(s.to_rust_string_lossy(scope))
}

/// True when a byte length is within a string size limit. Split out so
/// the boundary is testable without building a half-gigabyte string;
/// new_from_utf8 remains the final authority.
fn fits_v8_string(len: usize, limit: usize) -> bool {
    len <= limit
}

/// Makes a V8 string, or None when the content exceeds V8's maximum
/// string length (~536 M characters); callers turn that into a guest
/// Error instead of aborting the process.
fn str_local<'s>(scope: &mut v8::HandleScope<'s>, s: &str) -> Option<v8::Local<'s, v8::String>> {
    if !fits_v8_string(s.len(), v8::String::MAX_LENGTH) {
        return None;
    }
    v8::String::new_from_utf8(scope, s.as_bytes(), v8::NewStringType::Normal)
}

/// Builds a string array. Entries are paths, capped far below the V8
/// string limit, so the empty-string fallback is unreachable in practice.
fn str_array<'s>(scope: &mut v8::HandleScope<'s>, names: Vec<String>) -> v8::Local<'s, v8::Array> {
    let array = v8::Array::new(scope, names.len() as i32);
    for (i, name) in names.iter().enumerate() {
        let s = str_local(scope, name).unwrap_or_else(|| v8::String::empty(scope));
        array.set_index(scope, i as u32, s.into());
    }
    array
}

/// Reads a boolean option (`{recursive: true}` style) off an argument.
fn opt_flag(
    scope: &mut v8::HandleScope,
    args: &v8::FunctionCallbackArguments,
    idx: i32,
    name: &str,
) -> bool {
    let v = args.get(idx);
    if !v.is_object() {
        return false;
    }
    match v8::Local::<v8::Object>::try_from(v) {
        Ok(obj) => {
            let key = str_local(scope, name).unwrap_or_else(|| v8::String::empty(scope));
            obj.get(scope, key.into())
                .is_some_and(|flag| flag.boolean_value(scope))
        }
        Err(_) => false,
    }
}

/// The number of bytes `write` would copy for this value, computed
/// without copying: `utf8_length` for strings, the current view length
/// for buffers (a view left out of bounds by a resize reports zero,
/// exactly as the guest sees it). None for values that are not writable
/// data - those fail in arg_bytes with the type error.
fn data_len(scope: &mut v8::HandleScope, v: &v8::Local<v8::Value>) -> Option<usize> {
    if v.is_string() {
        return v.to_string(scope).map(|s| s.utf8_length(scope));
    }
    if v.is_array_buffer_view() {
        return v8::Local::<v8::ArrayBufferView>::try_from(*v)
            .ok()
            .map(|view| view.byte_length());
    }
    if v.is_array_buffer() {
        return v8::Local::<v8::ArrayBuffer>::try_from(*v)
            .ok()
            .map(|buffer| buffer.byte_length());
    }
    if v.is_shared_array_buffer() {
        return v8::Local::<v8::SharedArrayBuffer>::try_from(*v)
            .ok()
            .map(|buffer| buffer.byte_length());
    }
    None
}

/// Only detachment is an error. A view left out of bounds by a
/// resizable-buffer shrink behaves the way JS shows it: length zero, so
/// the write stores an empty file.
fn check_view(
    scope: &mut v8::HandleScope,
    view: &v8::Local<v8::ArrayBufferView>,
) -> Result<(), String> {
    match view.buffer(scope) {
        Some(buffer) if buffer.was_detached() => {
            Err("cannot write from a detached ArrayBuffer".to_string())
        }
        _ => Ok(()),
    }
}

/// Extracts write() data: a string's UTF-8 bytes, a typed array's or
/// DataView's contents, or a whole (possibly shared) ArrayBuffer. Views
/// are copied with their byte_offset/length honored, so a small view over
/// a large buffer writes only what it can see. SharedArrayBuffer contents
/// are read as a snapshot; v8vm guests are single-threaded, so no other
/// thread can mutate them concurrently.
fn arg_bytes(scope: &mut v8::HandleScope, v: v8::Local<v8::Value>) -> Result<Vec<u8>, String> {
    if v.is_string() {
        return Ok(to_rust_string(scope, &v).into_bytes());
    }
    if v.is_array_buffer_view() {
        let view = v8::Local::<v8::ArrayBufferView>::try_from(v)
            .map_err(|_| BAD_WRITE_DATA.to_string())?;
        check_view(scope, &view)?;
        let mut buf = vec![0u8; view.byte_length()];
        view.copy_contents(&mut buf);
        return Ok(buf);
    }
    if v.is_array_buffer() {
        let buffer = v8::Local::<v8::ArrayBuffer>::try_from(v)
            .map_err(|_| BAD_WRITE_DATA.to_string())?;
        if buffer.was_detached() {
            return Err("cannot write from a detached ArrayBuffer".to_string());
        }
        let view = v8::Uint8Array::new(scope, buffer, 0, buffer.byte_length())
            .ok_or("file too large to read as bytes".to_string())?;
        let mut buf = vec![0u8; view.byte_length()];
        view.copy_contents(&mut buf);
        return Ok(buf);
    }
    if v.is_shared_array_buffer() {
        let buffer = v8::Local::<v8::SharedArrayBuffer>::try_from(v)
            .map_err(|_| BAD_WRITE_DATA.to_string())?;
        let store = buffer.get_backing_store();
        let cells: &[std::cell::Cell<u8>] = &store;
        return Ok(cells.iter().map(std::cell::Cell::get).collect());
    }
    Err(BAD_WRITE_DATA.to_string())
}

const BAD_WRITE_DATA: &str = "write data must be a string, ArrayBuffer, or typed array";

fn stat_obj<'s>(scope: &mut v8::HandleScope<'s>, st: &Stat) -> v8::Local<'s, v8::Object> {
    let obj = v8::Object::new(scope);
    let ty = if st.is_dir { "dir" } else { "file" };
    let key = str_local(scope, "type").unwrap_or_else(|| v8::String::empty(scope));
    let ty = str_local(scope, ty).unwrap_or_else(|| v8::String::empty(scope));
    obj.set(scope, key.into(), ty.into());
    let key = str_local(scope, "size").unwrap_or_else(|| v8::String::empty(scope));
    let num = v8::Number::new(scope, st.size as f64);
    obj.set(scope, key.into(), num.into());
    obj
}

guest_cb!(fs_read, |args, rv, scope| {
    let path = path_arg!(scope, args, 0);
    match world_mut(scope).fs.read_string(&path) {
        Ok(text) => match str_local(scope, &text) {
            Some(s) => rv.set(s.into()),
            None => throw(scope, "file too large to read as a string"),
        },
        Err(e) => throw(scope, &e),
    }
});

guest_cb!(fs_read_bytes, |args, rv, scope| {
    let path = path_arg!(scope, args, 0);
    let bytes = match world_mut(scope).fs.read_bytes(&path) {
        Ok(b) => b,
        Err(e) => return throw(scope, &e),
    };
    // The backing store owns the bytes; the Uint8Array is a view over it.
    let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
    let len = store.byte_length();
    let buffer = v8::ArrayBuffer::with_backing_store(scope, &store);
    match v8::Uint8Array::new(scope, buffer, 0, len) {
        Some(view) => rv.set(view.into()),
        None => throw(scope, "file too large to read as bytes"),
    }
});

guest_cb!(fs_write, |args, _rv, scope| {
    let path = path_arg!(scope, args, 0);
    let data = args.get(1);
    // Reject a giant string or buffer against the quota before copying.
    if let Some(len) = data_len(scope, &data) {
        if let Err(e) = world_mut(scope).fs.probe_write(&path, len) {
            return throw(scope, &e);
        }
    }
    match arg_bytes(scope, data) {
        Ok(bytes) => {
            if let Err(e) = world_mut(scope).fs.write(&path, bytes) {
                throw(scope, &e);
            }
        }
        Err(e) => throw(scope, &e),
    }
});

guest_cb!(fs_list, |args, rv, scope| {
    let dir = if args.length() > 0 && !args.get(0).is_undefined() {
        path_arg!(scope, args, 0)
    } else {
        "/".to_string()
    };
    match world_mut(scope).fs.list(&dir) {
        Ok(names) => rv.set(str_array(scope, names).into()),
        Err(e) => throw(scope, &e),
    }
});

guest_cb!(fs_remove, |args, _rv, scope| {
    let path = path_arg!(scope, args, 0);
    let recursive = opt_flag(scope, &args, 1, "recursive");
    if let Err(e) = world_mut(scope).fs.remove(&path, recursive) {
        throw(scope, &e);
    }
});

guest_cb!(fs_mkdir, |args, _rv, scope| {
    let path = path_arg!(scope, args, 0);
    let recursive = opt_flag(scope, &args, 1, "recursive");
    if let Err(e) = world_mut(scope).fs.mkdir(&path, recursive) {
        throw(scope, &e);
    }
});

guest_cb!(fs_exists, |args, rv, scope| {
    // Like any other nonsense path, a lone surrogate here just means no.
    let hit = match arg_path(scope, &args.get(0)) {
        PathArg::Text(p) => world_mut(scope).fs.exists(&p),
        PathArg::Threw => return,
        PathArg::LoneSurrogate => false,
    };
    rv.set(v8::Boolean::new(scope, hit).into());
});

guest_cb!(fs_stat, |args, rv, scope| {
    let path = path_arg!(scope, args, 0);
    match world_mut(scope).fs.stat(&path) {
        Ok(st) => rv.set(stat_obj(scope, &st).into()),
        Err(e) => throw(scope, &e),
    }
});

guest_cb!(fs_readdir, |args, rv, scope| {
    let path = path_arg!(scope, args, 0);
    match world_mut(scope).fs.readdir(&path) {
        Ok(names) => rv.set(str_array(scope, names).into()),
        Err(e) => throw(scope, &e),
    }
});

guest_cb!(fs_rename, |args, _rv, scope| {
    // One at a time: if `from`'s conversion throws, `to`'s must not run.
    let from = path_arg!(scope, args, 0);
    let to = path_arg!(scope, args, 1);
    if let Err(e) = world_mut(scope).fs.rename(&from, &to) {
        throw(scope, &e);
    }
});

guest_cb!(fs_snapshot, |_args, rv, scope| {
    let fs = &world_mut(scope).fs;
    match snapshot::encode(&fs.files, &fs.dirs) {
        Ok(json) => match str_local(scope, &json) {
            Some(s) => rv.set(s.into()),
            None => throw(scope, "snapshot too large"),
        },
        Err(e) => throw(scope, &e),
    }
});

guest_cb!(fs_restore, |args, _rv, scope| {
    let Some(json) = arg_text(scope, &args.get(0)) else { return };
    if let Err(e) = world_mut(scope).fs.restore(&json) {
        throw(scope, &e);
    }
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lone_surrogate_scan_matches_utf16_pairing() {
        assert!(!lone_surrogate_in(&[]));
        assert!(!lone_surrogate_in(&[0x61, 0x62]));
        assert!(!lone_surrogate_in(&[0xd800, 0xdc00])); // a valid pair
        assert!(lone_surrogate_in(&[0xd800])); // lead alone
        assert!(lone_surrogate_in(&[0xdc00])); // trail alone
        assert!(lone_surrogate_in(&[0x61, 0xd800, 0x62])); // lead then BMP
        assert!(lone_surrogate_in(&[0xd800, 0xd800])); // lead, lead
        assert!(!lone_surrogate_in(&[0xd83d, 0xde00, 0xfffd])); // emoji + U+FFFD
        assert!(!lone_surrogate_in(&[0xfffd, 0xfffd])); // genuine U+FFFD is fine
    }

    #[test]
    fn surrogate_scan_carries_leads_across_chunk_boundaries() {
        // A pair split across chunks is still a pair.
        let mut scan = SurrogateScan::default();
        assert!(!scan.feed(&[0x61, 0xd800]));
        assert!(!scan.feed(&[0xdc00, 0x62]));
        // A lead at the end of the last chunk is lone.
        let mut scan = SurrogateScan::default();
        assert!(!scan.feed(&[0xd800]));
        // A trail opening the next chunk without a pending lead is lone.
        let mut scan = SurrogateScan::default();
        assert!(!scan.feed(&[0x61]));
        assert!(scan.feed(&[0xdc00]));
        // A pending lead met by a BMP char is lone.
        let mut scan = SurrogateScan::default();
        assert!(!scan.feed(&[0xd800]));
        assert!(scan.feed(&[0x41]));
        // Feeding the scan in 2-unit slices matches the one-shot scan.
        let units: Vec<u16> = [0x61u16, 0xd83d, 0xde00, 0xd800, 0x62, 0xdc00]
            .into_iter()
            .cycle()
            .take(200)
            .collect();
        let mut scan = SurrogateScan::default();
        let mut streamed = false;
        for pair in units.chunks(2) {
            if scan.feed(pair) {
                streamed = true;
                break;
            }
        }
        assert_eq!(streamed, lone_surrogate_in(&units));
    }

    #[test]
    fn v8_string_limit_is_inclusive_at_the_boundary() {
        // Injected limit so the error path is exercised without a
        // half-gigabyte allocation; the real call site passes
        // v8::String::MAX_LENGTH.
        assert!(fits_v8_string(1024, 1024));
        assert!(!fits_v8_string(1025, 1024));
        assert!(fits_v8_string(0, 0));
        assert!(fits_v8_string(v8::String::MAX_LENGTH, v8::String::MAX_LENGTH));
        assert!(!fits_v8_string(
            v8::String::MAX_LENGTH + 1,
            v8::String::MAX_LENGTH
        ));
    }
}
