//! Writing the `--dump` file. The agent-loop pattern uses one file as both
//! --preload and --dump, so a write that dies halfway must not destroy the
//! workspace the run started from.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Temp-name collisions tolerated before giving up. A collision needs a
/// squatter or a clock repeat, so a few tries are plenty.
const TEMP_ATTEMPTS: usize = 4;

/// Longest slice of the target's name copied into the temp name. The rest
/// of the temp name adds under 60 bytes, which must still fit NAME_MAX
/// (255) for a target whose own name is nearly that long.
const TEMP_NAME_PART_MAX: usize = 180;

/// Distinguishes temp names made by one process within one clock tick.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes `json` to `dump`. A regular file, or a path that does not exist
/// yet, is replaced atomically: the old file stays whole until a complete,
/// fsynced replacement is renamed over it, so a failure at any point (disk
/// full, I/O error, a kill mid-write) leaves the previous dump intact.
///
/// Anything else is written in place with open(O_CREAT|O_TRUNC), exactly as
/// before the atomic path existed: a symlink (even a dangling one), a
/// device, a FIFO, a directory (which fails). Renaming over those would
/// replace the link or the device node, and resolving a symlink ourselves
/// would skip the kernel's fs.protected_symlinks check. A sandboxed run is
/// also written in place: Landlock lets it write only the dump file's own
/// inode, so it cannot create the temp file.
pub(crate) fn write(dump: &Path, json: &str) -> io::Result<()> {
    if crate::sandbox::landlocked() {
        return fs::write(dump, json);
    }
    match fs::symlink_metadata(dump) {
        Ok(meta) if meta.is_file() => replace(dump, json, Some(meta.permissions())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => replace(dump, json, None),
        // Not a regular file, or not inspectable: the open reports the
        // kernel's own verdict.
        _ => fs::write(dump, json),
    }
}

/// Writes the replacement next to `dump` and renames it into place.
fn replace(dump: &Path, json: &str, mode: Option<fs::Permissions>) -> io::Result<()> {
    // create_temp only returns a file this call created, so the cleanup
    // below can never delete someone else's.
    let (tmp, file) = create_temp(dump)?;
    let result = fill(file, json, mode).and_then(|()| fs::rename(&tmp, dump));
    match &result {
        Ok(()) => sync_parent(dump),
        // Best effort: the error being returned is the one that matters.
        Err(_) => drop(fs::remove_file(&tmp)),
    }
    result
}

/// Gives the file the old dump's mode so the replacement does not silently
/// change permissions, and syncs the data to disk before the rename.
fn fill(mut file: File, json: &str, mode: Option<fs::Permissions>) -> io::Result<()> {
    if let Some(mode) = mode {
        file.set_permissions(mode)?;
    }
    file.write_all(json.as_bytes())?;
    file.sync_all()
}

/// Makes the rename itself durable. Some filesystems refuse to fsync a
/// directory, and the data is already safe on disk, so errors are ignored.
fn sync_parent(dump: &Path) {
    let parent = match dump.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let _ = File::open(parent).and_then(|dir| dir.sync_all());
}

/// A fresh suffix for a temp name: the clock keeps separate processes
/// apart, the counter keeps this process's calls apart.
fn next_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{}-{}", nanos, TEMP_COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn create_temp(dump: &Path) -> io::Result<(PathBuf, File)> {
    create_temp_from(dump, (0..TEMP_ATTEMPTS).map(|_| next_suffix()))
}

/// Creates the first temp file whose name is free, trying one suffix per
/// attempt. create_new refuses to reuse a name, and a name that is taken
/// belongs to someone else: it is skipped, never opened or deleted. The
/// pid keeps concurrent runs that dump into one directory apart as well.
fn create_temp_from(
    dump: &Path,
    suffixes: impl Iterator<Item = String>,
) -> io::Result<(PathBuf, File)> {
    for suffix in suffixes {
        let path = temp_sibling(dump, &suffix);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free temporary file name for the dump",
    ))
}

/// A hidden name next to the target, so the rename stays on one filesystem.
fn temp_sibling(dump: &Path, suffix: &str) -> PathBuf {
    let name = dump.file_name().unwrap_or_default().to_string_lossy();
    let name = truncate_at_char(&name, TEMP_NAME_PART_MAX);
    dump.with_file_name(format!(
        ".{}.tardigrade-{}-{}.tmp",
        name,
        std::process::id(),
        suffix
    ))
}

/// The longest prefix of `s` that is at most `max` bytes and ends on a
/// character boundary, so cutting a multi-byte name cannot panic.
fn truncate_at_char(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory for one test, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "tardigrade-dump-{}-{}",
                name,
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create scratch dir");
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_file_already_at_the_temp_name_is_skipped_and_never_deleted() {
        let scratch = Scratch::new("squatter");
        let dump = scratch.0.join("ws.json");
        let squatter = temp_sibling(&dump, "taken");
        fs::write(&squatter, "SOMEONE ELSE").unwrap();

        // Every name is taken: the call fails and leaves the file alone.
        let err = create_temp_from(&dump, ["taken".to_string()].into_iter()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&squatter).unwrap(), "SOMEONE ELSE");

        // A free name after the taken one is used, and the squatter stays.
        let suffixes = ["taken".to_string(), "free".to_string()];
        let (path, _file) = create_temp_from(&dump, suffixes.into_iter()).unwrap();
        assert_eq!(path, temp_sibling(&dump, "free"));
        assert_eq!(fs::read_to_string(&squatter).unwrap(), "SOMEONE ELSE");
    }

    #[test]
    fn successive_suffixes_differ() {
        assert_ne!(next_suffix(), next_suffix());
    }

    #[test]
    fn a_long_multibyte_name_is_cut_on_a_character_boundary() {
        // 3-byte characters: a limit of 200 falls inside the 67th.
        let name = "\u{20ac}".repeat(100);
        assert_eq!(truncate_at_char(&name, 200).len(), 198);
        assert_eq!(truncate_at_char("short", 200), "short");
        let tmp = temp_sibling(Path::new("/d").join(&name).as_path(), &next_suffix());
        assert!(tmp.file_name().unwrap().len() <= 255);
    }
}
