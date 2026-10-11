//! The RAM filesystem core: path normalization and the [`RamFs`] tree.
//! Guest-facing callbacks live in [crate::fs_api]; the snapshot codec in
//! [crate::snapshot]. Paths are normalized (`.`, `..`, duplicate and
//! trailing slashes collapse) and always absolute, and paths that would
//! escape the root are rejected. (Lone surrogates in a guest path are
//! caught at the conversion layer in [crate::fs_api], where the raw
//! UTF-16 is still visible.)

use std::collections::{BTreeMap, BTreeSet};

pub(crate) type FsResult<T> = Result<T, String>;

/// Error for impossible usage arithmetic (checked everywhere so a bug
/// cannot wrap the counter past the quota).
const ACCOUNTING: &str = "fs accounting overflow";

/// Flat bookkeeping cost per entry on top of its key and content bytes.
pub(crate) const ENTRY_OVERHEAD: u64 = 64;
/// Normalized paths at or under this UTF-8 length are accepted.
pub(crate) const MAX_PATH_BYTES: usize = 4096;
/// Upper bound on the number of files plus directories.
pub(crate) const MAX_ENTRIES: usize = 100_000;
/// The Config.fs_quota_bytes default: 64 MiB.
pub const DEFAULT_FS_QUOTA_BYTES: u64 = 64 << 20;

/// A file entry costs its content plus its key plus flat bookkeeping.
fn file_cost(path: &str, len: usize) -> u64 {
    ENTRY_OVERHEAD + path.len() as u64 + len as u64
}

/// A directory entry costs its key plus flat bookkeeping.
fn dir_cost(path: &str) -> u64 {
    ENTRY_OVERHEAD + path.len() as u64
}

/// Full recount of the quota usage of a whole tree (root excluded).
#[cfg(test)]
pub(crate) fn usage(files: &BTreeMap<String, Vec<u8>>, dirs: &BTreeSet<String>) -> u64 {
    files
        .iter()
        .map(|(p, d)| file_cost(p, d.len()))
        .sum::<u64>()
        + dirs
            .iter()
            .filter(|d| d.as_str() != "/")
            .map(|d| dir_cost(d))
            .sum::<u64>()
}

/// Paths echoed in guest-facing errors are capped so a huge argument
/// cannot balloon the message past V8's string limit.
pub(crate) fn shown(path: &str) -> String {
    const MAX: usize = 256;
    if path.len() <= MAX {
        return path.to_string();
    }
    let cut = path
        .char_indices()
        .find(|(i, _)| *i >= MAX)
        .map_or(path.len(), |(i, _)| i);
    format!("{}...", &path[..cut])
}

