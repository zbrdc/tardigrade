//! vm.fs: the RAM filesystem. Directories are explicit entries; writing a
//! file creates its missing parent directories. Guest paths are normalized
//! (`.`, `..`, duplicate and trailing slashes collapse) and always absolute;
//! paths that would escape the root are rejected. Contents are bytes:
//! `read` returns them as a lossy UTF-8 string, `readBytes` as a Uint8Array,
//! and `write` accepts a string, ArrayBuffer, or typed array. Throwing
//! read/remove, as documented.

use std::collections::{BTreeMap, BTreeSet};

use crate::diag::{throw, to_rust_string};
use crate::guest_cb;
use crate::world::world_mut;

pub(crate) type FsResult<T> = Result<T, String>;

/// Every file and directory costs this many quota bytes on top of its
/// content, covering its key and its map entry.
pub(crate) const ENTRY_OVERHEAD: u64 = 64;
/// Normalized paths at or under this UTF-8 length are accepted.
pub(crate) const MAX_PATH_BYTES: usize = 4096;
/// Upper bound on the number of files plus directories.
pub(crate) const MAX_ENTRIES: usize = 100_000;
/// The Config.fs_quota_bytes default: 64 MiB.
pub const DEFAULT_FS_QUOTA_BYTES: u64 = 64 << 20;

fn file_cost(len: usize) -> u64 {
    ENTRY_OVERHEAD + len as u64
}

/// Full recount: content bytes plus overhead per entry, root excluded.
fn usage(files: &BTreeMap<String, Vec<u8>>, dirs: &BTreeSet<String>) -> u64 {
    files.values().map(|d| file_cost(d.len())).sum::<u64>()
        + ENTRY_OVERHEAD * (dirs.len() as u64 - 1)
}

/// Parsed snapshot payload: explicit directories plus file contents.
type Snapshot = (BTreeSet<String>, Vec<(String, Vec<u8>)>);

fn parse_snapshot(json: &str) -> FsResult<Snapshot> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("invalid snapshot: {}", e))?;
    let obj = v.as_object().ok_or("invalid snapshot: expected an object")?;
    // A legacy map's keys are absolute paths, so a bare "version" key can
    // only belong to the versioned format.
    if obj.get("version").and_then(serde_json::Value::as_i64) == Some(2) {
        return parse_v2(obj);
    }
    parse_legacy(obj)
}

fn parse_v2(obj: &serde_json::Map<String, serde_json::Value>) -> FsResult<Snapshot> {
    let files = obj
        .get("files")
        .and_then(|v| v.as_object())
        .ok_or("invalid snapshot: files must be an object")?;
    let mut out = Vec::with_capacity(files.len());
    for (p, entry) in files {
        out.push((normalize(p)?, entry_bytes(entry)?));
    }
    let mut dirs = BTreeSet::new();
    if let Some(v) = obj.get("dirs") {
        let arr = v.as_array().ok_or("invalid snapshot: dirs must be an array")?;
        for d in arr {
            let s = d.as_str().ok_or("invalid snapshot: dirs entries must be strings")?;
            dirs.insert(normalize(s)?);
        }
    }
    Ok((dirs, out))
}

fn entry_bytes(entry: &serde_json::Value) -> FsResult<Vec<u8>> {
    let obj = entry
        .as_object()
        .ok_or("invalid snapshot: file entry must be {text} or {b64}")?;
    if let Some(t) = obj.get("text") {
        return t
            .as_str()
            .map(|s| s.as_bytes().to_vec())
            .ok_or("invalid snapshot: text must be a string".to_string());
    }
    if let Some(b) = obj.get("b64") {
        let s = b.as_str().ok_or("invalid snapshot: b64 must be a string")?;
        return b64::decode(s).ok_or("invalid snapshot: bad base64".to_string());
    }
    Err("invalid snapshot: file entry must be {text} or {b64}".to_string())
}

fn parse_legacy(obj: &serde_json::Map<String, serde_json::Value>) -> FsResult<Snapshot> {
    let mut files = Vec::with_capacity(obj.len());
    for (p, v) in obj {
        let s = v.as_str().ok_or("invalid snapshot: old-format values must be strings")?;
        files.push((normalize(p)?, s.as_bytes().to_vec()));
    }
    Ok((BTreeSet::new(), files))
}

/// Collapses `.`/`..`/duplicate slashes and rejects anything that is not an
/// absolute path inside the sandbox root.
fn normalize(path: &str) -> FsResult<String> {
    if path.contains('\0') {
        return Err(format!("path contains NUL: {}", path));
    }
    if !path.starts_with('/') {
        return Err(format!("path must be absolute: {}", path));
    }
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                // Popping past the root means the path wants out.
                if parts.pop().is_none() {
                    return Err(format!("path escapes root: {}", path));
                }
            }
            other => parts.push(other),
        }
    }
    let norm = if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    };
    if norm.len() > MAX_PATH_BYTES {
        return Err(format!("path too long: {} bytes (max {})", norm.len(), MAX_PATH_BYTES));
    }
    Ok(norm)
}

/// Proper ancestors of a normalized path, shallowest first:
/// `"/a/b/c"` -> `["/a", "/a/b"]`, `"/a"` and `"/"` -> `[]`.
fn ancestors(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, _) in path.match_indices('/') {
        if i > 0 {
            out.push(path[..i].to_string());
        }
    }
    out
}

/// Parent directory of a normalized path; always `/` one level deep.
fn parent(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => path[..i].to_string(),
    }
}

