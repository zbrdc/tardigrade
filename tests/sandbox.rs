//! Prove the `--sandbox` layers bite. The probes run in the single-threaded
//! `sandbox_probe` helper binary: `sandbox::apply` must precede every thread
//! and the cargo test harness runs each test on its own thread.

use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

fn run_probe(args: &[&str]) -> Vec<String> {
    let out = Command::new(env!("CARGO_BIN_EXE_sandbox_probe"))
        .args(args)
        .output()
        .expect("run sandbox_probe");
    assert!(
        out.status.success(),
        "probe helper failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8 probe output")
        .lines()
        .filter(|line| line.starts_with("PROBE|"))
        .map(String::from)
        .collect()
}

fn result<'a>(lines: &'a [String], name: &str) -> &'a str {
    lines
        .iter()
        .find_map(|line| line.strip_prefix(&format!("PROBE|{}|", name)))
        .unwrap_or_else(|| panic!("no result for {} in {:?}", name, lines))
}

/// Per-test scratch paths; the escape file pre-exists so the write-existing
/// probe exercises open+truncate, exactly what the dump write does.
fn probe_setup(name: &str) -> (PathBuf, PathBuf) {
    let dump = scratch(&format!("{}-dump.json", name));
    let escape = scratch(&format!("{}-escape", name));
    std::fs::create_dir_all(&escape).expect("escape dir");
    std::fs::write(escape.join("escape.txt"), "x").expect("escape file");
    (dump, escape)
}

fn probe_args<'a>(dump: &'a Path, escape: &'a Path) -> [&'a str; 3] {
    ["probe", dump.to_str().unwrap(), escape.to_str().unwrap()]
}

#[test]
fn sandbox_denies_filesystem_network_and_exec() {
    let (dump, escape) = probe_setup("deny");
    let lines = run_probe(&probe_args(&dump, &escape));
    assert_eq!(result(&lines, "threads"), "1", "apply starts from one thread");
    for name in [
        "read-etc-passwd",
        "write-existing",
        "write-new",
        "read-dump",
        "socket",
        "exec",
    ] {
        assert_eq!(result(&lines, name), "denied", "{} must be denied", name);
    }
}

#[test]
fn sandbox_allows_only_the_precreated_dump_file() {
    let (dump, escape) = probe_setup("dump");
    let lines = run_probe(&probe_args(&dump, &escape));
    assert_eq!(result(&lines, "write-dump"), "ok", "the dump write must pass");
    assert_eq!(result(&lines, "read-dump"), "denied", "even the dump is write-only");
    assert_eq!(result(&lines, "write-existing"), "denied", "sibling files refused");
    assert_eq!(result(&lines, "write-new"), "denied", "new files refused");
    let content = std::fs::read_to_string(&dump).expect("dump content");
    assert_eq!(content, "{\"probe\":1}");
}

#[test]
fn sandbox_restrictions_reach_threads_spawned_after_apply() {
    let (dump, escape) = probe_setup("thr");
    let lines = run_probe(&probe_args(&dump, &escape));
    for name in [
        "thr-read-etc-passwd",
        "thr-write-existing",
        "thr-write-new",
        "thr-read-dump",
        "thr-socket",
        "thr-exec",
    ] {
        assert_eq!(result(&lines, name), "denied", "{} must be denied", name);
    }
    assert_eq!(result(&lines, "thr-write-dump"), "ok", "the dump write passes there too");
}

#[test]
fn sandbox_refuses_when_other_threads_already_run() {
    let lines = run_probe(&["late-thread"]);
    let r = result(&lines, "late-thread");
    assert!(r.starts_with("refused:"), "apply must refuse: {:?}", lines);
    assert!(r.contains("thread"), "message must name threads: {:?}", lines);
}

#[test]
fn sandbox_refuses_when_v8_is_already_initialized() {
    let lines = run_probe(&["late-v8"]);
    let r = result(&lines, "late-v8");
    assert!(r.starts_with("refused:"), "apply must refuse: {:?}", lines);
    assert!(
        r.contains("run()/serve()"),
        "message must name the ordering: {:?}",
        lines
    );
}
