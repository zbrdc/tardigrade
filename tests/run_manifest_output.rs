//! Focused tests for the private, create-new manifest output boundary.

#[path = "../src/manifest_output.rs"]
mod manifest_output;

use manifest_output::write_new;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        let path = root.join(format!(
            "manifest-output-{label}-{}-{}",
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

#[derive(Clone, Copy, Default)]
struct ChildSetup {
    umask: Option<libc::mode_t>,
    file_size_limit: Option<u64>,
}

fn wait_bounded(mut child: Child, seconds: u64) -> Output {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        match child.try_wait().expect("poll child test") {
            Some(_) => break,
            None if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            None => {
                child.kill().expect("kill timed-out child test");
                let _ = child.wait_with_output().expect("reap timed-out child test");
                panic!("child test exceeded its {seconds}s deadline");
            }
        }
    }
    child.wait_with_output().expect("collect child test output")
}

fn run_child_test(test_name: &str, environment: &[(&str, &Path)], setup: ChildSetup) -> Output {
    use std::os::unix::process::CommandExt;

    let mut command = Command::new(std::env::current_exe().expect("test executable path"));
    command
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in environment {
        command.env(key, value);
    }
    // SAFETY: this child-only hook uses only async-signal-safe libc calls.
    unsafe {
        command.pre_exec(move || {
            if let Some(mask) = setup.umask {
                libc::umask(mask);
            }
            if let Some(limit) = setup.file_size_limit {
                if libc::signal(libc::SIGXFSZ, libc::SIG_IGN) == libc::SIG_ERR {
                    return Err(std::io::Error::last_os_error());
                }
                let cap = libc::rlimit {
                    rlim_cur: limit as libc::rlim_t,
                    rlim_max: limit as libc::rlim_t,
                };
                if libc::setrlimit(libc::RLIMIT_FSIZE, &cap) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    wait_bounded(command.spawn().expect("spawn child test"), 15)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn child_private_mode_and_closed_descriptor() {
    let Some(path) = std::env::var_os("MANIFEST_OUTPUT_CHILD_PATH") else {
        return;
    };
    let path = PathBuf::from(path);
    write_new(&path, b"private manifest", None).expect("write child manifest");

    let metadata = fs::metadata(&path).expect("stat manifest");
    assert_eq!(
        metadata.permissions().mode() & 0o777,
        0o600,
        "manifest mode must be private even with a permissive umask"
    );
    let identity = (metadata.dev(), metadata.ino());
    for entry in fs::read_dir("/proc/self/fd").expect("list child descriptors") {
        let entry = entry.expect("read descriptor entry");
        if let Ok(open_file) = fs::metadata(entry.path()) {
            assert_ne!(
                (open_file.dev(), open_file.ino()),
                identity,
                "manifest descriptor {} remained open after write_new returned",
                entry.file_name().to_string_lossy()
            );
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn new_manifest_is_private_and_has_no_open_descriptor() {
    let scratch = Scratch::new("private");
    let path = scratch.0.join("prepared.json");
    let output = run_child_test(
        "child_private_mode_and_closed_descriptor",
        &[("MANIFEST_OUTPUT_CHILD_PATH", &path)],
        ChildSetup {
            umask: Some(0),
            ..ChildSetup::default()
        },
    );
    assert!(
        output.status.success(),
        "child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&path).unwrap(), b"private manifest");
}

#[test]
fn existing_regular_destination_is_preserved() {
    let scratch = Scratch::new("existing");
    let regular = scratch.0.join("existing.json");
    fs::write(&regular, b"do not change").unwrap();
    let error = write_new(&regular, b"replacement", None).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(&regular).unwrap(), b"do not change");
}

#[cfg(unix)]
#[test]
fn existing_symlink_destination_and_target_are_preserved() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new("existing-symlink");
    let target = scratch.0.join("target.json");
    let link = scratch.0.join("existing-link.json");
    fs::write(&target, b"symlink target stays").unwrap();
    symlink(&target, &link).unwrap();
    let error = write_new(&link, b"replacement", None).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read(&target).unwrap(), b"symlink target stays");
}

#[test]
fn existing_directory_destination_is_preserved() {
    let scratch = Scratch::new("existing-directory");
    let directory = scratch.0.join("existing-directory");
    fs::create_dir(&directory).unwrap();
    assert_eq!(
        write_new(&directory, b"replacement", None)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert!(directory.is_dir());
}

#[test]
fn exact_dump_alias_is_refused_and_owned_manifest_removed() {
    let scratch = Scratch::new("exact-alias");
    let exact = scratch.0.join("exact.json");
    let error = write_new(&exact, b"manifest", Some(&exact)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        !exact.exists(),
        "the newly created alias must be cleaned up"
    );
}

#[test]
fn equivalent_dump_spelling_is_refused_and_owned_manifest_removed() {
    let scratch = Scratch::new("equivalent-alias");
    let equivalent = scratch.0.join("equivalent.json");
    let equivalent_spelling = scratch.0.join(".").join("equivalent.json");
    let error = write_new(&equivalent, b"manifest", Some(&equivalent_spelling)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        !equivalent.exists(),
        "equivalent path spellings must be detected and cleaned"
    );
}

#[cfg(unix)]
#[test]
fn dangling_dump_symlink_alias_is_refused_and_preserved() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new("dangling-alias");
    let dangling_alias = scratch.0.join("dangling-dump.json");
    let manifest = scratch.0.join("manifest-through-dangling.json");
    symlink(&manifest, &dangling_alias).unwrap();
    let error = write_new(&manifest, b"manifest", Some(&dangling_alias)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        !manifest.exists(),
        "the newly created inode reached through the dangling alias must be cleaned"
    );
    assert!(
        fs::symlink_metadata(&dangling_alias)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the caller-owned dump symlink must remain"
    );
}

#[test]
fn distinct_dump_path_is_allowed() {
    let scratch = Scratch::new("distinct-dump");
    let dump = scratch.0.join("distinct-dump.json");
    let manifest = scratch.0.join("distinct-manifest.json");
    fs::write(&dump, b"dump").unwrap();
    write_new(&manifest, b"manifest", Some(&dump)).unwrap();
    assert_eq!(fs::read(&dump).unwrap(), b"dump");
    assert_eq!(fs::read(&manifest).unwrap(), b"manifest");
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn child_partial_write_failure_is_cleaned() {
    let Some(path) = std::env::var_os("MANIFEST_OUTPUT_CHILD_PATH") else {
        return;
    };
    let path = PathBuf::from(path);
    let probe = PathBuf::from(
        std::env::var_os("MANIFEST_OUTPUT_CHILD_PROBE").expect("partial-write probe path"),
    );
    let mut probe_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .expect("create partial-write probe");
    let error = probe_file
        .write_all(&vec![b'p'; 4096])
        .expect_err("RLIMIT_FSIZE must force a partial write");
    assert_eq!(error.raw_os_error(), Some(libc::EFBIG));
    assert_eq!(
        probe_file.metadata().unwrap().len(),
        128,
        "probe must verify that bytes landed before EFBIG"
    );
    drop(probe_file);
    fs::remove_file(&probe).unwrap();

    let error = write_new(&path, &vec![b'm'; 4096], None)
        .expect_err("the manifest write must hit RLIMIT_FSIZE");
    assert_eq!(error.raw_os_error(), Some(libc::EFBIG));
    assert!(
        matches!(
            fs::symlink_metadata(&path),
            Err(ref error) if error.kind() == std::io::ErrorKind::NotFound
        ),
        "failed output artifact was not removed"
    );
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn partial_write_failure_removes_only_its_new_file() {
    let scratch = Scratch::new("partial-write");
    let path = scratch.0.join("prepared.json");
    let probe = scratch.0.join("partial-write-probe");
    let output = run_child_test(
        "child_partial_write_failure_is_cleaned",
        &[
            ("MANIFEST_OUTPUT_CHILD_PATH", &path),
            ("MANIFEST_OUTPUT_CHILD_PROBE", &probe),
        ],
        ChildSetup {
            file_size_limit: Some(128),
            ..ChildSetup::default()
        },
    );
    assert!(
        output.status.success(),
        "child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!path.exists(), "failed manifest artifact must be absent");
    assert!(!probe.exists(), "partial-write probe must be removed");
}
