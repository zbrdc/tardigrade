//! HTTP face for a tardigrade guest app.
//!
//! Owns the real localhost socket (trusted host code) and relays each
//! request into the guest as one JS expression over the tardigrade --serve
//! console. The guest keeps its zero-syscall guarantee.
//!
//! Hardening, in one place: request bodies and header lines are capped
//! before allocation, sockets carry read/write timeouts so one silent
//! client cannot stall the bridge, guest-supplied status codes and
//! content types are validated before they reach the response line, and
//! a dead guest is answered 502 and restarted instead of panicking.
//! Guest `console.log` output still shares the guest's stdout with serve
//! replies (see docs/KNOWN_BUGS.md); debug with `console.error`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Largest request body the bridge will allocate for, whatever
/// Content-Length claims.
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Largest single header (or request) line, and most header lines.
const MAX_LINE_BYTES: usize = 8192;
const MAX_HEADERS: usize = 100;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(15);

struct Guest {
    proc: Child,
    out: BufReader<std::process::ChildStdout>,
    tardigrade: String,
    app: String,
}

/// Each relayed request becomes one serve command; a bound keeps one
/// stuck handler from parking the bridge forever (the reply for an
/// expired bound is the fixed "command timed out").
const BRIDGE_COMMAND_TIMEOUT_MS: u64 = 30_000;

impl Guest {
    fn new(tardigrade: &str, app: &str) -> std::io::Result<Guest> {
        let mut proc = Command::new(tardigrade)
            .arg("--serve")
            .arg(format!("--command-timeout={}", BRIDGE_COMMAND_TIMEOUT_MS))
            .arg(app)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let stdout = proc.stdout.take().expect("guest stdout was piped");
        Ok(Guest {
            proc,
            // One persistent reader: a fresh BufReader per call could
            // buffer past the reply line and silently discard the rest.
            out: BufReader::new(stdout),
            tardigrade: tardigrade.to_string(),
            app: app.to_string(),
        })
    }

    /// Sends one JS expression; returns the guest's JSON line, or None
    /// when the guest died or stopped answering.
    fn call(&mut self, expr: &str) -> Option<String> {
        let stdin = self.proc.stdin.as_mut()?;
        writeln!(stdin, "{}", expr).ok()?;
        stdin.flush().ok()?;
        let mut line = String::new();
        match self.out.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim_end().to_string()),
        }
    }

    fn restart(&mut self) {
        let _ = self.proc.kill();
        let _ = self.proc.wait();
        if let Ok(g) = Guest::new(&self.tardigrade.clone(), &self.app.clone()) {
            *self = g;
        }
    }
}

struct Request {
    method: String,
    path: String,
    body: String,
}

enum ReadOutcome {
    Req(Request),
    /// Content-Length over the cap: answer 413 without reading it.
    TooLarge,
    /// Malformed, oversized headers, or the client went away.
    Bad,
}

/// Reads one line of at most MAX_LINE_BYTES (through the reader's fill,
/// so an unterminated flood cannot grow memory first). None: too long,
/// EOF, or an I/O error.
fn read_capped_line(reader: &mut impl BufRead) -> Option<String> {
    let mut line = Vec::new();
    loop {
        let avail = reader.fill_buf().ok()?;
        if avail.is_empty() {
            return if line.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(&line).into_owned())
            };
        }
        match avail.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                if line.len() + pos > MAX_LINE_BYTES {
                    return None;
                }
                line.extend_from_slice(&avail[..=pos]);
                reader.consume(pos + 1);
                return Some(String::from_utf8_lossy(&line).into_owned());
            }
            None => {
                if line.len() + avail.len() > MAX_LINE_BYTES {
                    return None;
                }
                let n = avail.len();
                line.extend_from_slice(avail);
                reader.consume(n);
            }
        }
    }
}

