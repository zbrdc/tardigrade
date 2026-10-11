//! HTTP face for a v8vm guest app.
//!
//! Owns the real localhost socket (trusted host code) and relays each
//! request into the guest as one JS expression over the v8vm --serve
//! console. The guest keeps its zero-syscall guarantee.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};

struct Guest {
    proc: Child,
}

impl Guest {
    fn new(v8vm: &str, app: &str) -> Guest {
        let proc = Command::new(v8vm)
            .arg("--serve")
            .arg(app)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn v8vm --serve");
        Guest { proc }
    }

    /// Sends one JS expression; returns the guest's JSON line.
    fn call(&mut self, expr: &str) -> String {
        let stdin = self.proc.stdin.as_mut().expect("guest stdin");
        writeln!(stdin, "{}", expr).expect("write command");
        stdin.flush().expect("flush command");
        let stdout = self.proc.stdout.as_mut().expect("guest stdout");
        let mut line = String::new();
        BufReader::new(stdout)
            .read_line(&mut line)
            .expect("read response");
        line.trim_end().to_string()
    }
}

struct Request {
    method: String,
    path: String,
    body: String,
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some(v) = header
            .to_ascii_lowercase()
            .strip_prefix("content-length:")
        {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).ok()?;
    }
    Some(Request {
        method,
        path,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn relay(stream: &mut TcpStream, guest: &mut Guest, req: &Request) {
    // The request is embedded as a JSON literal built by serde_json, so the
    // expression is always one syntactically safe line.
    let payload = serde_json::json!({
        "method": req.method,
        "path": req.path,
        "body": req.body,
    });
    let expr = format!("handler({})", payload.to_string());
    let out = guest.call(&expr);

    let (status, ctype, body): (i64, String, String) = match serde_json::from_str::<serde_json::Value>(&out) {
        Ok(v) if v.get("$error").is_some() => (
            500,
            "text/plain".to_string(),
            v.get("$error").and_then(|e| e.as_str()).unwrap_or("error").to_string(),
        ),
        Ok(v) => (
            v.get("status").and_then(|s| s.as_i64()).unwrap_or(200),
            v.get("type").and_then(|t| t.as_str()).unwrap_or("text/html").to_string(),
            v.get("body").and_then(|b| b.as_str()).unwrap_or("").to_string(),
        ),
        Err(_) => (200, "text/plain".to_string(), out),
    };

    let response = format!(
        "HTTP/1.1 {} OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        ctype,
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn main() {
    let v8vm = std::env::var("OUT").unwrap_or_else(|_| "v8vm".to_string());
    let app = std::env::args().nth(1).unwrap_or_else(|| "examples/webapp.js".to_string());
    let port: u16 = std::env::args()
        .nth(2)
        .and_then(|p| p.parse().ok())
        .unwrap_or(8077);

    let mut guest = Guest::new(&v8vm, &app);
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind");
    println!("serving {} from v8vm on http://127.0.0.1:{}", app, port);

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        if let Some(req) = read_request(&mut stream) {
            relay(&mut stream, &mut guest, &req);
        }
    }
}