/// Collapses `.`/`..`/duplicate slashes and rejects anything that is not an
/// absolute path inside the sandbox root.
pub(crate) fn normalize(path: &str) -> FsResult<String> {
    if path.contains('\0') {
        return Err(format!("path contains NUL: {}", shown(path)));
    }
    if !path.starts_with('/') {
        return Err(format!("path must be absolute: {}", shown(path)));
    }
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                // Popping past the root means the path wants out.
                if parts.pop().is_none() {
                    return Err(format!("path escapes root: {}", shown(path)));
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
        return Err(format!(
            "path too long: {} bytes (max {})",
            norm.len(),
            MAX_PATH_BYTES
        ));
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

/// The `stat()` result: kind plus a size that means content bytes for files
/// and immediate child count for directories.
#[derive(Debug)]
pub(crate) struct Stat {
    pub(crate) is_dir: bool,
    pub(crate) size: u64,
}

/// A write that has passed every check, ready to be applied.
struct WritePlan {
    path: String,
    add: u64,
    old: u64,
    missing: Vec<String>,
    new_file: usize,
}

/// What a rename will do once committed. `used_delta` is the re-keying
/// delta only (shorter keys free bytes); destination removals are
/// refunded separately when the plan commits.
struct RenamePlan {
    from: String,
    to: String,
    from_is_dir: bool,
    dest_is_file: bool,
    dest_is_dir: bool,
    /// Net usage change: destination removals plus key-length growth of
    /// every re-keyed descendant (negative when bytes are freed).
    used_delta: i64,
}

/// What a restore will add once it has passed every check.
struct RestorePlan {
    new_dirs: Vec<String>,
    used_delta: i64,
}

/// The filesystem state: byte-valued files plus the set of directories
/// that exist in their own right (root included). Every file's ancestors
/// are directories; `write` and `restore` maintain that invariant. `used`
/// tracks key + content + bookkeeping bytes so growth is checked before
/// anything mutates.
pub(crate) struct RamFs {
    pub(crate) files: BTreeMap<String, Vec<u8>>,
    pub(crate) dirs: BTreeSet<String>,
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

    /// Checked usage bookkeeping: an overflow or underflow is a bug, not
    /// something a guest should be able to trigger, so it surfaces as an
    /// error instead of wrapping around the quota.
    fn charge(&mut self, delta: u64) -> FsResult<()> {
        self.used = self
            .used
            .checked_add(delta)
            .ok_or_else(|| ACCOUNTING.to_string())?;
        Ok(())
    }

    fn refund(&mut self, delta: u64) -> FsResult<()> {
        self.used = self
            .used
            .checked_sub(delta)
            .ok_or_else(|| ACCOUNTING.to_string())?;
        Ok(())
    }

    fn charge_signed(&mut self, delta: i64) -> FsResult<()> {
        self.used = self
            .used
            .checked_add_signed(delta)
            .ok_or_else(|| ACCOUNTING.to_string())?;
        Ok(())
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

    /// Everything `write` needs to decide, derived once so the caller can
    /// run the same check before copying any data in.
    fn plan_write(&self, path: &str, len: usize) -> FsResult<WritePlan> {
        let p = normalize(path)?;
        if self.is_dir(&p) {
            return Err(format!("is a directory: {}", p));
        }
        let missing = self.missing_dirs(&p)?;
        let old = self.files.get(&p).map_or(0, |d| file_cost(&p, d.len()));
        let add = file_cost(&p, len) + missing.iter().map(|d| dir_cost(d)).sum::<u64>();
        let new_file = usize::from(!self.files.contains_key(&p));
        Ok(WritePlan {
            path: p,
            add,
            old,
            missing,
            new_file,
        })
    }

    /// Post-operation usage for a write, checked end to end so an
    /// (unreachable) counter near u64::MAX errors instead of wrapping.
    fn used_after(&self, add: u64, old: u64) -> FsResult<u64> {
        self.used
            .checked_add(add)
            .and_then(|u| u.checked_sub(old))
            .ok_or_else(|| ACCOUNTING.to_string())
    }

    /// Quota, caps, and shape check for a would-be write of `len` bytes,
    /// without any data. Lets the callback reject a giant buffer before
    /// copying it into Rust memory.
    pub(crate) fn probe_write(&self, path: &str, len: usize) -> FsResult<()> {
        let plan = self.plan_write(path, len)?;
        let used_after = self.used_after(plan.add, plan.old)?;
        self.reserve(
            used_after,
            self.entries() + plan.missing.len() + plan.new_file,
        )
    }

    /// Writing creates missing parent directories (documented guest rule)
    /// and replaces an existing file at `path` wholesale.
    pub(crate) fn write(&mut self, path: &str, data: Vec<u8>) -> FsResult<()> {
        let plan = self.plan_write(path, data.len())?;
        let used_after = self.used_after(plan.add, plan.old)?;
        self.reserve(
            used_after,
            self.entries() + plan.missing.len() + plan.new_file,
        )?;
        let WritePlan {
            path,
            add,
            old,
            missing,
            ..
        } = plan;
        self.insert_dirs(missing);
        self.files.insert(path, data);
        self.charge_signed(add as i64 - old as i64)
    }

    /// `recursive` mirrors Node: missing parents are created, and an
    /// existing directory is a success rather than an error.
    pub(crate) fn mkdir(&mut self, path: &str, recursive: bool) -> FsResult<()> {
        let p = normalize(path)?;
        if self.is_dir(&p) {
            return if recursive {
                Ok(())
            } else {
                Err(format!("already exists: {}", p))
            };
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
        let add: u64 = missing.iter().map(|d| dir_cost(d)).sum::<u64>() + dir_cost(&p);
        let count = missing.len() + 1;
        let used_after = self.used_after(add, 0)?;
        self.reserve(used_after, self.entries() + count)?;
        self.insert_dirs(missing);
        self.dirs.insert(p);
        self.charge(add)
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
            return Ok(Stat {
                is_dir: false,
                size: data.len() as u64,
            });
        }
        if self.is_dir(&p) {
            return Ok(Stat {
                is_dir: true,
                size: self.child_names(&p).len() as u64,
            });
        }
        Err(format!("no such file: {}", p))
    }

    /// Recursive full-path listing of the files under `dir` (directories
    /// are not listed); the vm.fs.list() compatibility shape.
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
            return self.refund(file_cost(&p, data.len()));
        }
        if !self.is_dir(&p) {
            return Err(format!("no such file: {}", p));
        }
        let (files, dirs) = self.entries_under(&child_prefix(&p));
        if !recursive && !(files.is_empty() && dirs.is_empty()) {
            return Err(format!("directory not empty: {}", p));
        }
        let freed = files
            .iter()
            .map(|f| self.files.get(f).map_or(0, |d| file_cost(f, d.len())))
            .sum::<u64>()
            + dirs.iter().map(|d| dir_cost(d)).sum::<u64>()
            + dir_cost(&p);
        for f in files {
            self.files.remove(&f);
        }
        for d in dirs {
            self.dirs.remove(&d);
        }
        self.dirs.remove(&p);
        self.refund(freed)
    }

    /// POSIX-shaped rename: a file replaces a file, a directory moves onto
    /// an empty directory, and mismatched kinds or a move into the source's
    /// own subtree are errors. The destination's parent must exist, and
    /// renaming a path onto itself is the POSIX no-op success.
    pub(crate) fn rename(&mut self, from: &str, to: &str) -> FsResult<()> {
        let (f, t) = (normalize(from)?, normalize(to)?);
        if f == t {
            return Ok(());
        }
        let plan = self.validate_rename(&f, &t)?;
        self.commit_rename(plan)
    }

    /// Checks everything a rename needs and computes its key-growth delta
    /// without touching the tree. `f` and `t` arrive normalized, distinct.
    fn validate_rename(&self, f: &str, t: &str) -> FsResult<RenamePlan> {
        if f == "/" {
            return Err("cannot rename root".to_string());
        }
        let tparent = parent(t);
        if !self.is_dir(&tparent) {
            return Err(format!("no such file: {}", tparent));
        }
        let from_is_dir = self.is_dir(f);
        if !from_is_dir && !self.files.contains_key(f) {
            return Err(format!("no such file: {}", f));
        }
        if from_is_dir && t.starts_with(&child_prefix(f)) {
            return Err(format!("cannot rename a directory into itself: {}", t));
        }
        let (dest_is_file, dest_is_dir) = self.check_rename_dest(t, from_is_dir)?;
        let key_growth = if from_is_dir {
            self.subtree_key_growth(f, t)?
        } else {
            t.len() as i64 - f.len() as i64
        };
        let removals = self.dest_removals(t, dest_is_file, dest_is_dir);
        self.reserve_rename(key_growth, removals, dest_is_file)?;
        Ok(RenamePlan {
            from: f.to_string(),
            to: t.to_string(),
            from_is_dir,
            dest_is_file,
            dest_is_dir,
            used_delta: key_growth,
        })
    }

    /// Kind rules for the destination: a file may replace a file, a
    /// directory may move onto an empty directory, anything else errors.
    fn check_rename_dest(&self, t: &str, from_is_dir: bool) -> FsResult<(bool, bool)> {
        let dest_is_file = self.files.contains_key(t);
        let dest_is_dir = self.is_dir(t);
        if dest_is_file && from_is_dir {
            return Err(format!("not a directory: {}", t));
        }
        if dest_is_dir && !from_is_dir {
            return Err(format!("is a directory: {}", t));
        }
        if dest_is_dir && !self.child_names(t).is_empty() {
            return Err(format!("directory not empty: {}", t));
        }
        Ok((dest_is_file, dest_is_dir))
    }

    /// Bytes freed by dropping the destination (a replaced file or the
    /// empty directory a moved dir replaces).
    fn dest_removals(&self, t: &str, dest_is_file: bool, dest_is_dir: bool) -> u64 {
        let file = if dest_is_file {
            self.files.get(t).map_or(0, |d| file_cost(t, d.len()))
        } else {
            0
        };
        file + u64::from(dest_is_dir) * dir_cost(t)
    }

    /// A rename can only grow the usage (longer keys), never the entry
    /// count, so only net growth needs the quota gate.
    fn reserve_rename(&self, key_growth: i64, removals: u64, dest_is_file: bool) -> FsResult<()> {
        let net = key_growth - removals as i64;
        if net <= 0 {
            return Ok(());
        }
        let entries_after = self.entries() - usize::from(dest_is_file);
        let used_after = self
            .used
            .checked_add_signed(net)
            .ok_or_else(|| ACCOUNTING.to_string())?;
        self.reserve(used_after, entries_after)
    }

    fn commit_rename(&mut self, plan: RenamePlan) -> FsResult<()> {
        if plan.dest_is_file {
            if let Some(data) = self.files.remove(&plan.to) {
                self.refund(file_cost(&plan.to, data.len()))?;
            }
        }
        if plan.dest_is_dir {
            self.dirs.remove(&plan.to);
            self.refund(dir_cost(&plan.to))?;
        }
        if plan.from_is_dir {
            self.move_subtree(&plan.from, &plan.to);
        } else if let Some(data) = self.files.remove(&plan.from) {
            self.files.insert(plan.to, data);
        }
        self.charge_signed(plan.used_delta)
    }

    /// Sum of (new key length - old key length) over every entry a
    /// directory rename re-keys, the directory itself included, for
    /// growth of either sign - shrinking keys frees quota, and the usage
    /// must follow. Re-keyed paths past the length cap are refused before
    /// any mutation, but only when the keys actually grow.
    fn subtree_key_growth(&self, from: &str, to: &str) -> FsResult<i64> {
        let growth = to.len() as i64 - from.len() as i64;
        let prefix = child_prefix(from);
        let mut count = 1; // `from` itself is re-keyed to `to`
        let keys = self
            .files
            .keys()
            .map(String::as_str)
            .chain(self.dirs.iter().map(String::as_str))
            .filter(|k| k.starts_with(&prefix));
        for k in keys {
            if k.len() as i64 + growth > MAX_PATH_BYTES as i64 {
                return Err(format!("path too long after rename (max {})", MAX_PATH_BYTES));
            }
            count += 1;
        }
        Ok(growth.saturating_mul(count))
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

    /// Restores a snapshot, merging into the current state. All-or-nothing:
    /// a snapshot that would break the caps or the quota, or that clashes
    /// with existing entries, changes nothing. With a quota set, a payload
    /// larger than six quotas plus 1 MiB is rejected before parsing: JSON
    /// escapes control characters to six bytes each, so a snapshot of a
    /// full-quota filesystem always fits under the gate, while a hostile
    /// payload still cannot make the host allocate unboundedly.
    pub(crate) fn restore(&mut self, json: &str) -> FsResult<()> {
        if self.quota != 0 && json.len() as u64 > self.quota.saturating_mul(6).saturating_add(1 << 20)
        {
            return Err(format!(
                "snapshot too large for the quota ({} bytes)",
                json.len()
            ));
        }
        let snap = crate::snapshot::parse(json)?;
        self.apply_snapshot(snap)
    }

    /// Validates the merge as a delta against the current state, then
    /// commits. Every ancestor of a restored entry becomes a directory, so
    /// stat, exists and readdir see one uniform tree even for legacy
    /// snapshots.
    fn apply_snapshot(&mut self, snap: crate::snapshot::Snapshot) -> FsResult<()> {
        let plan = self.plan_restore(&snap)?;
        for d in plan.new_dirs {
            self.dirs.insert(d);
        }
        for (p, bytes) in snap.files {
            self.files.insert(p, bytes);
        }
        self.charge_signed(plan.used_delta)
    }

    /// The delta a restore would apply: new directories, replaced and
    /// added files, and the resulting usage and entry counts. A path that
    /// is currently a file cannot become a directory (consistent with
    /// write/mkdir), and a snapshot file cannot land on any directory.
    fn plan_restore(&self, snap: &crate::snapshot::Snapshot) -> FsResult<RestorePlan> {
        let mut want = snap.dirs.clone();
        for d in &snap.dirs {
            want.extend(ancestors(d));
        }
        for (p, _) in &snap.files {
            want.extend(ancestors(p));
        }
        let mut new_dirs = Vec::new();
        for d in &want {
            if self.files.contains_key(d) {
                return Err(format!("not a directory: {}", d));
            }
            if !self.dirs.contains(d) {
                new_dirs.push(d.clone());
            }
        }
        let mut delta = 0i64;
        let mut added = new_dirs.len();
        for d in &new_dirs {
            delta += dir_cost(d) as i64;
        }
        for (p, bytes) in &snap.files {
            if self.dirs.contains(p) || snap.dirs.contains(p) {
                return Err(format!("is a directory: {}", p));
            }
            // Not itself a directory, but some snapshot directory lives
            // under this file: the file is what stands in the way.
            if want.contains(p) {
                return Err(format!("not a directory: {}", p));
            }
            let old = self.files.get(p).map_or(0, |d| file_cost(p, d.len()));
            delta += file_cost(p, bytes.len()) as i64 - old as i64;
            added += usize::from(old == 0);
        }
        let entries_after = self.entries() + added;
        if entries_after > MAX_ENTRIES {
            return Err(format!("fs entry limit exceeded (max {})", MAX_ENTRIES));
        }
        // i128: quotas at or above 2^63 are representable (--fs-quota
        // accepts them), and casting them to i64 would wrap negative and
        // reject every restore.
        if self.quota != 0 && (self.used as i128 + delta as i128) > self.quota as i128 {
            return Err("fs quota exceeded".to_string());
        }
        Ok(RestorePlan {
            new_dirs,
            used_delta: delta,
        })
    }
}

fn rekey(path: &str, from: &str, to: &str) -> String {
    format!("{}{}", to, &path[from.len()..])
}

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
        assert_eq!(normalize("/a/\u{fffd}").unwrap(), "/a/\u{fffd}");
    }

    #[test]
    fn write_creates_parents_and_reads_back() {
        let mut f = fs();
        f.write("/a/b/c.txt", b"hi".to_vec()).unwrap();
        assert_eq!(f.read_string("/a/b/c.txt").unwrap(), "hi");
        assert!(f.exists("/a") && f.exists("/a/b"));
        assert_eq!(f.readdir("/a").unwrap(), vec!["b".to_string()]);
    }

    #[test]
    fn write_rejects_directory_and_file_conflicts() {
        let mut f = fs();
        f.mkdir("/d", false).unwrap();
        assert_eq!(f.write("/d", b"x".to_vec()).unwrap_err(), "is a directory: /d");
        f.write("/f", b"x".to_vec()).unwrap();
        assert_eq!(f.mkdir("/f", false).unwrap_err(), "already exists: /f");
        assert_eq!(f.mkdir("/f/x", false).unwrap_err(), "not a directory: /f");
        assert_eq!(
            f.write("/f/x", b"x".to_vec()).unwrap_err(),
            "not a directory: /f"
        );
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
        f.write("/f.txt", "héllo".as_bytes().to_vec()).unwrap(); // 6 UTF-8 bytes
        f.mkdir("/d", false).unwrap();
        f.write("/d/x", b"1".to_vec()).unwrap();
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
        f.write("/d/a.txt", b"A".to_vec()).unwrap();
        f.write("/d/e/b.txt", b"B".to_vec()).unwrap();
        f.mkdir("/d/empty", false).unwrap();
        assert_eq!(
            f.list("/d").unwrap(),
            vec!["/d/a.txt".to_string(), "/d/e/b.txt".to_string()]
        );
        assert_eq!(f.list("/d/empty").unwrap(), Vec::<String>::new());
        assert_eq!(
            f.list("/d/a.txt").unwrap_err(),
            "not a directory: /d/a.txt"
        );
    }

    #[test]
    fn listing_a_dir_ignores_longer_sibling_prefixes() {
        let mut f = fs();
        f.write("/a/x", b"1".to_vec()).unwrap();
        f.write("/ab/y", b"2".to_vec()).unwrap();
        f.mkdir("/ab/d", false).unwrap();
        assert_eq!(f.list("/a").unwrap(), vec!["/a/x".to_string()]);
        assert_eq!(f.readdir("/a").unwrap(), vec!["x".to_string()]);
        assert_eq!(
            f.readdir("/ab").unwrap(),
            vec!["d".to_string(), "y".to_string()]
        );
    }

    #[test]
    fn readdir_lists_immediate_children_only() {
        let mut f = fs();
        f.write("/d/a.txt", b"A".to_vec()).unwrap();
        f.mkdir("/d/sub", false).unwrap();
        f.write("/d/sub/deep.txt", b"D".to_vec()).unwrap();
        assert_eq!(
            f.readdir("/d").unwrap(),
            vec!["a.txt".to_string(), "sub".to_string()]
        );
        assert_eq!(
            f.readdir("/d/a.txt").unwrap_err(),
            "not a directory: /d/a.txt"
        );
        assert_eq!(f.readdir("/missing").unwrap_err(), "no such file: /missing");
    }

    #[test]
    fn remove_handles_files_dirs_and_subtrees() {
        let mut f = fs();
        f.write("/d/a.txt", b"A".to_vec()).unwrap();
        f.remove("/d/a.txt", false).unwrap();
        assert_eq!(
            f.remove("/d/a.txt", false).unwrap_err(),
            "no such file: /d/a.txt"
        );
        f.mkdir("/d/sub", false).unwrap();
        assert_eq!(
            f.remove("/d", false).unwrap_err(),
            "directory not empty: /d"
        );
        f.remove("/d", true).unwrap();
        assert!(!f.exists("/d") && !f.exists("/d/sub"));
        assert_eq!(f.remove("/", true).unwrap_err(), "cannot remove root");
    }

    #[test]
    fn rename_moves_and_replaces() {
        let mut f = fs();
        f.write("/a.txt", b"1".to_vec()).unwrap();
        f.write("/b.txt", b"2".to_vec()).unwrap();
        f.rename("/a.txt", "/b.txt").unwrap();
        assert_eq!(f.read_string("/b.txt").unwrap(), "1");
        assert!(!f.exists("/a.txt"));

        f.mkdir("/d", false).unwrap();
        f.write("/d/x", b"X".to_vec()).unwrap();
        f.mkdir("/e", false).unwrap();
        f.rename("/d", "/e").unwrap();
        assert_eq!(f.read_string("/e/x").unwrap(), "X");
        assert!(f.exists("/e") && !f.exists("/d"));
    }

    #[test]
    fn rename_rejects_bad_targets() {
        let mut f = fs();
        f.write("/f", b"1".to_vec()).unwrap();
        f.mkdir("/d", false).unwrap();
        f.mkdir("/d/sub", false).unwrap();
        assert_eq!(
            f.rename("/missing", "/x").unwrap_err(),
            "no such file: /missing"
        );
        assert_eq!(f.rename("/f", "/d").unwrap_err(), "is a directory: /d");
        assert_eq!(f.rename("/d", "/f").unwrap_err(), "not a directory: /f");
        assert_eq!(
            f.rename("/d", "/d/sub").unwrap_err(),
            "cannot rename a directory into itself: /d/sub"
        );
        f.write("/deep/x", b"1".to_vec()).unwrap();
        assert_eq!(f.rename("/deep", "/nope/x").unwrap_err(), "no such file: /nope");
        // onto a non-empty directory
        f.mkdir("/full", false).unwrap();
        f.write("/full/kid", b"k".to_vec()).unwrap();
        assert_eq!(
            f.rename("/d", "/full").unwrap_err(),
            "directory not empty: /full"
        );
        // rename onto itself is the POSIX no-op success
        f.rename("/f", "/f").unwrap();
        assert_eq!(f.read_string("/f").unwrap(), "1");
    }

    #[test]
    fn rename_refuses_re_keyed_paths_past_the_cap() {
        let mut f = fs();
        let deep = format!("/a/{}/x", "b".repeat(MAX_PATH_BYTES - 5));
        assert_eq!(deep.len(), MAX_PATH_BYTES);
        f.write(&deep, b"1".to_vec()).unwrap();
        // Renaming /a to /cccccc would push the child past the cap.
        let err = f.rename("/a", "/cccccc").unwrap_err();
        assert_eq!(err, format!("path too long after rename (max {})", MAX_PATH_BYTES));
        assert!(f.exists(&deep)); // nothing moved
        // A same-length destination re-keys within the cap.
        f.rename("/a", "/z").unwrap();
        let moved = format!("/z/{}/x", "b".repeat(MAX_PATH_BYTES - 5));
        assert!(f.exists(&moved));
    }

    #[test]
    fn binary_contents_roundtrip_and_lossy_read() {
        let mut f = fs();
        f.write("/b.bin", vec![0xff, 0xfe, 0x00, b'A']).unwrap();
        assert_eq!(
            f.read_bytes("/b.bin").unwrap(),
            vec![0xff, 0xfe, 0x00, b'A']
        );
        let text = f.read_string("/b.bin").unwrap();
        assert_eq!(text.chars().count(), 4); // two U+FFFD chars + NUL + 'A'
        assert_eq!(text, "\u{fffd}\u{fffd}\0A");
        f.write("/empty", Vec::new()).unwrap();
        assert_eq!(f.read_string("/empty").unwrap(), "");
        assert_eq!(f.stat("/empty").unwrap().size, 0);
        assert_eq!(f.read_bytes("/missing").unwrap_err(), "no such file: /missing");
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
    fn quota_counts_keys_directories_and_content() {
        // "/f" is 2 bytes of key: 64 overhead + 2 key + 10 content = 76.
        let mut f = RamFs::new(76);
        f.write("/f", vec![0u8; 10]).unwrap(); // fills the quota exactly
        assert_eq!(f.write("/f", vec![0u8; 11]).unwrap_err(), "fs quota exceeded");
        assert_eq!(f.read_bytes("/f").unwrap().len(), 10); // unchanged after failure
        assert_eq!(f.write("/g", vec![0]).unwrap_err(), "fs quota exceeded");
        assert!(!f.exists("/g"));
        f.write("/f", vec![0u8; 9]).unwrap(); // shrinking always fits
        f.write("/f", vec![0u8; 10]).unwrap(); // and refills to the brim
    }

    #[test]
    fn long_keys_hit_the_quota_early() {
        // ~2997-byte keys: each empty file costs 64 + key bytes.
        let base = format!("/k{}", "a".repeat(2995));
        let one = 64 + base.len() as u64;
        let mut f = RamFs::new(one); // room for exactly one
        assert!(f.write(&base, Vec::new()).is_ok());
        assert_eq!(
            f.write(&format!("{}2", base), Vec::new()).unwrap_err(),
            "fs quota exceeded"
        );
        // With a flat 64-byte rule a 100_000 quota would hold ~1500 of
        // these; charging keys must cut that to a few dozen.
        let mut roomy = RamFs::new(100_000);
        let mut wrote = 0u64;
        while roomy.write(&format!("{}{}", base, wrote), Vec::new()).is_ok() {
            wrote += 1;
        }
        assert!(
            (wrote * one..(wrote + 1) * one).contains(&100_000) || wrote * one <= 100_000,
            "wrote {}",
            wrote
        );
        assert!(wrote < 100, "keys must be charged: {} entries fit", wrote);
    }

    #[test]
    fn quota_survives_structural_ops() {
        // "/d" costs 66, "/d/a" with one byte costs 70: 136 total.
        let mut f = RamFs::new(136);
        f.mkdir("/d", true).unwrap();
        f.write("/d/a", vec![1]).unwrap();
        assert_eq!(f.write("/d/b", vec![1]).unwrap_err(), "fs quota exceeded");
        assert!(!f.exists("/d/b"));
        f.remove("/d/a", false).unwrap(); // free the file, keep the dir
        f.rename("/d", "/e").unwrap(); // re-keying must not double-count
        f.write("/e/b", vec![1]).unwrap();
        assert!(f.exists("/e/b") && !f.exists("/d"));
        let mut off = RamFs::new(0); // 0 disables the quota entirely
        off.write("/x", vec![0u8; 8192]).unwrap();
        assert_eq!(off.read_bytes("/x").unwrap().len(), 8192);
    }

    #[test]
    fn rename_growth_is_quota_checked() {
        let mut f = RamFs::new(file_cost("/a", 0)); // exactly one empty file
        f.write("/a", Vec::new()).unwrap();
        // Renaming to the longer /bbbbbb would grow past the quota.
        assert_eq!(f.rename("/a", "/bbbbbb").unwrap_err(), "fs quota exceeded");
        assert!(f.exists("/a"));
    }

    #[test]
    fn path_and_entry_caps_have_clear_errors() {
        let mut f = fs();
        let long = format!("/{}", "a".repeat(MAX_PATH_BYTES));
        assert!(
            f.write(&long, Vec::new())
                .unwrap_err()
                .starts_with("path too long")
        );
        let exact = format!("/{}", "a".repeat(MAX_PATH_BYTES - 1));
        f.write(&exact, Vec::new()).unwrap(); // at the cap is fine
        let mut full = fs();
        for i in 0..MAX_ENTRIES {
            full.mkdir(&format!("/d{}", i), false).unwrap();
        }
        assert!(
            full.mkdir("/over", false)
                .unwrap_err()
                .contains("fs entry limit exceeded")
        );
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
        assert_eq!(f.used, usage(&f.files, &f.dirs));
    }

    #[test]
    fn restore_rejects_a_directory_landing_on_a_file() {
        let mut f = fs();
        f.write("/a", b"x".to_vec()).unwrap();
        let snap = r#"{"version":2,"files":{},"dirs":["/a/b"]}"#.to_string();
        assert_eq!(f.restore(&snap).unwrap_err(), "not a directory: /a");
        assert_eq!(f.read_string("/a").unwrap(), "x"); // atomic: still there
    }

    #[test]
    fn restore_rejects_a_file_landing_on_a_directory() {
        let mut f = fs();
        f.mkdir("/d", false).unwrap();
        let snap = r#"{"version":2,"files":{"/d":{"text":"x"}},"dirs":[]}"#.to_string();
        assert_eq!(f.restore(&snap).unwrap_err(), "is a directory: /d");
        assert!(f.is_dir("/d") && !f.files.contains_key("/d"));
    }

    #[test]
    fn restore_rejects_oversized_payloads_before_parsing() {
        let mut f = RamFs::new(1024);
        let big = format!(
            "{{\"version\":2,\"files\":{{\"/x\":{{\"b64\":\"{}\"}}}}}}",
            "A".repeat(6 * 1024 + (1 << 20) + 8)
        );
        let err = f.restore(&big).unwrap_err();
        assert!(err.starts_with("snapshot too large for the quota"), "{}", err);
        assert!(f.list("/").unwrap().is_empty());
        // just under the wire parses, then fails on the content quota
        let edge = "A".repeat(6 * 1024 + (1 << 20) - 128);
        let snap = format!("{{\"version\":2,\"files\":{{\"/x\":{{\"b64\":\"{}\"}}}}}}", edge);
        assert_eq!(f.restore(&snap).unwrap_err(), "fs quota exceeded");
    }

    #[test]
    fn restore_rejects_snapshots_past_the_entry_cap_at_parse_time() {
        // One deliberately broken entry sorts first: if the count were
        // not taken up front, walking would fail with a path error
        // instead of the entry limit.
        let mut files = String::from("{\"version\":2,\"files\":{\"!!relative!!\":{\"text\":\"\"},");
        for i in 0..MAX_ENTRIES {
            files.push_str(&format!("\"/f{}\":{{\"text\":\"\"}},", i));
        }
        files.push_str("\"/last\":{\"text\":\"\"}},\"dirs\":[]}");
        let mut f = fs();
        let err = f.restore(&files).unwrap_err();
        assert!(err.contains("fs entry limit exceeded"), "{}", err);
        assert!(f.list("/").unwrap().is_empty());
    }

    #[test]
    fn restore_rejects_duplicate_normalized_keys() {
        let mut f = RamFs::new(1000);
        f.write("/a", vec![0u8; 900]).unwrap();
        // Two spellings of one path must not double-charge or shrink /a.
        let dup = r#"{"version":2,"files":{"/a":{"text":"y"},"//a":{"text":"z"}}}"#;
        assert_eq!(f.restore(dup).unwrap_err(), "duplicate path in snapshot: /a");
        assert_eq!(f.read_bytes("/a").unwrap().len(), 900); // unchanged
        assert_eq!(f.used, usage(&f.files, &f.dirs));
        // legacy duplicates collide after relative prefixing, too
        assert_eq!(
            f.restore(r#"{"b":"1","/b":"2"}"#).unwrap_err(),
            "duplicate path in snapshot: /b"
        );
        // a file and a dir spelling the same path are duplicates as well
        assert_eq!(
            f.restore(r#"{"version":2,"files":{"/c":{"text":"1"}},"dirs":["//c"]}"#)
                .unwrap_err(),
            "duplicate path in snapshot: /c"
        );
        assert_eq!(f.used, usage(&f.files, &f.dirs));
    }

    #[test]
    fn rename_cycles_do_not_drift_the_usage() {
        let mut f = fs();
        let long = format!("/{}", "a".repeat(2000));
        f.mkdir(&long, true).unwrap();
        for i in 0..20 {
            f.write(&format!("{}/f{}", long, i), Vec::new()).unwrap();
        }
        // Shrinking must free key bytes, growing must charge them back.
        for _ in 0..50 {
            f.rename(&long, "/b").unwrap();
            assert_eq!(f.used, usage(&f.files, &f.dirs));
            f.rename("/b", &long).unwrap();
            assert_eq!(f.used, usage(&f.files, &f.dirs));
        }
    }

    #[test]
    fn restore_works_with_quotas_at_or_above_2_to_the_63() {
        // --fs-quota accepts values whose byte counts do not fit i64;
        // casting the quota would wrap negative and reject everything.
        let mut f = RamFs::new(1 << 63);
        f.restore(r#"{"version":2,"files":{"/a":{"text":"x"}}}"#).unwrap();
        assert_eq!(f.read_string("/a").unwrap(), "x");
        f.write("/b", b"y".to_vec()).unwrap();
        assert_eq!(f.read_string("/b").unwrap(), "y");
    }

    #[test]
    fn usage_gates_error_instead_of_wrapping() {
        // A counter near u64::MAX is unreachable through the quota, but
        // the gates must still refuse to wrap it around to a small value.
        let mut f = fs();
        f.used = u64::MAX - 10;
        let err = f.write("/f", vec![0u8; 20]).unwrap_err();
        assert_eq!(err, "fs accounting overflow");
        assert_eq!(f.used, u64::MAX - 10); // untouched
        let err = f.mkdir("/d", false).unwrap_err();
        assert_eq!(err, "fs accounting overflow");
    }

    #[test]
    fn error_messages_truncate_huge_paths() {
        let err = normalize(&"a".repeat(100_000)).unwrap_err();
        assert!(err.starts_with("path must be absolute: a"));
        assert!(err.ends_with("..."));
        assert!(err.len() < 300, "{}", err.len());
        // multibyte chars are not split by the cut
        let err = normalize(&"é".repeat(100_000)).unwrap_err();
        assert!(err.ends_with("..."));
        assert!(std::str::from_utf8(err.as_bytes()).is_ok());
    }

    #[test]
    fn restore_reports_the_file_when_a_dir_would_sit_under_it() {
        let mut f = fs();
        // a declared directory under a snapshot file
        let snap = r#"{"version":2,"files":{"/a":{"text":"x"}},"dirs":["/a/b"]}"#;
        assert_eq!(f.restore(snap).unwrap_err(), "not a directory: /a");
        // a file and its own child
        let snap = r#"{"version":2,"files":{"/p":{"text":"x"},"/p/c":{"text":"y"}}}"#;
        assert_eq!(f.restore(snap).unwrap_err(), "not a directory: /p");
        // ... while a file landing on a directory still says so
        f.mkdir("/d", false).unwrap();
        let snap = r#"{"version":2,"files":{"/d":{"text":"x"}}}"#;
        assert_eq!(f.restore(snap).unwrap_err(), "is a directory: /d");
        assert!(f.list("/").unwrap().is_empty()); // nothing applied
    }

    #[test]
    fn restore_merges_and_keeps_unmentioned_entries() {
        let mut f = fs();
        f.write("/keep", b"k".to_vec()).unwrap();
        f.restore("{\"/new\":\"n\"}").unwrap();
        assert!(f.exists("/keep") && f.exists("/new"));
    }
}
