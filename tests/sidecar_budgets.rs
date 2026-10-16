//! Exercise cumulative payload accounting through the real sidecar protocol.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const CHILD_DEADLINE: Duration = Duration::from_secs(8);
const IO_DEADLINE: Duration = Duration::from_secs(3);
const SERVER_DEADLINE: Duration = Duration::from_secs(12);

#[derive(Clone)]
enum Reply {
    Body {
        status: u16,
        reason: &'static str,
        body: Vec<u8>,
    },
    Drop,
    Partial {
        declared_len: usize,
        body: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

struct LoopbackServer {
    url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    stop: Arc<AtomicBool>,
    worker_done: mpsc::Receiver<()>,
    worker: Option<JoinHandle<()>>,
}

impl LoopbackServer {
    fn start(path: &str, replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback HTTP server");
        let url = format!("http://{}{}", listener.local_addr().unwrap(), path);
        listener
            .set_nonblocking(true)
            .expect("make loopback listener nonblocking");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let records = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (done_sender, worker_done) = mpsc::channel();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + SERVER_DEADLINE;
            let mut index = 0usize;
            while !worker_stop.load(Ordering::SeqCst) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_read_timeout(Some(IO_DEADLINE));
                        let _ = stream.set_write_timeout(Some(IO_DEADLINE));
                        let mut reader = BufReader::new(stream);
                        let recorded = read_http_request(&mut reader)
                            .expect("read complete request from controlled sidecar");
                        records.lock().unwrap().push(recorded);
                        match replies.get(index).cloned().unwrap_or_else(empty_reply) {
                            Reply::Body {
                                status,
                                reason,
                                body,
                            } => write_http_response(reader.get_mut(), status, reason, &body)
                                .expect("write controlled HTTP response"),
                            Reply::Drop => {
                                let _ = reader.get_mut().shutdown(Shutdown::Both);
                            }
                            Reply::Partial { declared_len, body } => {
                                write!(
                                    reader.get_mut(),
                                    "HTTP/1.1 200 OK\r\nContent-Length: {declared_len}\r\nConnection: close\r\n\r\n"
                                )
                                .and_then(|_| reader.get_mut().write_all(&body))
                                .and_then(|_| reader.get_mut().flush())
                                .expect("write partial HTTP response");
                                let _ = reader.get_mut().shutdown(Shutdown::Both);
                            }
                        }
                        index += 1;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("accept controlled HTTP request: {error}"),
                }
            }
            let _ = done_sender.send(());
        });
        Self {
            url,
            requests,
            stop,
            worker_done,
            worker: Some(worker),
        }
    }

    fn finish(mut self) -> Vec<RecordedRequest> {
        self.stop.store(true, Ordering::SeqCst);
        self.worker_done
            .recv_timeout(SERVER_DEADLINE)
            .expect("controlled HTTP server did not stop before its deadline");
        if let Some(worker) = self.worker.take() {
            worker.join().expect("controlled HTTP server worker");
        }
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.worker_done.recv_timeout(SERVER_DEADLINE);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn empty_reply() -> Reply {
    Reply::Body {
        status: 200,
        reason: "OK",
        body: Vec::new(),
    }
}

fn read_http_request(reader: &mut BufReader<TcpStream>) -> io::Result<RecordedRequest> {
    const MAX_REQUEST_BYTES: usize = 16 * 1024;
    let mut header_bytes = 0usize;
    let mut first_line = String::new();
    if reader.read_line(&mut first_line)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "missing request",
        ));
    }
    header_bytes += first_line.len();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        let count = reader.read_line(&mut line)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated request headers",
            ));
        }
        header_bytes = header_bytes.saturating_add(count);
        if header_bytes > MAX_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request headers exceeded test bound",
            ));
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value
                    .trim()
                    .parse()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            }
        }
    }
    if content_length > MAX_REQUEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request body exceeded test bound",
        ));
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body)?;
    let mut request_line = first_line.split_whitespace();
    Ok(RecordedRequest {
        method: request_line.next().unwrap_or_default().to_owned(),
        path: request_line.next().unwrap_or_default().to_owned(),
        body,
    })
}

fn write_http_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &[u8],
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

struct ChildOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn drain_pipe<R: Read + Send + 'static>(
    mut pipe: R,
) -> (mpsc::Receiver<io::Result<Vec<u8>>>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = pipe.read_to_end(&mut bytes).map(|_| bytes);
        let _ = sender.send(result);
    });
    (receiver, worker)
}