/// Prefix all direct children of `dir` live under (root's is `/` itself).
fn child_prefix(dir: &str) -> String {
    if dir == "/" {
        "/".to_string()
    } else {
        format!("{}/", dir)
    }
}

/// First path segment below `prefix`, or `None` if not a child.
fn child_name<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    path.strip_prefix(prefix)
        .and_then(|rest| rest.split('/').next())
        .filter(|name| !name.is_empty())
}

/// The filesystem state: byte-valued files plus the set of directories
/// that exist in their own right (root included). Every file's ancestors
/// are directories; `write` and `restore` maintain that invariant. `used`
/// tracks content bytes plus ENTRY_OVERHEAD per entry so growth can be
/// checked before anything mutates.
pub(crate) struct RamFs {
    files: BTreeMap<String, Vec<u8>>,
    dirs: BTreeSet<String>,
    /// 0 disables the limit.
    quota: u64,
    used: u64,
}

impl RamFs {
    pub(crate) fn new(quota: u64) -> Self {
        RamFs {
            files: BTreeMap::new(),
            dirs: BTreeSet::from(["/".to_string()]),
            quota,
            used: 0,
        }
    }

    fn entries(&self) -> usize {
        // The root itself is not a counted entry; `used` mirrors this.
        self.files.len() + self.dirs.len() - 1
    }

    /// Growth gate: fails without mutating when the post-operation state
    /// would pass the entry cap or the quota.
    fn reserve(&self, used_after: u64, entries_after: usize) -> FsResult<()> {
        if entries_after > MAX_ENTRIES {
            return Err(format!("fs entry limit exceeded (max {})", MAX_ENTRIES));
        }
        if self.quota != 0 && used_after > self.quota {
            return Err("fs quota exceeded".to_string());
        }
        Ok(())
    }

    fn is_dir(&self, path: &str) -> bool {
        self.dirs.contains(path)
    }

    fn entries_under(&self, prefix: &str) -> (Vec<String>, Vec<String>) {
        let take = |k: &String| k.starts_with(prefix);
        let files = self
            .files
            .range(prefix.to_string()..)
            .map(|(k, _)| k.clone())
            .take_while(take)
            .collect();
        let dirs = self
            .dirs
            .range(prefix.to_string()..)
            .cloned()
            .take_while(take)
            .collect();
        (files, dirs)
    }

    /// Immediate child names (files and directories), sorted.
    fn child_names(&self, dir: &str) -> BTreeSet<String> {
        let prefix = child_prefix(dir);
        let (files, dirs) = self.entries_under(&prefix);
        files
            .iter()
            .chain(dirs.iter())
            .filter_map(|p| child_name(p, &prefix))
            .map(str::to_string)
            .collect()
    }

    /// Ancestors of `path` that do not exist yet; a file in the way is an
    /// error because a path cannot be both a file and a directory.
    fn missing_dirs(&self, path: &str) -> FsResult<Vec<String>> {
        let mut out = Vec::new();
        for anc in ancestors(path) {
            if self.files.contains_key(&anc) {
                return Err(format!("not a directory: {}", anc));
            }
            if !self.is_dir(&anc) {
                out.push(anc);
            }
        }
        Ok(out)
    }

    fn insert_dirs(&mut self, dirs: Vec<String>) {
        for d in dirs {
            self.dirs.insert(d);
        }
    }

    pub(crate) fn read_string(&self, path: &str) -> FsResult<String> {
        // Lossy by design: binary files stay readable as strings with
        // U+FFFD where the bytes are not valid UTF-8.
        Ok(String::from_utf8_lossy(&self.read_bytes(path)?).into_owned())
    }

    pub(crate) fn read_bytes(&self, path: &str) -> FsResult<Vec<u8>> {
        let p = normalize(path)?;
        match self.files.get(&p) {
            Some(data) => Ok(data.clone()),
            None if self.is_dir(&p) => Err(format!("is a directory: {}", p)),
            None => Err(format!("no such file: {}", p)),
        }
    }

    /// Writing creates missing parent directories (documented guest rule)
    /// and replaces an existing file at `path` wholesale.
    pub(crate) fn write(&mut self, path: &str, data: Vec<u8>) -> FsResult<()> {
        let p = normalize(path)?;
        if self.is_dir(&p) {
            return Err(format!("is a directory: {}", p));
        }
        let missing = self.missing_dirs(&p)?;
        let old = self.files.get(&p).map_or(0, |d| file_cost(d.len()));
        let add = file_cost(data.len()) + ENTRY_OVERHEAD * missing.len() as u64;
        let new_file = usize::from(!self.files.contains_key(&p));
        self.reserve(self.used + add - old, self.entries() + missing.len() + new_file)?;
        self.insert_dirs(missing);
        self.files.insert(p, data);
        self.used += add - old;
        Ok(())
    }

    /// `recursive` mirrors Node: missing parents are created, and an
    /// existing directory is a success rather than an error.
    pub(crate) fn mkdir(&mut self, path: &str, recursive: bool) -> FsResult<()> {
        let p = normalize(path)?;
        if self.is_dir(&p) {
            return if recursive { Ok(()) } else { Err(format!("already exists: {}", p)) };
        }
        if self.files.contains_key(&p) {
            return Err(format!("already exists: {}", p));
        }
        for anc in ancestors(&p) {
            if self.files.contains_key(&anc) {
                return Err(format!("not a directory: {}", anc));
            }
            if !recursive && !self.is_dir(&anc) {
                return Err(format!("no such file: {}", anc));
            }
        }
        let missing = self.missing_dirs(&p)?;
        let add = missing.len() as u64 + 1;
        self.reserve(
            self.used + ENTRY_OVERHEAD * add,
            self.entries() + add as usize,
        )?;
        self.insert_dirs(missing);
        self.dirs.insert(p);
        self.used += ENTRY_OVERHEAD * add;
        Ok(())
    }

