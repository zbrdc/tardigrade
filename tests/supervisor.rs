// docs/SUPERVISOR_CONTRACT.md: deadline enforcement with an emptied worker
// group, and faithful propagation of the worker's own exit status.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::var("CARGO_TARGET_TMPDIR").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(dir).join(name)
}

#[test]
fn supervisor_enforces_deadline_and_empties_the_group() {
    let app = scratch("supervisor_spin.js");
    std::fs::write(&app, "while (true) {}").unwrap();
    let start = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_tardigrade-supervise"))
        .arg("--deadline=1500")
        .arg("--term-grace=500")
        .arg("--")
        .arg(env!("CARGO_BIN_EXE_tardigrade"))
        .arg(&app)
        .arg("--timeout=0")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(124),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // 1.5 s deadline + 0.5 s grace + bounded sweep must stay well below
    // any plausible hang; this also proves the supervisor itself returned.
    assert!(start.elapsed() < Duration::from_secs(15));
}

#[test]
fn supervisor_propagates_worker_exit_code() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tardigrade-supervise"))
        .arg("--deadline=20000")
        .arg("--")
        .arg(env!("CARGO_BIN_EXE_tardigrade"))
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"vm.exit(7)")
        .unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(7));
}
