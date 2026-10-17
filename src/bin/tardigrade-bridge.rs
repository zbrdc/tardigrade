//! Concurrent production bridge (P2, docs/BRIDGE_CONTRACT.md).
//!
//! A pool of `tardigrade --serve` workers, each in its own process group,
//! serves one request at a time (docs/SERVING_CONTRACT.md). Overload gets
//! a defined 503; a dead worker answers 502 and is group-killed, reaped
//! and replaced, so its net-sidecar cannot survive it (P0 semantics).

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};

use tardigrade::bridge_http::{map_reply, read_request, respond, ReadOutcome, SOCKET_TIMEOUT};

struct Worker {
    proc: Child,
    out: BufReader<std::process::ChildStdout>,
}

impl Worker {
    fn spawn(
        tardigrade: &str,
        command_timeout_ms: u64,
        guest_args: &[String],
        app: &str,
    ) -> Option<Worker> {
        let mut cmd = Command::new(tardigrade);
        cmd.arg("--serve")
            .arg(format!("--command-timeout={}", command_timeout_ms));
        for flag in guest_args {
            cmd.arg(flag);
        }
        let mut proc = cmd
            .arg(app)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Own process group: a restart group-kills the worker and any
            // sidecar it spawned; the bridge itself is never in the group.
            .process_group(0)
            .spawn()
            .ok()?;
        let stdout = proc.stdout.take().expect("worker stdout was piped");
        Some(Worker {
            proc,
            // One persistent reader: a fresh BufReader per call could
            // buffer past the reply line and silently discard the rest.
            out: BufReader::new(stdout),
        })
    }

    /// One serve command; None means the worker died or stopped answering.
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

    /// Group-kill and reap: P0 semantics, so a sidecar cannot survive.
    fn kill(&mut self) {
        let pgid = self.proc.id() as i32;
        // SAFETY: kill(2) to the worker's own process group only.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
        let _ = self.proc.wait();
    }
}

struct Pool {
    idle: Mutex<Vec<Worker>>,
    free: Condvar,
    waiting: Mutex<usize>,
    queue_depth: usize,
    tardigrade: String,
    command_timeout_ms: u64,
    guest_args: Vec<String>,
    app: String,
}

impl Pool {
    /// Takes an idle worker, or registers as waiting. Err(()) is overload:
    /// queue_depth requests are already waiting for a worker.
    fn acquire(&self) -> Result<Worker, ()> {
        {
            let mut idle = self.idle.lock().unwrap();
            if let Some(worker) = idle.pop() {
                return Ok(worker);
            }
        }
        let mut waiting = self.waiting.lock().unwrap();
        if *waiting >= self.queue_depth {
            return Err(());
        }
        *waiting += 1;
        drop(waiting);
        let mut idle = self.idle.lock().unwrap();
        loop {
            if let Some(worker) = idle.pop() {
                let mut waiting = self.waiting.lock().unwrap();
                *waiting -= 1;
                return Ok(worker);
            }
            idle = self.free.wait(idle).unwrap();
        }
    }

    fn release(&self, worker: Worker) {
        let mut idle = self.idle.lock().unwrap();
        idle.push(worker);
        drop(idle);
        self.free.notify_all();
    }

    /// Replaces a dead worker; the replacement is released into the pool.
    fn replace(&self, mut dead: Worker) {
        dead.kill();
        if let Some(w) = Worker::spawn(
            &self.tardigrade,
            self.command_timeout_ms,
            &self.guest_args,
            &self.app,
        ) {
            self.release(w);
        }
    }
}

fn serve_connection(stream: &mut TcpStream, pool: &Pool) {
    let _ = stream.set_read_timeout(Some(SOCKET_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SOCKET_TIMEOUT));
    let req = match read_request(stream) {
        ReadOutcome::Req(req) => req,
        ReadOutcome::TooLarge => {
            respond(stream, 413, "text/plain", "request too large");
            return;
        }
        ReadOutcome::Bad => {
            respond(stream, 400, "text/plain", "bad request");
            return;
        }
    };
    let mut worker = match pool.acquire() {
        Ok(w) => w,
        Err(()) => {
            respond(stream, 503, "text/plain", "service overloaded");
            return;
        }
    };
    // The request is embedded as a JSON literal built by serde_json, so the
    // expression is always one syntactically safe line.
    let payload = serde_json::json!({
        "method": req.method,
        "path": req.path,
        "body": req.body,
    });
    let expr = format!("handler({})", payload);
    match worker.call(&expr) {
        Some(out) => {
            let (status, ctype, body) = map_reply(&out);
            respond(stream, status, &ctype, &body);
            pool.release(worker);
        }
        None => {
            respond(stream, 502, "text/plain", "worker unavailable");
            pool.replace(worker);
        }
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: tardigrade-bridge [--addr=127.0.0.1:8078] [--workers=N] [--queue-depth=Q] [--command-timeout=MS] -- APP [GUEST_FLAGS...]"
    );
    std::process::exit(2);
}

fn main() {
    let mut addr = "127.0.0.1:8078".to_string();
    let mut workers = 2usize;
    let mut queue_depth = 0usize;
    let mut command_timeout_ms = 30_000u64;
    let mut rest: Vec<String> = Vec::new();
    let mut argv = std::env::args().skip(1).peekable();
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--" => rest.extend(argv.by_ref()),
            _ if arg.starts_with("--addr=") => addr = arg.trim_start_matches("--addr=").to_string(),
            _ if arg.starts_with("--workers=") => {
                workers = arg
                    .trim_start_matches("--workers=")
                    .parse()
                    .unwrap_or_else(|_| usage());
            }
            _ if arg.starts_with("--queue-depth=") => {
                queue_depth = arg
                    .trim_start_matches("--queue-depth=")
                    .parse()
                    .unwrap_or_else(|_| usage());
            }
            _ if arg.starts_with("--command-timeout=") => {
                command_timeout_ms = arg
                    .trim_start_matches("--command-timeout=")
                    .parse()
                    .unwrap_or_else(|_| usage());
            }
            _ => usage(),
        }
    }
    if rest.is_empty() || !(1..=64).contains(&workers) {
        usage();
    }
    let app = rest.remove(0);
    let tardigrade = std::env::var("OUT").unwrap_or_else(|_| "tardigrade".to_string());

    let pool = Arc::new(Pool {
        idle: Mutex::new(Vec::new()),
        free: Condvar::new(),
        waiting: Mutex::new(0),
        queue_depth,
        tardigrade: tardigrade.clone(),
        command_timeout_ms,
        guest_args: rest,
        app,
    });
    for _ in 0..workers {
        let w = Worker::spawn(&tardigrade, command_timeout_ms, &pool.guest_args, &pool.app)
            .expect("spawn bridge worker");
        pool.release(w);
    }

    let listener = TcpListener::bind(&addr).expect("bind");
    let bound = listener.local_addr().expect("local addr");
    println!(
        "serving {} from {} workers on http://{}",
        pool.app, workers, bound
    );
    // The startup line is a rendezvous for whoever spawned the bridge:
    // stdout to a pipe is block-buffered, so flush it explicitly.
    let _ = std::io::stdout().flush();
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            continue;
        };
        let pool = pool.clone();
        std::thread::spawn(move || serve_connection(&mut stream, &pool));
    }
}