    /// Unlike the mutating calls, a nonsense path here just means "no".
    pub(crate) fn exists(&self, path: &str) -> bool {
        match normalize(path) {
            Ok(p) => self.files.contains_key(&p) || self.is_dir(&p),
            Err(_) => false,
        }
    }

    pub(crate) fn stat(&self, path: &str) -> FsResult<Stat> {
        let p = normalize(path)?;
        if let Some(data) = self.files.get(&p) {
            return Ok(Stat { is_dir: false, size: data.len() as u64 });
        }
        if self.is_dir(&p) {
            return Ok(Stat { is_dir: true, size: self.child_names(&p).len() as u64 });
        }
        Err(format!("no such file: {}", p))
    }

    /// Recursive full-path listing of the files under `dir` (directories are
    /// not listed); the pre-snapshot() and vm.fs.list() compatibility shape.
    pub(crate) fn list(&self, dir: &str) -> FsResult<Vec<String>> {
        let p = normalize(dir)?;
        if self.files.contains_key(&p) {
            return Err(format!("not a directory: {}", p));
        }
        let (files, _) = self.entries_under(&child_prefix(&p));
        Ok(files)
    }

    pub(crate) fn readdir(&self, path: &str) -> FsResult<Vec<String>> {
        let p = normalize(path)?;
        if self.files.contains_key(&p) {
            return Err(format!("not a directory: {}", p));
        }
        if !self.is_dir(&p) {
            return Err(format!("no such file: {}", p));
        }
        Ok(self.child_names(&p).into_iter().collect())
    }

    /// Removes a file, an empty directory, or a directory subtree when
    /// `recursive` is set. The root itself is not removable.
    pub(crate) fn remove(&mut self, path: &str, recursive: bool) -> FsResult<()> {
        let p = normalize(path)?;
        if p == "/" {
            return Err("cannot remove root".to_string());
        }
        if let Some(data) = self.files.remove(&p) {
            self.used -= file_cost(data.len());
            return Ok(());
        }
        if !self.is_dir(&p) {
            return Err(format!("no such file: {}", p));
        }
        let (files, dirs) = self.entries_under(&child_prefix(&p));
        if !recursive && !(files.is_empty() && dirs.is_empty()) {
            return Err(format!("directory not empty: {}", p));
        }
        for f in files {
            if let Some(data) = self.files.remove(&f) {
                self.used -= file_cost(data.len());
            }
        }
        let freed = ENTRY_OVERHEAD * (dirs.len() as u64 + 1);
        for d in dirs {
            self.dirs.remove(&d);
        }
        self.dirs.remove(&p);
        self.used -= freed;
        Ok(())
    }

    /// POSIX-shaped rename: a file replaces a file, a directory moves onto
    /// an empty directory, and mismatched kinds or a move into the source's
    /// own subtree are errors. The destination's parent must exist.
    pub(crate) fn rename(&mut self, from: &str, to: &str) -> FsResult<()> {
        let (f, t) = (normalize(from)?, normalize(to)?);
        if f == t {
            return Ok(());
        }
        if f == "/" {
            return Err("cannot rename root".to_string());
        }
        let tparent = parent(&t);
        if !self.is_dir(&tparent) {
            return Err(format!("no such file: {}", tparent));
        }
        let from_is_dir = self.is_dir(&f);
        if !from_is_dir && !self.files.contains_key(&f) {
            return Err(format!("no such file: {}", f));
        }
        if from_is_dir && t.starts_with(&child_prefix(&f)) {
            return Err(format!("cannot rename a directory into itself: {}", t));
        }
        if from_is_dir {
            self.check_move_fits(&f, &t)?;
        }
        let to_is_dir = self.is_dir(&t);
        let to_is_file = self.files.contains_key(&t);
        if to_is_file && from_is_dir {
            return Err(format!("not a directory: {}", t));
        }
        if to_is_dir && !from_is_dir {
            return Err(format!("is a directory: {}", t));
        }
        if to_is_dir && !self.child_names(&t).is_empty() {
            return Err(format!("directory not empty: {}", t));
        }
        if to_is_file {
            if let Some(data) = self.files.remove(&t) {
                self.used -= file_cost(data.len());
            }
        }
        if to_is_dir {
            self.dirs.remove(&t);
            self.used -= ENTRY_OVERHEAD;
        }
        if from_is_dir {
            self.move_subtree(&f, &t);
        } else if let Some(data) = self.files.remove(&f) {
            self.files.insert(t, data);
        }
        Ok(())
    }

    /// Renaming onto a longer path grows every re-keyed descendant; none
    /// of them may cross the path cap. Checked before any mutation.
    fn check_move_fits(&self, from: &str, to: &str) -> FsResult<()> {
        let growth = to.len() as isize - from.len() as isize;
        if growth <= 0 {
            return Ok(());
        }
        let prefix = child_prefix(from);
        let too_long =
            |k: &str| k.len() as isize + growth > MAX_PATH_BYTES as isize;
        let keys = self
            .files
            .keys()
            .map(String::as_str)
            .chain(self.dirs.iter().map(String::as_str));
        if keys.filter(|k| k.starts_with(&prefix)).any(too_long) {
            return Err(format!("path too long after rename (max {})", MAX_PATH_BYTES));
        }
        Ok(())
    }

