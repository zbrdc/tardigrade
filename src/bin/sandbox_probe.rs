// Probe helper for tests/sandbox.rs: proves the --sandbox layers bite.
//
// It is a plain binary because v8vm::sandbox::apply() must run before any
// thread exists, and the cargo test harness runs every test on a worker
// thread. Modes:
//   probe <dump> <escape_dir>  apply the sandbox and run the probes
//   late-thread                apply with a thread already running (refused)
//   late-v8                    apply after run() initialized V8 (refused)
// Output lines are "PROBE|<name>|<outcome>" for the tests to assert on.

use std::io::ErrorKind;
use std::net::TcpStream;
use std::path::{Path, PathBuf};

/// Classify one probe attempt the way the tests expect to read it.
fn outcome(result: std::io::Result<()>) -> String {
    match result {
        Ok(()) => "ok".to_string(),
        Err(e) if e.kind() == ErrorKind::PermissionDenied => "denied".to_string(),
        Err(e) => format!("err:{:?}", e.kind()),
    }
}

fn probe_all(dump: &Path, escape_dir: &Path) -> Vec<(String, String)> {
    let try_exec = std::process::Command::new("/bin/true").spawn().map(|_| ());
    vec![
        ("read-etc-passwd".into(), outcome(std::fs::File::open("/etc/passwd").map(|_| ()))),
        ("write-existing".into(), outcome(std::fs::write(escape_dir.join("escape.txt"), "x"))),
        ("write-new".into(), outcome(std::fs::write(escape_dir.join("new.txt"), "x"))),
        ("write-dump".into(), outcome(std::fs::write(dump, "{\"probe\":1}"))),
        ("read-dump".into(), outcome(std::fs::File::open(dump).map(|_| ()))),
        ("socket".into(), outcome(TcpStream::connect("127.0.0.1:1").map(|_| ()))),
        ("exec".into(), outcome(try_exec)),
    ]
}

fn thread_count() -> usize {
    std::fs::read_dir("/proc/self/task").map(|t| t.count()).unwrap_or(0)
}

fn mode_probe(dump: PathBuf, escape_dir: PathBuf) {
    println!("PROBE|threads|{}", thread_count());
    if let Err(e) = v8vm::sandbox::apply(Some(&dump)) {
        println!("PROBE|apply|refused:{}", e);
        return;
    }
    for (name, outcome) in probe_all(&dump, &escape_dir) {
        println!("PROBE|{}|{}", name, outcome);
    }
    // A thread created after apply must inherit both layers.
    let (d, e) = (dump.clone(), escape_dir.clone());
    let in_thread = std::thread::spawn(move || probe_all(&d, &e)).join().expect("probe thread");
    for (name, outcome) in in_thread {
        println!("PROBE|thr-{}|{}", name, outcome);
    }
}

fn mode_late_thread() {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let keeper = std::thread::spawn(move || {
        let _ = rx.recv();
    });
    match v8vm::sandbox::apply(None) {
        Err(e) => println!("PROBE|late-thread|refused:{}", e),
        Ok(()) => println!("PROBE|late-thread|ok"),
    }
    drop(tx);
    let _ = keeper.join();
}

fn mode_late_v8() {
    let _ = v8vm::run("", "noop.js", &v8vm::Config::new());
    match v8vm::sandbox::apply(None) {
        Err(e) => println!("PROBE|late-v8|refused:{}", e),
        Ok(()) => println!("PROBE|late-v8|ok"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [mode, dump, escape_dir] if mode == "probe" => {
            mode_probe(PathBuf::from(dump), PathBuf::from(escape_dir))
        }
        [mode] if mode == "late-thread" => mode_late_thread(),
        [mode] if mode == "late-v8" => mode_late_v8(),
        _ => {
            eprintln!("usage: sandbox_probe probe <dump> <escape_dir> | late-thread | late-v8");
            std::process::exit(2);
        }
    }
}
