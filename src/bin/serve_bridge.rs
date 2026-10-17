//! HTTP face for a tardigrade guest app: the original demonstration
//! bridge. The concurrent production bridge is `tardigrade-bridge`
//! (docs/BRIDGE_CONTRACT.md); both share their HTTP limits and reply
//! taxonomy through `tardigrade::bridge_http`.
//!
//! Owns the real localhost socket (trusted host code) and relays each
//! request into the guest as one JS expression over the tardigrade --serve
//! console. The guest keeps its zero-syscall guarantee. A dead guest is
//! answered 502 and restarted instead of panicking. Guest `console.log`
//! output still shares the guest's stdout with serve replies (see
//! docs/KNOWN_BUGS.md); debug with `console.error`.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};

use tardigrade::bridge_http::{
    map_reply, read_request, respond, ReadOutcome, Request, SOCKET_TIMEOUT,
};

/// Each relayed request becomes one serve command; a bound keeps one
/// stuck handler from parking the bridge forever (the reply for an
/// expired bound is the fixed "command timed out").
const BRIDGE_COMMAND_TIMEOUT_MS: u64 = 30_000;

struct Guest {
    proc: Child,
    out: BufReader<std::process::ChildStdout>,
    tardigrade: String,
    app: String,
}

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
    let (status, ctype, body) = map_reply(&out);
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
