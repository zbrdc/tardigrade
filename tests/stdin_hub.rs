//! stdin hub integration test (harness-less). The parent re-execs itself in
//! a child mode so it can drive the child's real stdin pipe and check what
//! each guest saw. Cargo may pass libtest-style arguments to harness-less
//! targets; only the explicit mode flags matter here and everything else is
//! ignored.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

const READ_ONE: &str =
    r#"vm.stdin.read().then((c) => { console.log('first:' + JSON.stringify(c)); vm.exit(0); });"#;
const READ_TWO: &str =
    r#"vm.stdin.read().then((c) => { console.log('second:' + JSON.stringify(c)); vm.exit(0); });"#;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--child") {
        child_two_runs();
        return;
    }
    if args.iter().any(|a| a == "--child-serve") {
        child_run_then_serve();
        return;
    }
    let ok = two_runs_keep_unread_input() && run_then_serve_shares_the_hub();
    if !ok {
        eprintln!("stdin_hub: FAILED");
        std::process::exit(1);
    }
    println!("stdin_hub: ok");
}

/// Two sequential runs: the first reads one chunk and exits; the second must
/// receive the chunk that arrives after the first run ended.
fn child_two_runs() {
    let cfg = v8vm::Config::new();
    let a = v8vm::run(READ_ONE, "one.js", &cfg);
    let b = v8vm::run(READ_TWO, "two.js", &cfg);
    std::process::exit(if a == 0 && b == 0 { 0 } else { 1 });
}

/// A run that reads stdin, then a serve session in the same process: serve
/// commands must flow through the same stdin hub instead of deadlocking on
/// the reader's stdin lock.
fn child_run_then_serve() {
    let cfg = v8vm::Config::new();
    let a = v8vm::run(READ_ONE, "one.js", &cfg);
    let b = v8vm::serve(std::io::stdout(), "globalThis.q = 1;", "serve.js", &cfg);
    std::process::exit(if a == 0 && b == 0 { 0 } else { 1 });
}

struct Session {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

fn session(mode: &str) -> Session {
    let mut child = Command::new(std::env::current_exe().expect("current exe"))
        .arg(mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn child");
    let stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    let (tx, rx) = mpsc::channel::<String>();
    // Read on a thread so a starved guest fails fast instead of hanging.
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            let _ = tx.send(std::mem::take(&mut line));
        }
    });
    Session { child, stdin, lines: rx }
}

fn feed(stdin: &mut ChildStdin, data: &[u8]) {
    stdin.write_all(data).expect("feed child");
    stdin.flush().expect("flush child");
}

fn next_line(lines: &Receiver<String>, what: &str) -> String {
    lines
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|_| panic!("no {} output within 30 s", what))
}

fn finish(mut s: Session) -> bool {
    drop(s.stdin); // EOF
    s.child.wait().expect("wait child").success()
}

fn two_runs_keep_unread_input() -> bool {
    let s = session("--child");
    let (mut stdin, lines) = (s.stdin, s.lines);
    feed(&mut stdin, b"one\n");
    let l1 = next_line(&lines, "first");
    // This chunk must survive the end of the first run: nothing subscribed
    // consumes it until the second run asks for it.
    feed(&mut stdin, b"two\n");
    let l2 = next_line(&lines, "second");
    let ok = l1 == "first:\"one\\n\"\n"
        && l2 == "second:\"two\\n\"\n"
        && finish(Session { child: s.child, stdin, lines });
    if !ok {
        eprintln!("two_runs: got {:?} {:?}", l1, l2);
    }
    ok
}

fn run_then_serve_shares_the_hub() -> bool {
    let s = session("--child-serve");
    let (mut stdin, lines) = (s.stdin, s.lines);
    feed(&mut stdin, b"one\n");
    let l1 = next_line(&lines, "first");
    feed(&mut stdin, b"40 + 2\n");
    let l2 = next_line(&lines, "serve reply");
    let ok = l1 == "first:\"one\\n\"\n"
        && l2 == "42\n"
        && finish(Session { child: s.child, stdin, lines });
    if !ok {
        eprintln!("run_then_serve: got {:?} {:?}", l1, l2);
    }
    ok
}