fn read_request(stream: &mut TcpStream) -> ReadOutcome {
    let Ok(clone) = stream.try_clone() else {
        return ReadOutcome::Bad;
    };
    let mut reader = BufReader::new(clone);
    let Some(request_line) = read_capped_line(&mut reader) else {
        return ReadOutcome::Bad;
    };
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return ReadOutcome::Bad;
    };
    let method = method.to_string();
    let path = path.to_string();

    let mut content_length = 0usize;
    for _ in 0..MAX_HEADERS {
        let Some(header) = read_capped_line(&mut reader) else {
            return ReadOutcome::Bad;
        };
        let header = header.trim_end();
        if header.is_empty() {
            // End of headers.
            let mut body = vec![0u8; content_length];
            if content_length > 0 && reader.read_exact(&mut body).is_err() {
                return ReadOutcome::Bad;
            }
            return ReadOutcome::Req(Request {
                method,
                path,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            match v.trim().parse::<usize>() {
                Ok(n) if n > MAX_BODY_BYTES => return ReadOutcome::TooLarge,
                Ok(n) => content_length = n,
                // An unparsable length is a bad request, not a zero body.
                Err(_) => return ReadOutcome::Bad,
            }
        }
    }
    ReadOutcome::Bad
}

/// Only a plausible HTTP status reaches the response line; anything else
/// the guest produced is a bridge-level 502.
fn valid_status(v: Option<i64>) -> Option<u16> {
    match v {
        Some(n) if (100..=599).contains(&n) => Some(n as u16),
        _ => None,
    }
}

/// A content type with controls (CR/LF above all) would inject headers;
/// fall back to a fixed type instead of forwarding it.
fn sanitize_content_type(v: Option<&str>) -> String {
    match v {
        Some(t) if !t.is_empty() && t.chars().all(|c| c.is_ascii() && !c.is_ascii_control()) => {
            t.to_string()
        }
        _ => "text/plain".to_string(),
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Content Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

fn respond(stream: &mut TcpStream, status: u16, ctype: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        reason(status),
        ctype,
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn relay(stream: &mut TcpStream, guest: &mut Guest, req: &Request) {
    // The request is embedded as a JSON literal built by serde_json, so the
    // expression is always one syntactically safe line.
    let payload = serde_json::json!({
        "method": req.method,
        "path": req.path,
        "body": req.body,
    });
    let expr = format!("handler({})", payload);
    let Some(out) = guest.call(&expr) else {
        respond(stream, 502, "text/plain", "guest unavailable");
        guest.restart();
        return;
    };

    let (status, ctype, body): (u16, String, String) =
        match serde_json::from_str::<serde_json::Value>(&out) {
            Ok(v)
                if v.get("$error")
                    .is_some_and(|e| e.as_str() == Some("command timed out")) =>
            {
                (
                    504,
                    "text/plain".to_string(),
                    "command timed out".to_string(),
                )
            }
            Ok(v) if v.get("$error").is_some() => (
                500,
                "text/plain".to_string(),
                v.get("$error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("error")
                    .to_string(),
            ),
            Ok(v) => match valid_status(v.get("status").and_then(|s| s.as_i64())) {
                Some(status) => (
                    status,
                    sanitize_content_type(v.get("type").and_then(|t| t.as_str())),
                    v.get("body")
                        .and_then(|b| b.as_str())
                        .unwrap_or("")
                        .to_string(),
                ),
                None if v.get("status").is_some() => (
                    502,
                    "text/plain".to_string(),
                    "guest returned an invalid status".to_string(),
                ),
                None => (
                    200,
                    sanitize_content_type(v.get("type").and_then(|t| t.as_str())),
                    v.get("body")
                        .and_then(|b| b.as_str())
                        .unwrap_or("")
                        .to_string(),
                ),
            },
            Err(_) => (200, "text/plain".to_string(), out),
        };

    respond(stream, status, &ctype, &body);
}

fn main() {
    let tardigrade = std::env::var("OUT").unwrap_or_else(|_| "tardigrade".to_string());
    let app = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "examples/webapp.js".to_string());
    let port: u16 = std::env::args()
        .nth(2)
        .and_then(|p| p.parse().ok())
        .unwrap_or(8077);

    let mut guest = Guest::new(&tardigrade, &app).expect("spawn tardigrade --serve");
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind");
    println!(
        "serving {} from tardigrade on http://127.0.0.1:{}",
        app, port
    );

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let _ = stream.set_read_timeout(Some(SOCKET_TIMEOUT));
        let _ = stream.set_write_timeout(Some(SOCKET_TIMEOUT));
        match read_request(&mut stream) {
            ReadOutcome::Req(req) => relay(&mut stream, &mut guest, &req),
            ReadOutcome::TooLarge => respond(&mut stream, 413, "text/plain", "request too large"),
            ReadOutcome::Bad => respond(&mut stream, 400, "text/plain", "bad request"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_must_be_a_real_http_status() {
        assert_eq!(valid_status(Some(200)), Some(200));
        assert_eq!(valid_status(Some(599)), Some(599));
        assert_eq!(valid_status(Some(99)), None);
        assert_eq!(valid_status(Some(600)), None);
        assert_eq!(valid_status(Some(-1)), None);
        assert_eq!(valid_status(None), None);
    }

    #[test]
    fn content_type_cannot_inject_headers() {
        assert_eq!(sanitize_content_type(Some("text/html")), "text/html");
        assert_eq!(
            sanitize_content_type(Some("text/html\r\nX-Evil: 1")),
            "text/plain"
        );
        assert_eq!(sanitize_content_type(Some("")), "text/plain");
        assert_eq!(sanitize_content_type(None), "text/plain");
    }

    #[test]
    fn capped_line_refuses_a_flood() {
        let data = vec![b'a'; MAX_LINE_BYTES + 100];
        let mut r = BufReader::new(&data[..]);
        assert!(read_capped_line(&mut r).is_none());
        let mut ok = BufReader::new(&b"GET / HTTP/1.1\r\n"[..]);
        assert_eq!(read_capped_line(&mut ok).unwrap(), "GET / HTTP/1.1\r\n");
    }
}