    /// Snapshot format version 2:
    /// {"version":2,"files":{path:{"text":s}|{"b64":s}},"dirs":[...]}.
    /// Valid UTF-8 content is stored as text, anything else as base64;
    /// B-tree key order makes dumps deterministic. Old readers that only
    /// understand the legacy flat map can still read text-only snapshots'
    /// files through vm.fs.list/read, and this reader accepts both.
    pub(crate) fn snapshot_json(&self) -> String {
        let mut files = serde_json::Map::new();
        for (p, bytes) in &self.files {
            let entry = match std::str::from_utf8(bytes) {
                Ok(text) => serde_json::json!({ "text": text }),
                Err(_) => serde_json::json!({ "b64": b64::encode(bytes) }),
            };
            files.insert(p.clone(), entry);
        }
        let dirs: Vec<&str> = self.dirs.iter().map(String::as_str).collect();
        let v = serde_json::json!({ "version": 2, "files": files, "dirs": dirs });
        serde_json::to_string(&v).expect("a snapshot is always serializable")
    }

    /// Restores a version-2 snapshot or the legacy flat {path: text} map,
    /// merging into the current state. All-or-nothing: a snapshot that
    /// would break the caps or the quota changes nothing.
    pub(crate) fn restore(&mut self, json: &str) -> FsResult<()> {
        let (dirs, files) = parse_snapshot(json)?;
        self.apply_snapshot(dirs, files)
    }

    /// Plans the merge on copies and commits only a state that fits. Every
    /// ancestor of a restored entry becomes a directory, so stat, exists
    /// and readdir see one uniform tree even for legacy snapshots.
    fn apply_snapshot(
        &mut self,
        dirs: BTreeSet<String>,
        files: Vec<(String, Vec<u8>)>,
    ) -> FsResult<()> {
        let mut want = dirs.clone();
        for d in &dirs {
            want.extend(ancestors(d));
        }
        for (p, _) in &files {
            want.extend(ancestors(p));
        }
        let mut next_dirs = self.dirs.clone();
        let mut next_files = self.files.clone();
        for d in want {
            next_files.remove(&d);
            next_dirs.insert(d);
        }
        for (p, bytes) in files {
            if next_dirs.contains(&p) {
                return Err(format!("is a directory: {}", p));
            }
            next_files.insert(p, bytes);
        }
        let used = usage(&next_files, &next_dirs);
        if next_files.len() + next_dirs.len() - 1 > MAX_ENTRIES {
            return Err(format!("fs entry limit exceeded (max {})", MAX_ENTRIES));
        }
        if self.quota != 0 && used > self.quota {
            return Err("fs quota exceeded".to_string());
        }
        self.files = next_files;
        self.dirs = next_dirs;
        self.used = used;
        Ok(())
    }

    /// Re-keys a directory subtree under a new path.
    fn move_subtree(&mut self, from: &str, to: &str) {
        let (files, dirs) = self.entries_under(&child_prefix(from));
        for f in files {
            if let Some(data) = self.files.remove(&f) {
                self.files.insert(rekey(&f, from, to), data);
            }
        }
        for d in dirs {
            self.dirs.remove(&d);
            self.dirs.insert(rekey(&d, from, to));
        }
        self.dirs.remove(from);
        self.dirs.insert(to.to_string());
    }
}

fn rekey(path: &str, from: &str, to: &str) -> String {
    format!("{}{}", to, &path[from.len()..])
}

/// Standard base64 with padding, in ~30 lines so the snapshot format needs
/// no extra crate.
mod b64 {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub(super) fn encode(data: &[u8]) -> String {
        let digit = |v: u32| ALPHABET[(v & 63) as usize] as char;
        let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
        for chunk in data.chunks(3) {
            let mut b = [0u8; 3];
            b[..chunk.len()].copy_from_slice(chunk);
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            out.push(digit(n >> 18));
            out.push(digit(n >> 12));
            out.push(if chunk.len() > 1 { digit(n >> 6) } else { '=' });
            out.push(if chunk.len() > 2 { digit(n) } else { '=' });
        }
        out
    }

    pub(super) fn decode(s: &str) -> Option<Vec<u8>> {
        let b = s.as_bytes();
        if !b.len().is_multiple_of(4) {
            return None;
        }
        let mut out = Vec::with_capacity(b.len() / 4 * 3);
        for chunk in b.chunks(4) {
            let pad = chunk.iter().filter(|&&c| c == b'=').count();
            // Padding is only ever trailing, and at most two characters.
            if pad > 2 || chunk[..4 - pad].contains(&b'=') {
                return None;
            }
            let mut n = 0u32;
            for (i, &c) in chunk.iter().enumerate() {
                let v = if c == b'=' {
                    0
                } else {
                    ALPHABET.iter().position(|&a| a == c)? as u32
                };
                n |= v << (18 - 6 * i);
            }
            out.push((n >> 16) as u8);
            if pad < 2 {
                out.push((n >> 8) as u8);
            }
            if pad < 1 {
                out.push(n as u8);
            }
        }
        Some(out)
    }
}

