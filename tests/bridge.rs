//! P2 bridge contract tests (docs/BRIDGE_CONTRACT.md): pooled concurrency,
//! defined overload, worker replacement, and shared caps.

use std::io::{BufRead, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const APP: &str = r#"
globalThis.handler = async (req) => {
  if (req.path === "/slow") {
    await new Promise((r) => setTimeout(r, 700));
    return { status: 200, body: "slow:" + req.body };
  }
  if (req.path === "/die") { vm.exit(3); }
  return { status: 200, body: "ok" };
};
"#;

fn scratch(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

fn start_bridge(flags: &[&str]) -> (Child, u16) {
    let app = scratch("bridge_app.js");
    std::fs::write(&app, APP).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tardigrade-bridge"))
        .env("OUT", env!("CARGO_BIN_EXE_tardigrade"))
        .args(flags)
        .arg("--")
        .arg(app.to_str().unwrap())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn bridge");
    // The bridge prints its bound address once the pool is up.
    let mut line = String::new();
    let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
    out.read_line(&mut line).expect("bridge startup line");
    let port: u16 = line
        .rsplit(':')
        .next()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or_else(|| panic!("bad startup line: {line}"));
    (child, port)
}

/// Stops a bridge; its workers see stdin EOF and exit, taking their
/// sidecars with them (PDEATHSIG).
fn stop(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// One request; returns (status, body).
fn get(port: u16, path: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let req = format!(
        "POST {} HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\n\r\n{}",
        path,
        body.len(),
        body
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status in reply: {text}"));
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

#[test]
fn bridge_maps_replies_and_caps() {
    let (child, port) = start_bridge(&["--workers=1", "--addr=127.0.0.1:0"]);
    assert_eq!(get(port, "/fast", "hi"), (200, "ok".to_string()));
    // The cap triggers on the declared Content-Length, so the refusal can
    // be observed without actually shipping a megabyte into a closing
    // socket (the 413 close would RST a full-body write).
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    stream
        .write_all(b"POST /big HTTP/1.1\r\nHost: t\r\nContent-Length: 1048577\r\n\r\nx")
        .unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    assert!(text.starts_with("HTTP/1.1 413"), "reply: {text}");
    assert_eq!(get(port, "/fast", "x").0, 200); // the bridge survives the caps
    stop(child);
}

#[test]
fn two_workers_serve_slow_requests_in_parallel() {
    let (child, port) = start_bridge(&["--workers=2", "--addr=127.0.0.1:0"]);
    let start = Instant::now();
    let a = std::thread::spawn(move || get(port, "/slow", "a"));
    let (s1, r1) = get(port, "/slow", "b");
    let (s2, r2) = a.join().unwrap();
    let elapsed = start.elapsed();
    assert_eq!((s1, r1.as_str()), (200, "slow:b"));
    assert_eq!((s2, r2.as_str()), (200, "slow:a"));
    assert!(
        elapsed < Duration::from_millis(1300),
        "two workers must overlap the 700 ms handlers: {elapsed:?}"
    );
    stop(child);
}

#[test]
fn one_worker_serializes_requests() {
    let (child, port) = start_bridge(&["--workers=1", "--queue-depth=1", "--addr=127.0.0.1:0"]);
    let start = Instant::now();
    let a = std::thread::spawn(move || get(port, "/slow", "a"));
    let (s1, _) = get(port, "/slow", "b");
    let (s2, _) = a.join().unwrap();
    let elapsed = start.elapsed();
    assert_eq!((s1, s2), (200, 200));
    assert!(
        elapsed >= Duration::from_millis(1350),
        "one worker must serialize two 700 ms handlers: {elapsed:?}"
    );
    stop(child);
}

#[test]
fn overload_returns_a_defined_503() {
    let (child, port) = start_bridge(&["--workers=1", "--queue-depth=0", "--addr=127.0.0.1:0"]);
    let a = std::thread::spawn(move || get(port, "/slow", "a"));
    let (s1, _) = get(port, "/slow", "b");
    let (s2, _) = a.join().unwrap();
    let mut statuses = [s1, s2];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 503]);
    stop(child);
}

#[test]
fn a_dead_worker_answers_502_and_is_replaced() {
    let (child, port) = start_bridge(&["--workers=1", "--addr=127.0.0.1:0"]);
    assert_eq!(get(port, "/die", "x").0, 502);
    assert_eq!(get(port, "/fast", "x"), (200, "ok".to_string()));
    stop(child);
}

/// Sends raw bytes and returns the reply's status, if any.
fn raw(port: u16, bytes: &[u8]) -> Option<u16> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let _ = stream.write_all(bytes);
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    text.split_whitespace().nth(1).and_then(|s| s.parse().ok())
}

#[test]
fn hostile_requests_get_defined_answers_and_leave_the_bridge_healthy() {
    let (child, port) = start_bridge(&["--workers=1", "--addr=127.0.0.1:0"]);
    // Garbage request line.
    assert_eq!(raw(port, b"not http at all\r\n\r\n").unwrap(), 400);
    // A single header line past the 8 KiB cap.
    let mut flood = b"POST / HTTP/1.1\r\nX-Flood: ".to_vec();
    flood.extend(std::iter::repeat_n(b'a', 9 * 1024));
    flood.extend_from_slice(b"\r\n\r\n");
    assert_eq!(raw(port, &flood).unwrap(), 400);
    // More headers than the cap allows.
    let mut many = b"POST / HTTP/1.1\r\n".to_vec();
    for i in 0..200 {
        many.extend_from_slice(format!("X-N{i}: v\r\n").as_bytes());
    }
    many.extend_from_slice(b"\r\n");
    assert_eq!(raw(port, &many).unwrap(), 400);
    // A client that vanishes mid-request must not disturb anyone.
    {
        let mut ghost = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let _ = ghost.write_all(b"POST /slow HTTP/1.1\r\nContent-Length: 10\r\n\r\nsho");
        drop(ghost);
    }
    // The bridge answers normally after all of it.
    assert_eq!(get(port, "/fast", "x"), (200, "ok".to_string()));
    stop(child);
}

#[test]
fn an_overload_burst_returns_a_defined_mixture_and_recovers() {
    let (child, port) = start_bridge(&["--workers=1", "--queue-depth=0", "--addr=127.0.0.1:0"]);
    let mut handles = Vec::new();
    for _ in 0..6 {
        handles.push(std::thread::spawn(move || get(port, "/slow", "x").0));
    }
    let mut ok = 0;
    let mut refused = 0;
    for h in handles {
        match h.join().unwrap() {
            200 => ok += 1,
            503 => refused += 1,
            other => panic!("undefined answer {other} under overload"),
        }
    }
    assert!(ok >= 1, "at least the served request must succeed");
    assert!(refused >= 1, "overload must produce defined 503s");
    // Recovery: the pool still works after the burst.
    assert_eq!(get(port, "/fast", "x"), (200, "ok".to_string()));
    stop(child);
}