fn run_sidecar(input: Vec<u8>) -> ChildOutput {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tardigrade"))
        .arg("net-sidecar")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn real sidecar");
    let stdin = child.stdin.take().expect("piped sidecar stdin");
    let (stdout_rx, stdout_worker) = drain_pipe(child.stdout.take().expect("piped sidecar stdout"));
    let (stderr_rx, stderr_worker) = drain_pipe(child.stderr.take().expect("piped sidecar stderr"));
    let (feeder_sender, feeder_receiver) = mpsc::channel();
    let feeder = thread::spawn(move || {
        let mut stdin = stdin;
        let _ = feeder_sender.send(stdin.write_all(&input));
    });

    let deadline = Instant::now() + CHILD_DEADLINE;
    let (status, timed_out) = loop {
        match child.try_wait().expect("poll sidecar child") {
            Some(status) => break (status, false),
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            None => {
                let _ = child.kill();
                let status = child.wait().expect("reap timed-out sidecar");
                break (status, true);
            }
        }
    };
    let feeder_result = feeder_receiver
        .recv_timeout(IO_DEADLINE)
        .expect("sidecar input feeder did not finish before its deadline");
    feeder.join().expect("join sidecar input feeder");
    let stdout = stdout_rx
        .recv_timeout(IO_DEADLINE)
        .expect("drain sidecar stdout before deadline")
        .expect("read sidecar stdout");
    let stderr = stderr_rx
        .recv_timeout(IO_DEADLINE)
        .expect("drain sidecar stderr before deadline")
        .expect("read sidecar stderr");
    stdout_worker.join().expect("join sidecar stdout drain");
    stderr_worker.join().expect("join sidecar stderr drain");
    assert!(!timed_out, "sidecar exceeded {CHILD_DEADLINE:?}");
    assert!(
        feeder_result.is_ok(),
        "sidecar closed stdin before consuming the test frames: {feeder_result:?}"
    );
    ChildOutput {
        status,
        stdout,
        stderr,
    }
}

fn policy(url: &str, method: &str, max_response_bytes: u64, total_bytes: u64) -> serde_json::Value {
    serde_json::json!({
        "policy": {
            "calls": {
                "local": {
                    "url": url,
                    "method": method
                }
            },
            "max_response_bytes": max_response_bytes,
            "total_bytes": total_bytes,
            "timeout_ms": 2000,
            "allow_private": true
        }
    })
}

fn append_frame(input: &mut Vec<u8>, value: &serde_json::Value) {
    input.extend_from_slice(&serde_json::to_vec(value).unwrap());
    input.push(b'\n');
}

fn request(id: u64, capability: &str, body: &str) -> serde_json::Value {
    serde_json::json!({"id": id, "cap": capability, "body": body})
}

fn run_requests(policy: serde_json::Value, requests: &[serde_json::Value]) -> ChildOutput {
    let mut input = Vec::new();
    append_frame(&mut input, &policy);
    for req in requests {
        append_frame(&mut input, req);
    }
    run_sidecar(input)
}

