//! Writing the `--dump` file. The agent-loop pattern uses one file as both
//! --preload and --dump, so a write that dies halfway must not destroy the
//! workspace the run started from.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Writes `json` to `dump`. Normally the old file stays whole until a
/// complete, fsynced replacement is renamed over it, so a failure at any
/// point (disk full, I/O error, a kill mid-write) leaves the previous dump
/// intact. `in_place` truncates and rewrites the file instead, for a
/// sandboxed run: Landlock lets it write only the dump file's own inode, so
/// it cannot create the temp file.
pub(crate) fn write(dump: &Path, json: &str, in_place: bool) -> io::Result<()> {
    let existing = fs::metadata(dump).ok();
    // A device or pipe (--dump=/dev/null) cannot be replaced by rename, and
    // a directory should fail as it always did.
    let regular_or_new = existing.as_ref().is_none_or(|m| m.is_file());
    if in_place || !regular_or_new {
        return fs::write(dump, json);
    }
    // Resolve a symlink first so the rename replaces the file it points to
    // rather than the link.
    let target = fs::canonicalize(dump).unwrap_or_else(|_| dump.to_path_buf());
    let tmp = temp_sibling(&target);
    let result = write_synced(&tmp, json, existing.map(|m| m.permissions()))
        .and_then(|()| fs::rename(&tmp, &target));
    if result.is_err() {
        // Best effort: the error being returned is the one that matters.
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// A hidden name next to the target, so the rename stays on one filesystem.
/// The pid keeps concurrent runs that dump into one directory apart.
fn temp_sibling(target: &Path) -> PathBuf {
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    target.with_file_name(format!(".{}.v8vm-{}.tmp", name, std::process::id()))
}

/// Creates `path` (never reusing a leftover), gives it the old dump's mode
/// so the replacement does not silently change permissions, and syncs the
/// data to disk before the caller renames it into place.
fn write_synced(path: &Path, json: &str, mode: Option<fs::Permissions>) -> io::Result<()> {
    let mut file: File = OpenOptions::new().write(true).create_new(true).open(path)?;
    if let Some(mode) = mode {
        file.set_permissions(mode)?;
    }
    file.write_all(json.as_bytes())?;
    file.sync_all()
}