/// The `stat()` result: kind plus a size that means content bytes for files
/// and immediate child count for directories.
#[derive(Debug)]
pub(crate) struct Stat {
    pub(crate) is_dir: bool,
    pub(crate) size: u64,
}

fn str_local<'s>(scope: &mut v8::HandleScope<'s>, s: &str) -> v8::Local<'s, v8::String> {
    v8::String::new_from_utf8(scope, s.as_bytes(), v8::NewStringType::Normal).unwrap()
}

fn str_array<'s>(scope: &mut v8::HandleScope<'s>, names: Vec<String>) -> v8::Local<'s, v8::Array> {
    let array = v8::Array::new(scope, names.len() as i32);
    for (i, name) in names.iter().enumerate() {
        let s = str_local(scope, name);
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
            let key = str_local(scope, name);
            obj.get(scope, key.into()).is_some_and(|flag| flag.boolean_value(scope))
        }
        Err(_) => false,
    }
}

/// Extracts write() data: a string's UTF-8 bytes, a typed array's or
/// DataView's contents, or a whole ArrayBuffer's backing store. Views are
/// copied with their byte_offset/length honored, so a small view over a
/// large buffer writes only what it can see.
fn arg_bytes(scope: &mut v8::HandleScope, v: v8::Local<v8::Value>) -> Option<Vec<u8>> {
    if v.is_string() {
        return Some(to_rust_string(scope, &v).into_bytes());
    }
    if v.is_array_buffer_view() {
        let view = v8::Local::<v8::ArrayBufferView>::try_from(v).ok()?;
        let mut buf = vec![0u8; view.byte_length()];
        view.copy_contents(&mut buf);
        return Some(buf);
    }
    if v.is_array_buffer() {
        let buffer = v8::Local::<v8::ArrayBuffer>::try_from(v).ok()?;
        let view = v8::Uint8Array::new(scope, buffer, 0, buffer.byte_length())?;
        let mut buf = vec![0u8; view.byte_length()];
        view.copy_contents(&mut buf);
        return Some(buf);
    }
    None
}

guest_cb!(fs_read, |args, rv, scope| {
    let path = to_rust_string(scope, &args.get(0));
    match world_mut(scope).fs.read_string(&path) {
        Ok(text) => rv.set(str_local(scope, &text).into()),
        Err(e) => throw(scope, &e),
    }
});

guest_cb!(fs_read_bytes, |args, rv, scope| {
    let path = to_rust_string(scope, &args.get(0));
    let bytes = match world_mut(scope).fs.read_bytes(&path) {
        Ok(b) => b,
        Err(e) => return throw(scope, &e),
    };
    // The backing store owns the bytes; the Uint8Array is a view over it.
    let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
    let len = store.byte_length();
    let buffer = v8::ArrayBuffer::with_backing_store(scope, &store);
    let view = v8::Uint8Array::new(scope, buffer, 0, len).unwrap();
    rv.set(view.into());
});

guest_cb!(fs_write, |args, _rv, scope| {
    let path = to_rust_string(scope, &args.get(0));
    let data = match arg_bytes(scope, args.get(1)) {
        Some(d) => d,
        None => return throw(scope, "write data must be a string, ArrayBuffer, or typed array"),
    };
    if let Err(e) = world_mut(scope).fs.write(&path, data) {
        throw(scope, &e);
    }
});

guest_cb!(fs_list, |args, rv, scope| {
    let dir = if args.length() > 0 && !args.get(0).is_undefined() {
        to_rust_string(scope, &args.get(0))
    } else {
        "/".to_string()
    };
    match world_mut(scope).fs.list(&dir) {
        Ok(names) => rv.set(str_array(scope, names).into()),
        Err(e) => throw(scope, &e),
    }
});

guest_cb!(fs_remove, |args, rv, scope| {
    let path = to_rust_string(scope, &args.get(0));
    let recursive = opt_flag(scope, &args, 1, "recursive");
    if let Err(e) = world_mut(scope).fs.remove(&path, recursive) {
        throw(scope, &e);
    }
    rv.set(v8::Boolean::new(scope, true).into());
});

guest_cb!(fs_mkdir, |args, _rv, scope| {
    let path = to_rust_string(scope, &args.get(0));
    let recursive = opt_flag(scope, &args, 1, "recursive");
    if let Err(e) = world_mut(scope).fs.mkdir(&path, recursive) {
        throw(scope, &e);
    }
});

guest_cb!(fs_exists, |args, rv, scope| {
    let path = to_rust_string(scope, &args.get(0));
    let hit = world_mut(scope).fs.exists(&path);
    rv.set(v8::Boolean::new(scope, hit).into());
});

guest_cb!(fs_stat, |args, rv, scope| {
    let path = to_rust_string(scope, &args.get(0));
    let st = match world_mut(scope).fs.stat(&path) {
        Ok(st) => st,
        Err(e) => return throw(scope, &e),
    };
    let obj = v8::Object::new(scope);
    let ty = if st.is_dir { "dir" } else { "file" };
    let key = str_local(scope, "type");
    let ty = str_local(scope, ty);
    obj.set(scope, key.into(), ty.into());
    let key = str_local(scope, "size");
    let num = v8::Number::new(scope, st.size as f64);
    obj.set(scope, key.into(), num.into());
    rv.set(obj.into());
});

guest_cb!(fs_readdir, |args, rv, scope| {
    let path = to_rust_string(scope, &args.get(0));
    match world_mut(scope).fs.readdir(&path) {
        Ok(names) => rv.set(str_array(scope, names).into()),
        Err(e) => throw(scope, &e),
    }
});

