//! Guest-facing vm.fs callbacks: argument conversion in, results and
//! errors out. Every conversion is fallible where V8 can refuse (strings
//! past the engine's maximum length, toString that throws, detached or
//! out-of-bounds buffers), and none of them panic: a panic inside an
//! extern "C" callback would abort the whole process.

use crate::diag::{throw, to_rust_string};
use crate::fs::Stat;
use crate::guest_cb;
use crate::snapshot;
use crate::world::world_mut;

/// Converts an argument to a Rust string. `None` means the conversion
/// itself threw (a toString override can raise): the caller returns from
/// the callback without touching the pending exception, so the guest sees
/// its own error rather than a confusing path message.
fn arg_str(scope: &mut v8::HandleScope, v: &v8::Value) -> Option<String> {
    let s = v.to_string(scope)?;
    Some(s.to_rust_string_lossy(scope))
}

/// Makes a V8 string, or None when the content exceeds V8's maximum
/// string length (~536 M characters); callers turn that into a guest
/// Error instead of aborting the process.
fn str_local<'s>(scope: &mut v8::HandleScope<'s>, s: &str) -> Option<v8::Local<'s, v8::String>> {
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

/// The number of bytes `write` would copy for this value, without copying
/// them: None for strings (already cheap to convert) and for values that
/// are not writable data at all.
fn arg_byte_len(scope: &mut v8::HandleScope, v: &v8::Local<v8::Value>) -> Option<usize> {
    if v.is_array_buffer_view() {
        let view = v8::Local::<v8::ArrayBufferView>::try_from(*v).ok()?;
        check_view(scope, &view).ok()?;
        return Some(view.byte_length());
    }
    if v.is_array_buffer() {
        let buffer = v8::Local::<v8::ArrayBuffer>::try_from(*v).ok()?;
        if buffer.was_detached() {
            return None;
        }
        return Some(buffer.byte_length());
    }
    if v.is_shared_array_buffer() {
        let buffer = v8::Local::<v8::SharedArrayBuffer>::try_from(*v).ok()?;
        return Some(buffer.byte_length());
    }
    None
}

/// Rejects views whose buffer was detached and views left out of bounds
/// by a resizable-buffer shrink: their stale offset no longer fits the
/// buffer. V8 reports the raw stored offset here, not the clamped value
/// the guest sees.
fn check_view(scope: &mut v8::HandleScope, view: &v8::Local<v8::ArrayBufferView>) -> Result<(), String> {
    let buffer = match view.buffer(scope) {
        Some(b) => b,
        None => return Ok(()), // no buffer to validate against
    };
    if buffer.was_detached() {
        return Err("cannot write from a detached ArrayBuffer".to_string());
    }
    if view.byte_offset() + view.byte_length() > buffer.byte_length() {
        return Err("cannot write from an out-of-bounds view".to_string());
    }
    Ok(())
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
    let Some(path) = arg_str(scope, &args.get(0)) else { return };
    match world_mut(scope).fs.read_string(&path) {
        Ok(text) => match str_local(scope, &text) {
            Some(s) => rv.set(s.into()),
            None => throw(scope, "file too large to read as a string"),
        },
        Err(e) => throw(scope, &e),
    }
});

guest_cb!(fs_read_bytes, |args, rv, scope| {
    let Some(path) = arg_str(scope, &args.get(0)) else { return };
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
    let Some(path) = arg_str(scope, &args.get(0)) else { return };
    let data = args.get(1);
    // Reject a giant buffer against the quota before copying it in.
    if let Some(len) = arg_byte_len(scope, &data) {
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
        let Some(d) = arg_str(scope, &args.get(0)) else { return };
        d
    } else {
        "/".to_string()
    };
    match world_mut(scope).fs.list(&dir) {
        Ok(names) => rv.set(str_array(scope, names).into()),
        Err(e) => throw(scope, &e),
    }
});

guest_cb!(fs_remove, |args, _rv, scope| {
    let Some(path) = arg_str(scope, &args.get(0)) else { return };
    let recursive = opt_flag(scope, &args, 1, "recursive");
    if let Err(e) = world_mut(scope).fs.remove(&path, recursive) {
        throw(scope, &e);
    }
});

guest_cb!(fs_mkdir, |args, _rv, scope| {
    let Some(path) = arg_str(scope, &args.get(0)) else { return };
    let recursive = opt_flag(scope, &args, 1, "recursive");
    if let Err(e) = world_mut(scope).fs.mkdir(&path, recursive) {
        throw(scope, &e);
    }
});

guest_cb!(fs_exists, |args, rv, scope| {
    let Some(path) = arg_str(scope, &args.get(0)) else { return };
    let hit = world_mut(scope).fs.exists(&path);
    rv.set(v8::Boolean::new(scope, hit).into());
});

guest_cb!(fs_stat, |args, rv, scope| {
    let Some(path) = arg_str(scope, &args.get(0)) else { return };
    match world_mut(scope).fs.stat(&path) {
        Ok(st) => rv.set(stat_obj(scope, &st).into()),
        Err(e) => throw(scope, &e),
    }
});

guest_cb!(fs_readdir, |args, rv, scope| {
    let Some(path) = arg_str(scope, &args.get(0)) else { return };
    match world_mut(scope).fs.readdir(&path) {
        Ok(names) => rv.set(str_array(scope, names).into()),
        Err(e) => throw(scope, &e),
    }
});

guest_cb!(fs_rename, |args, _rv, scope| {
    let (Some(from), Some(to)) = (arg_str(scope, &args.get(0)), arg_str(scope, &args.get(1)))
    else {
        return;
    };
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
    let Some(json) = arg_str(scope, &args.get(0)) else { return };
    if let Err(e) = world_mut(scope).fs.restore(&json) {
        throw(scope, &e);
    }
});
