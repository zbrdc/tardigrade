//! Exercise the real net-sidecar protocol directly, without the JavaScript API.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output, Stdio};
use std::thread;

const POLICY_FRAME_CAP: usize = 1024 * 1024;
const REQUEST_FRAME_HEADROOM: usize = 1024 * 1024 + 4096;

fn sidecar(input: Vec<u8>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tardigrade"))
        .arg("net-sidecar")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn real sidecar");
    let mut stdin = child.stdin.take().unwrap();
    let feeder = thread::spawn(move || {
        // An early, fail-closed sidecar exit can close stdin before the whole
        // oversized test frame is written.
        let _ = stdin.write_all(&input);
    });
    let output = child.wait_with_output().expect("wait for sidecar");
    feeder.join().unwrap();
    output
}

fn policy_value(url: &str, max_response_bytes: u64, total_bytes: u64) -> serde_json::Value {
    serde_json::json!({
        "policy": {
            "calls": {
                "local": {
                    "url": url,
                    "method": "POST"
                }
            },
            "max_response_bytes": max_response_bytes,
            "total_bytes": total_bytes,
            "timeout_ms": 3000,
            "allow_private": true
        }
    })
}

fn encode_frame(value: &serde_json::Value) -> Vec<u8> {
    let mut frame = serde_json::to_vec(value).unwrap();
    frame.push(b'\n');
    frame
}

fn policy_frame_of_size(size: usize) -> Vec<u8> {
    let mut value = policy_value("", 16, 0);
    let base_size = encode_frame(&value).len();
    assert!(size >= base_size);
    value["policy"]["calls"]["local"]["url"] =
        serde_json::Value::String("x".repeat(size - base_size));
    let frame = encode_frame(&value);
    assert_eq!(frame.len(), size);
    frame
}

fn request_frame_of_size(size: usize) -> Vec<u8> {
    let mut value = serde_json::json!({"id": 1, "cap": "unknown", "body": ""});
    let base_size = encode_frame(&value).len();
    assert!(size >= base_size);
    value["body"] = serde_json::Value::String("x".repeat(size - base_size));
    let frame = encode_frame(&value);
    assert_eq!(frame.len(), size);
    frame
}

fn append_policy(input: &mut Vec<u8>, value: &serde_json::Value) {
    input.extend_from_slice(&encode_frame(value));
}

fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        assert_ne!(
            reader.read_line(&mut line).unwrap(),
            0,
            "request headers ended"
        );
        if line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap();
            }
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).unwrap();
    body
}

fn respond(stream: &mut TcpStream, status: &str, body: &[u8]) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(body).unwrap();
}

#[test]
fn policy_frame_receiver_accepts_the_exact_limit_and_rejects_over_it() {
    let exact = policy_frame_of_size(POLICY_FRAME_CAP);
    let output = sidecar(exact);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stdout.is_empty(),
        "a policy alone should not produce replies"
    );

    let output = sidecar(policy_frame_of_size(POLICY_FRAME_CAP + 1));
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}

#[test]
fn request_frame_exact_limit_is_accepted_but_oversize_stops_before_its_tail() {
    // total_bytes=0 gives the specified request allowance of 1 MiB + 4096.
    let cap = REQUEST_FRAME_HEADROOM;
    let mut exact = Vec::new();
    append_policy(&mut exact, &policy_value("", 16, 0));
    exact.extend_from_slice(&request_frame_of_size(cap));
    let output = sidecar(exact);
    assert_eq!(output.status.code(), Some(0));
    let reply: serde_json::Value =
        serde_json::from_slice(output.stdout.strip_suffix(b"\n").unwrap()).unwrap();
    assert_eq!(reply["error"], "policy: unknown capability");

    let mut oversized = Vec::new();
    append_policy(&mut oversized, &policy_value("", 16, 0));
    oversized.extend_from_slice(&request_frame_of_size(cap + 1));
    oversized.extend_from_slice(&encode_frame(&serde_json::json!({
        "id": 2,
        "cap": "unknown",
        "body": ""
    })));
    let output = sidecar(oversized);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        output.stdout.is_empty(),
        "the sidecar must not parse an oversized frame or its tail"
    );
}

#[test]
fn unterminated_and_malformed_frames_fail_closed() {
    let mut truncated_policy = encode_frame(&policy_value("", 16, 0));
    truncated_policy.pop();
    let output = sidecar(truncated_policy);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());

    let mut truncated_request = Vec::new();
    append_policy(&mut truncated_request, &policy_value("", 16, 0));
    truncated_request.extend_from_slice(br#"{"id":1,"cap":"unknown","body":"tail""#);
    let output = sidecar(truncated_request);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());

    let mut malformed_request = Vec::new();
    append_policy(&mut malformed_request, &policy_value("", 16, 0));
    malformed_request.extend_from_slice(b"{\"id\":,\n");
    malformed_request.extend_from_slice(&encode_frame(&serde_json::json!({
        "id": 2,
        "cap": "unknown",
        "body": ""
    })));
    let output = sidecar(malformed_request);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        output.stdout.is_empty(),
        "malformed frame tail was processed"
    );
}

#[test]
fn ordinary_request_json_escaping_survives_the_real_sidecar() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/echo", listener.local_addr().unwrap());
    let body = "quote \" slash \\ newline\n tab\t nul\0 control\u{0001} snowman ☃";
    let expected_body = body.as_bytes().to_vec();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let received = read_http_request(&mut stream);
        respond(&mut stream, "200 OK", &received);
        received
    });

    let mut input = Vec::new();
    append_policy(&mut input, &policy_value(&url, 1024, 1024));
    input.extend_from_slice(&encode_frame(&serde_json::json!({
        "id": 7,
        "cap": "local",
        "body": body
    })));
    let output = sidecar(input);
    let received = server.join().unwrap();
    assert_eq!(received, expected_body);
    assert_eq!(output.status.code(), Some(0));
    let reply: serde_json::Value =
        serde_json::from_slice(output.stdout.strip_suffix(b"\n").unwrap()).unwrap();
    assert_eq!(reply["id"], 7);
    assert_eq!(reply["status"], 200);
    assert_eq!(reply["text"], body);
}

#[test]
fn response_body_cap_keeps_exact_responses_and_bounds_oversized_replies() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/reply", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        for response in [b"1234".as_slice(), b"12345".as_slice()] {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_http_request(&mut stream);
            respond(&mut stream, "200 OK", response);
        }
    });

    let mut input = Vec::new();
    append_policy(&mut input, &policy_value(&url, 4, 64));
    for id in [1, 2] {
        input.extend_from_slice(&encode_frame(&serde_json::json!({
            "id": id,
            "cap": "local",
            "body": ""
        })));
    }
    let output = sidecar(input);
    server.join().unwrap();
    assert_eq!(output.status.code(), Some(0));

    let mut lines = output.stdout.split(|&byte| byte == b'\n');
    let exact: serde_json::Value = serde_json::from_slice(lines.next().unwrap()).unwrap();
    let overflow: serde_json::Value = serde_json::from_slice(lines.next().unwrap()).unwrap();
    assert_eq!(exact["status"], 200);
    assert_eq!(exact["text"], "1234");
    assert_eq!(
        overflow["error"],
        "overflow: response exceeds the response cap"
    );
    assert!(lines.next().unwrap().is_empty());
    assert!(
        output.stdout.len() <= 2 * (6 * 4 + 4096),
        "sidecar response framing exceeded its bounded allowance"
    );
}