guest_cb!(fs_rename, |args, _rv, scope| {
    let from = to_rust_string(scope, &args.get(0));
    let to = to_rust_string(scope, &args.get(1));
    if let Err(e) = world_mut(scope).fs.rename(&from, &to) {
        throw(scope, &e);
    }
});

guest_cb!(fs_snapshot, |_args, rv, scope| {
    let json = world_mut(scope).fs.snapshot_json();
    rv.set(str_local(scope, &json).into());
});

guest_cb!(fs_restore, |args, _rv, scope| {
    let json = to_rust_string(scope, &args.get(0));
    if let Err(e) = world_mut(scope).fs.restore(&json) {
        throw(scope, &e);
    }
});

#[cfg(test)]
mod tests {
    use super::*;

    fn fs() -> RamFs {
        RamFs::new(0) // unlimited: these tests are about structure
    }

    #[test]
    fn normalize_collapses_and_rejects_escapes() {
        assert_eq!(normalize("/a//b/./c/").unwrap(), "/a/b/c");
        assert_eq!(normalize("/a/b/../c").unwrap(), "/a/c");
        assert_eq!(normalize("/../a").unwrap_err(), "path escapes root: /../a");
        assert_eq!(normalize("a/b").unwrap_err(), "path must be absolute: a/b");
        assert_eq!(normalize("///").unwrap(), "/");
        assert!(normalize("/a/\0").unwrap_err().starts_with("path contains NUL"));
    }

    #[test]
    fn write_creates_parents_and_reads_back() {
        let mut f = fs();
        f.write("/a/b/c.txt", "hi".into()).unwrap();
        assert_eq!(f.read_string("/a/b/c.txt").unwrap(), "hi");
        assert!(f.exists("/a") && f.exists("/a/b"));
        assert_eq!(f.readdir("/a").unwrap(), vec!["b".to_string()]);
    }

    #[test]
    fn write_rejects_directory_and_file_conflicts() {
        let mut f = fs();
        f.mkdir("/d", false).unwrap();
        assert_eq!(f.write("/d", "x".into()).unwrap_err(), "is a directory: /d");
        f.write("/f", "x".into()).unwrap();
        assert_eq!(f.mkdir("/f", false).unwrap_err(), "already exists: /f");
        assert_eq!(f.mkdir("/f/x", false).unwrap_err(), "not a directory: /f");
        assert_eq!(f.write("/f/x", "x".into()).unwrap_err(), "not a directory: /f");
    }

    #[test]
    fn mkdir_recursive_and_errors() {
        let mut f = fs();
        assert_eq!(f.mkdir("/a/b", false).unwrap_err(), "no such file: /a");
        f.mkdir("/a/b", true).unwrap();
        f.mkdir("/a/b", true).unwrap(); // recursive tolerates existing
        assert_eq!(f.mkdir("/a/b", false).unwrap_err(), "already exists: /a/b");
        assert!(f.exists("/a/b"));
    }

    #[test]
    fn stat_reports_kind_and_size() {
        let mut f = fs();
        f.write("/f.txt", "héllo".into()).unwrap(); // 6 UTF-8 bytes
        f.mkdir("/d", false).unwrap();
        f.write("/d/x", "1".into()).unwrap();
        f.mkdir("/d/sub", false).unwrap();
        let file = f.stat("/f.txt").unwrap();
        assert_eq!((file.is_dir, file.size), (false, 6));
        let dir = f.stat("/d").unwrap();
        assert_eq!((dir.is_dir, dir.size), (true, 2));
        assert_eq!(f.stat("/nope").unwrap_err(), "no such file: /nope");
    }

    #[test]
    fn list_is_recursive_full_paths_files_only() {
        let mut f = fs();
        f.write("/d/a.txt", "A".into()).unwrap();
        f.write("/d/e/b.txt", "B".into()).unwrap();
        f.mkdir("/d/empty", false).unwrap();
        assert_eq!(
            f.list("/d").unwrap(),
            vec!["/d/a.txt".to_string(), "/d/e/b.txt".to_string()]
        );
        assert_eq!(f.list("/d/empty").unwrap(), Vec::<String>::new());
        assert_eq!(f.list("/d/a.txt").unwrap_err(), "not a directory: /d/a.txt");
    }

    #[test]
    fn readdir_lists_immediate_children_only() {
        let mut f = fs();
        f.write("/d/a.txt", "A".into()).unwrap();
        f.mkdir("/d/sub", false).unwrap();
        f.write("/d/sub/deep.txt", "D".into()).unwrap();
        assert_eq!(f.readdir("/d").unwrap(), vec!["a.txt".to_string(), "sub".to_string()]);
        assert_eq!(f.readdir("/d/a.txt").unwrap_err(), "not a directory: /d/a.txt");
        assert_eq!(f.readdir("/missing").unwrap_err(), "no such file: /missing");
    }

    #[test]
    fn binary_contents_roundtrip_and_lossy_read() {
        let mut f = fs();
        f.write("/b.bin", vec![0xff, 0xfe, 0x00, b'A']).unwrap();
        assert_eq!(f.read_bytes("/b.bin").unwrap(), vec![0xff, 0xfe, 0x00, b'A']);
        let text = f.read_string("/b.bin").unwrap();
        assert_eq!(text.chars().count(), 4); // two U+FFFD chars + NUL + 'A'
        assert_eq!(text, "\u{fffd}\u{fffd}\0A");
        f.write("/empty", Vec::new()).unwrap();
        assert_eq!(f.read_string("/empty").unwrap(), "");
        assert_eq!(f.stat("/empty").unwrap().size, 0);
    }

