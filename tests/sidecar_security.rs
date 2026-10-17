//! FU-34 sidecar security matrix (docs/PLATFORM_ROADMAP.md): the sidecar
//! is a security boundary. Redirects must be returned, never followed;
//! operator URL credentials must never reach guest-visible errors; a
//! hostname that resolves privately must be refused without the flag,
//! before any connection is made.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output, Stdio};
use std::thread;

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
        let _ = stdin.write_all(&input);
    });
    let output = child.wait_with_output().expect("wait for sidecar");
    feeder.join().unwrap();
    output
}

fn frames(url: &str, allow_private: bool) -> Vec<u8> {
    let policy = serde_json::json!({
        "policy": {
            "calls": {"local": {"url": url, "method": "POST"}},
            "max_response_bytes": 1024,
            "total_bytes": 8192,
            "timeout_ms": 3000,
            "allow_private": allow_private
        }
    });
    let mut out = serde_json::to_vec(&policy).unwrap();
    out.push(b'\n');
    let request = serde_json::json!({"id": 1, "cap": "local", "body": "x"});
    out.extend_from_slice(&serde_json::to_vec(&request).unwrap());
    out.push(b'\n');
    out
}

fn reply_of(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8(output.stdout.clone()).expect("sidecar stdout utf-8");
    let last = stdout.lines().last().expect("one reply line");
    serde_json::from_str(last).expect("reply is JSON")
}

/// Reads one HTTP request roughly (drain to the body), then answers.
fn answer_once(mut conn: TcpStream, response: &'static str) {
    let mut buf = [0u8; 1024];
    let _ = conn.read(&mut buf);
    let _ = conn.write_all(response.as_bytes());
    let _ = conn.flush();
}

#[test]
fn a_redirect_is_returned_not_followed() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        if let Ok((conn, _)) = listener.accept() {
            h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            answer_once(
                conn,
                "HTTP/1.1 302 Found\r\nLocation: /admin\r\nContent-Length: 0\r\n\r\n",
            );
        }
        // A followed redirect would arrive here as a second connection.
        thread::sleep(std::time::Duration::from_millis(500));
        hits
    });

    let out = sidecar(frames(&format!("http://127.0.0.1:{port}/start"), true));
    let reply = reply_of(&out);
    assert_eq!(reply["status"].as_u64(), Some(302), "reply: {reply}");
    let hits = server.join().unwrap();
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the sidecar must not follow the redirect"
    );
}

#[test]
fn operator_url_credentials_never_reach_errors() {
    // A port with no listener: the request must fail, and every
    // guest-visible surface must stay free of the URL's credentials.
    let gone = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = gone.local_addr().unwrap().port();
    drop(gone);

    let out = sidecar(frames(
        &format!("http://alice:seekrit@127.0.0.1:{port}/"),
        true,
    ));
    let reply = reply_of(&out);
    assert!(
        reply.get("error").is_some(),
        "expected an error reply: {reply}"
    );
    let all = format!(
        "{}{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
        reply
    );
    assert!(!all.contains("seekrit"), "credential leaked: {all}");
    assert!(!all.contains("alice:"), "user info leaked: {all}");
}

#[test]
fn a_private_hostname_is_refused_before_any_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();

    let out = sidecar(frames(&format!("http://localhost:{port}/"), false));
    let reply = reply_of(&out);
    let error = reply["error"].as_str().unwrap_or_default().to_lowercase();
    assert!(
        error.contains("private"),
        "expected a private-address refusal, got: {reply}"
    );
    assert!(
        matches!(listener.accept(), Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "no connection may be made to a privately resolving hostname"
    );
}
