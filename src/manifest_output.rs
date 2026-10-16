use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

/// Remove `path` only while it still names the file created by this call.
#[cfg(unix)]
fn cleanup_owned(path: &Path, owner: FileIdentity) {
    let still_owned = fs::symlink_metadata(path)
        .map(|metadata| FileIdentity::from_metadata(&metadata) == owner)
        .unwrap_or(false);
    if still_owned {
        let _ = fs::remove_file(path);
    }
}

/// Create a new private manifest, publish its bytes, and close it before
/// returning. Existing paths are never opened or replaced.
#[cfg(unix)]
pub(crate) fn write_new(path: &Path, bytes: &[u8], dump: Option<&Path>) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = options.open(path)?;
    let owner = match file.metadata() {
        Ok(metadata) => FileIdentity::from_metadata(&metadata),
        Err(error) => {
            drop(file);
            return Err(error);
        }
    };

    let result = (|| {
        if let Some(dump) = dump {
            match fs::metadata(dump) {
                Ok(metadata) if FileIdentity::from_metadata(&metadata) == owner => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "manifest output aliases dump",
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }

        file.write_all(bytes)?;
        file.sync_all()
    })();

    drop(file);
    if let Err(error) = result {
        cleanup_owned(path, owner);
        return Err(error);
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn write_new(_path: &Path, _bytes: &[u8], _dump: Option<&Path>) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "manifest output requires Unix filesystem identity",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::{cleanup_owned, FileIdentity};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "manifest-output-inode-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).expect("create fresh NVMe fixture");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn inode_guard_preserves_a_replacement_file() {
        let scratch = Scratch::new();
        let path = scratch.0.join("manifest.json");
        let replacement = scratch.0.join("replacement");
        fs::write(&path, b"owned").expect("seed original artifact");
        let owner = FileIdentity::from_metadata(&fs::metadata(&path).unwrap());
        fs::write(&replacement, b"replacement").expect("seed replacement");
        let replacement_identity =
            FileIdentity::from_metadata(&fs::metadata(&replacement).unwrap());
        assert_ne!(owner, replacement_identity);
        fs::rename(&replacement, &path).expect("replace original artifact");

        cleanup_owned(&path, owner);

        assert_eq!(
            fs::read(&path).expect("replacement must survive"),
            b"replacement"
        );
    }
}