    #[test]
    fn write_replaces_file_bytes_wholesale() {
        let mut f = fs();
        f.write("/f", vec![1, 2, 3, 4]).unwrap();
        f.write("/f", vec![9]).unwrap();
        assert_eq!(f.read_bytes("/f").unwrap(), vec![9]);
        assert_eq!(f.stat("/f").unwrap().size, 1);
    }

    #[test]
    fn quota_exact_fit_passes_one_more_byte_fails() {
        let mut f = RamFs::new(ENTRY_OVERHEAD + 10);
        f.write("/f", vec![0u8; 10]).unwrap(); // fills the quota exactly
        assert_eq!(f.write("/f", vec![0u8; 11]).unwrap_err(), "fs quota exceeded");
        assert_eq!(f.read_bytes("/f").unwrap().len(), 10); // unchanged after failure
        assert_eq!(f.write("/g", vec![0]).unwrap_err(), "fs quota exceeded");
        assert!(!f.exists("/g"));
        f.write("/f", vec![0u8; 9]).unwrap(); // shrinking always fits
        f.write("/f", vec![0u8; 10]).unwrap(); // and refills to the brim
    }

    #[test]
    fn quota_counts_directories_and_survives_structural_ops() {
        let mut f = RamFs::new(ENTRY_OVERHEAD * 2 + 1);
        f.mkdir("/d", true).unwrap(); // 64
        f.write("/d/a", vec![1]).unwrap(); // +65 = 129, the whole quota
        assert_eq!(f.write("/d/b", vec![1]).unwrap_err(), "fs quota exceeded");
        assert!(!f.exists("/d/b"));
        f.remove("/d/a", false).unwrap(); // free the file, keep the dir
        f.rename("/d", "/e").unwrap(); // re-keying must not double-count
        f.write("/e/b", vec![1]).unwrap();
        assert!(f.exists("/e/b") && !f.exists("/d"));
        let mut f2 = RamFs::new(0); // 0 disables the quota entirely
        f2.write("/x", vec![0u8; 8192]).unwrap();
        assert_eq!(f2.read_bytes("/x").unwrap().len(), 8192);
    }

    #[test]
    fn path_and_entry_caps_have_clear_errors() {
        let mut f = fs();
        let long = format!("/{}", "a".repeat(MAX_PATH_BYTES));
        assert!(f.write(&long, vec![]).unwrap_err().starts_with("path too long"));
        let exact = format!("/{}", "a".repeat(MAX_PATH_BYTES - 1));
        f.write(&exact, vec![]).unwrap(); // at the cap is fine
        let mut full = fs();
        for i in 0..MAX_ENTRIES {
            full.mkdir(&format!("/d{}", i), false).unwrap();
        }
        assert!(full.mkdir("/over", false).unwrap_err().contains("fs entry limit exceeded"));
    }

    #[test]
    fn used_accounting_matches_a_full_recount() {
        let mut f = fs();
        f.write("/a/b/c", vec![1, 2, 3]).unwrap();
        f.mkdir("/x/y", true).unwrap();
        f.write("/m", vec![0u8; 100]).unwrap();
        f.remove("/m", false).unwrap();
        f.rename("/a", "/z").unwrap();
        f.write("/z/b/c", vec![9]).unwrap();
        let recount = f.files.values().map(|d| file_cost(d.len())).sum::<u64>()
            + ENTRY_OVERHEAD * (f.dirs.len() as u64 - 1); // root is free
        assert_eq!(f.used, recount);
    }

    #[test]
    fn b64_roundtrips_everything_and_rejects_junk() {
        let all: Vec<u8> = (0..=255u8).collect();
        for len in [0, 1, 2, 3, 4, 5, 63, 64, 255, 256] {
            let data = &all[..len];
            assert_eq!(b64::decode(&b64::encode(data)).unwrap(), data, "len {}", len);
        }
        assert_eq!(b64::encode(b"f"), "Zg==");
        assert_eq!(b64::encode(b"fo"), "Zm8=");
        assert_eq!(b64::encode(b"foo"), "Zm9v");
        assert_eq!(b64::decode("Zm9v").unwrap(), b"foo");
        assert_eq!(b64::decode("===="), None); // more than two pads
        assert_eq!(b64::decode("A"), None); // not a multiple of four
        assert_eq!(b64::decode("Zm9*"), None); // outside the alphabet
        assert_eq!(b64::decode("Z=9v"), None); // interior padding
    }

