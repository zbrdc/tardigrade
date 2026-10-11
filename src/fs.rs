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
    Ok(if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    })
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
/// are directories; `write` and `restore` maintain that invariant.
pub(crate) struct RamFs {
    files: BTreeMap<String, Vec<u8>>,
    dirs: BTreeSet<String>,
}

impl RamFs {
    pub(crate) fn new() -> Self {
        RamFs {
            files: BTreeMap::new(),
            dirs: BTreeSet::from(["/".to_string()]),
        }
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
        self.insert_dirs(missing);
        self.files.insert(p, data);
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
        self.insert_dirs(missing);
        self.dirs.insert(p);
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
        if self.files.remove(&p).is_some() {
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
            self.files.remove(&f);
        }
        for d in dirs {
            self.dirs.remove(&d);
        }
        self.dirs.remove(&p);
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
            self.files.remove(&t);
        }
        if to_is_dir {
            self.dirs.remove(&t);
        }
        if from_is_dir {
            self.move_subtree(&f, &t);
        } else if let Some(data) = self.files.remove(&f) {
            self.files.insert(t, data);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fs() -> RamFs {
        RamFs::new()
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