fn replies(output: &ChildOutput) -> Vec<serde_json::Value> {
    assert_eq!(
        output.status.code(),
        Some(0),
        "sidecar status {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("decode sidecar reply"))
        .collect()
}

fn assert_requests(actual: &[RecordedRequest], expected: &[(&str, &str, &[u8])]) {
    let expected = expected
        .iter()
        .map(|(method, path, body)| RecordedRequest {
            method: (*method).to_owned(),
            path: (*path).to_owned(),
            body: body.to_vec(),
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, expected, "controlled HTTP egress differed");
}

#[test]
fn cumulative_utf8_budget_refuses_third_body_without_egress() {
    let server = LoopbackServer::start(
        "/cumulative",
        vec![
            Reply::Body {
                status: 200,
                reason: "OK",
                body: b"a".to_vec(),
            },
            Reply::Body {
                status: 200,
                reason: "OK",
                body: "☃".as_bytes().to_vec(),
            },
        ],
    );
    let output = run_requests(
        policy(&server.url, "POST", 8, 7),
        &[
            request(1, "local", "é"),
            request(2, "local", "x"),
            request(3, "local", "z"),
        ],
    );
    assert_eq!(
        replies(&output),
        vec![
            serde_json::json!({"id": 1, "status": 200, "text": "a"}),
            serde_json::json!({"id": 2, "status": 200, "text": "☃"}),
            serde_json::json!({"id": 3, "error": "policy: total byte budget exceeded"}),
        ]
    );
    assert_requests(
        &server.finish(),
        &[
            ("POST", "/cumulative", "é".as_bytes()),
            ("POST", "/cumulative", b"x"),
        ],
    );
}

#[test]
fn oversized_refusal_preserves_credit_and_unknown_capability_is_checked_first() {
    let server = LoopbackServer::start("/preserve", vec![empty_reply()]);
    let output = run_requests(
        policy(&server.url, "POST", 4, 3),
        &[
            request(1, "missing", "oversized"),
            request(2, "local", "four"),
            request(3, "local", "é"),
        ],
    );
    assert_eq!(
        replies(&output),
        vec![
            serde_json::json!({"id": 1, "error": "policy: unknown capability"}),
            serde_json::json!({"id": 2, "error": "policy: total byte budget exceeded"}),
            serde_json::json!({"id": 3, "status": 200, "text": ""}),
        ]
    );
    assert_requests(&server.finish(), &[("POST", "/preserve", "é".as_bytes())]);
}

#[test]
fn per_response_bound_overflow_exhausts_total_allowance() {
    let server = LoopbackServer::start(
        "/response-cap",
        vec![Reply::Body {
            status: 200,
            reason: "OK",
            body: b"four".to_vec(),
        }],
    );
    let output = run_requests(
        policy(&server.url, "POST", 3, 10),
        &[request(1, "local", ""), request(2, "local", "x")],
    );
    assert_eq!(
        replies(&output),
        vec![
            serde_json::json!({
                "id": 1,
                "error": "overflow: response exceeds the response cap"
            }),
            serde_json::json!({"id": 2, "error": "policy: total byte budget exceeded"}),
        ]
    );
    assert_requests(&server.finish(), &[("POST", "/response-cap", b"")]);
}

#[test]
fn remaining_total_bound_is_independent_of_response_cap_and_exhausts() {
    let server = LoopbackServer::start(
        "/remaining",
        vec![Reply::Body {
            status: 200,
            reason: "OK",
            body: b"12345".to_vec(),
        }],
    );
    let output = run_requests(
        policy(&server.url, "POST", 16, 5),
        &[request(1, "local", "x"), request(2, "local", "y")],
    );
    assert_eq!(
        replies(&output),
        vec![
            serde_json::json!({
                "id": 1,
                "error": "overflow: response exceeds the response cap"
            }),
            serde_json::json!({"id": 2, "error": "policy: total byte budget exceeded"}),
        ]
    );
    assert_requests(&server.finish(), &[("POST", "/remaining", b"x")]);
}

#[test]
fn last_byte_allows_empty_response_and_zero_grant_only_allows_empty_payloads() {
    let exact_server = LoopbackServer::start("/last-byte", vec![empty_reply()]);
    let exact = run_requests(
        policy(&exact_server.url, "POST", 0, 1),
        &[request(1, "local", "x")],
    );
    assert_eq!(
        replies(&exact),
        vec![serde_json::json!({"id": 1, "status": 200, "text": ""})]
    );
    assert_requests(&exact_server.finish(), &[("POST", "/last-byte", b"x")]);

    let zero_server = LoopbackServer::start("/zero", vec![empty_reply()]);
    let zero = run_requests(
        policy(&zero_server.url, "POST", 0, 0),
        &[request(1, "local", ""), request(2, "local", "x")],
    );
    assert_eq!(
        replies(&zero),
        vec![
            serde_json::json!({"id": 1, "status": 200, "text": ""}),
            serde_json::json!({"id": 2, "error": "policy: total byte budget exceeded"}),
        ]
    );
    assert_requests(&zero_server.finish(), &[("POST", "/zero", b"")]);
}

#[test]
fn admitted_get_body_stays_debited_after_transport_failure() {
    let server = LoopbackServer::start("/transport-failure", vec![Reply::Drop]);
    let output = run_requests(
        policy(&server.url, "GET", 8, 4),
        &[request(1, "local", "abc"), request(2, "local", "de")],
    );
    assert_eq!(
        replies(&output),
        vec![
            serde_json::json!({"id": 1, "error": "network"}),
            serde_json::json!({"id": 2, "error": "policy: total byte budget exceeded"}),
        ]
    );
    assert_requests(&server.finish(), &[("GET", "/transport-failure", b"")]);
}

#[test]
fn partial_response_read_failure_exhausts_remaining_allowance() {
    let server = LoopbackServer::start(
        "/partial",
        vec![Reply::Partial {
            declared_len: 5,
            body: b"ab".to_vec(),
        }],
    );
    let output = run_requests(
        policy(&server.url, "POST", 8, 4),
        &[request(1, "local", "x"), request(2, "local", "y")],
    );
    assert_eq!(
        replies(&output),
        vec![
            serde_json::json!({
                "id": 1,
                "error": "overflow: response exceeds the response cap"
            }),
            serde_json::json!({"id": 2, "error": "policy: total byte budget exceeded"}),
        ]
    );
    assert_requests(&server.finish(), &[("POST", "/partial", b"x")]);
}

#[test]
fn non_2xx_body_is_returned_and_charged() {
    let server = LoopbackServer::start(
        "/non-2xx",
        vec![Reply::Body {
            status: 429,
            reason: "Too Many Requests",
            body: "é".as_bytes().to_vec(),
        }],
    );
    let output = run_requests(
        policy(&server.url, "POST", 8, 3),
        &[request(1, "local", "x"), request(2, "local", "y")],
    );
    assert_eq!(
        replies(&output),
        vec![
            serde_json::json!({"id": 1, "status": 429, "text": "é"}),
            serde_json::json!({"id": 2, "error": "policy: total byte budget exceeded"}),
        ]
    );
    assert_requests(&server.finish(), &[("POST", "/non-2xx", b"x")]);
}