    #[test]
    fn snapshot_v2_roundtrips_text_bytes_and_dirs() {
        let mut f = fs();
        f.write("/a.txt", b"text".to_vec()).unwrap();
        f.write("/bin", vec![0xff, 0x00, 0x81]).unwrap();
        f.mkdir("/empty/deep", true).unwrap();
        let json = f.snapshot_json();
        // serde_json's map is a BTreeMap, so the layout is deterministic.
        assert!(json.starts_with("{\"dirs\":[\"/\",\"/empty\",\"/empty/deep\"],\"files\":"));
        assert!(json.contains("\"/a.txt\":{\"text\":\"text\"}"));
        assert!(json.contains("\"/bin\":{\"b64\":\"/wCB\"}"));
        let mut g = fs();
        g.restore(&json).unwrap();
        assert_eq!(g.read_bytes("/a.txt").unwrap(), b"text");
        assert_eq!(g.read_bytes("/bin").unwrap(), vec![0xff, 0x00, 0x81]);
        assert!(g.exists("/empty") && g.exists("/empty/deep"));
        assert_eq!(g.readdir("/empty").unwrap(), vec!["deep".to_string()]);
        assert_eq!(g.readdir("/empty/deep").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn restore_accepts_the_legacy_flat_map() {
        let mut f = fs();
        f.restore(r#"{"/greeting.txt":"hello","/lib/util.js":"u=7"}"#).unwrap();
        assert_eq!(f.read_string("/greeting.txt").unwrap(), "hello");
        assert!(f.exists("/lib")); // implicit parents materialize as dirs
        assert_eq!(f.readdir("/lib").unwrap(), vec!["util.js".to_string()]);
        // and it merges into existing state like the old per-file writes
        f.write("/keep", b"k".to_vec()).unwrap();
        f.restore("{\"/new\":\"n\"}").unwrap();
        assert!(f.exists("/keep") && f.exists("/new"));
    }

    #[test]
    fn restore_is_validated_and_atomic() {
        let mut f = fs();
        f.write("/keep", b"k".to_vec()).unwrap();
        assert!(f.restore("{not json").unwrap_err().contains("invalid snapshot"));
        assert!(f.restore("[]").unwrap_err().contains("invalid snapshot"));
        assert!(f.restore("{}").is_ok()); // an empty legacy map is fine
        let v2 =
            serde_json::json!({"version": 2, "files": {"/x": "a string, not an object"}});
        assert!(f.restore(&v2.to_string()).unwrap_err().contains("invalid snapshot"));
        let bad_b64 = serde_json::json!({"version": 2, "files": {"/x": {"b64": "!!"}}});
        assert!(f.restore(&bad_b64.to_string()).unwrap_err().contains("bad base64"));
        assert!(f.restore(r#"{"/../escape": "x"}"#).unwrap_err().contains("path escapes root"));
        assert!(f.restore(r#"{"n": 1}"#).unwrap_err().contains("old-format values"));
        // a snapshot that does not fit changes nothing
        let mut q = RamFs::new(ENTRY_OVERHEAD + 5);
        let big = serde_json::json!({"version": 2, "files": {"/big": {"text": "0000000000"}}});
        assert_eq!(q.restore(&big.to_string()).unwrap_err(), "fs quota exceeded");
        assert!(!q.exists("/big") && q.list("/").unwrap().is_empty());
        // a file cannot land on a directory the snapshot itself declares
        let clash = serde_json::json!({"version": 2, "files": {"/d": {"text": "x"}}, "dirs": ["/d"]});
        assert_eq!(f.restore(&clash.to_string()).unwrap_err(), "is a directory: /d");
        assert!(f.exists("/keep")); // still untouched by every failure above
    }

    #[test]
    fn remove_handles_files_dirs_and_subtrees() {
        let mut f = fs();
        f.write("/d/a.txt", "A".into()).unwrap();
        f.remove("/d/a.txt", false).unwrap();
        assert_eq!(f.remove("/d/a.txt", false).unwrap_err(), "no such file: /d/a.txt");
        f.mkdir("/d/sub", false).unwrap();
        assert_eq!(f.remove("/d", false).unwrap_err(), "directory not empty: /d");
        f.remove("/d", true).unwrap();
        assert!(!f.exists("/d") && !f.exists("/d/sub"));
        assert_eq!(f.remove("/", true).unwrap_err(), "cannot remove root");
    }

    #[test]
    fn rename_moves_and_replaces() {
        let mut f = fs();
        f.write("/a.txt", "1".into()).unwrap();
        f.write("/b.txt", "2".into()).unwrap();
        f.rename("/a.txt", "/b.txt").unwrap();
        assert_eq!(f.read_string("/b.txt").unwrap(), "1");
        assert!(!f.exists("/a.txt"));

        f.mkdir("/d", false).unwrap();
        f.write("/d/x", "X".into()).unwrap();
        f.mkdir("/e", false).unwrap();
        f.rename("/d", "/e").unwrap();
        assert_eq!(f.read_string("/e/x").unwrap(), "X");
        assert!(f.exists("/e") && !f.exists("/d"));
    }

    #[test]
    fn rename_rejects_bad_targets() {
        let mut f = fs();
        f.write("/f", "1".into()).unwrap();
        f.mkdir("/d", false).unwrap();
        f.mkdir("/d/sub", false).unwrap();
        assert_eq!(f.rename("/missing", "/x").unwrap_err(), "no such file: /missing");
        assert_eq!(f.rename("/f", "/d").unwrap_err(), "is a directory: /d");
        assert_eq!(f.rename("/d", "/f").unwrap_err(), "not a directory: /f");
        assert_eq!(f.rename("/d", "/d/sub").unwrap_err(), "cannot rename a directory into itself: /d/sub");
        f.write("/deep/x", "1".into()).unwrap();
        assert_eq!(f.rename("/deep", "/nope/x").unwrap_err(), "no such file: /nope");
        // onto a non-empty directory
        f.mkdir("/full", false).unwrap();
        f.write("/full/kid", "k".into()).unwrap();
        assert_eq!(f.rename("/d", "/full").unwrap_err(), "directory not empty: /full");
        // rename onto itself is a no-op success
        f.rename("/f", "/f").unwrap();
        assert_eq!(f.read_string("/f").unwrap(), "1");
    }
}
