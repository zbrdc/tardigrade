//! Pin the lifetime of session-owned host I/O across `--serve` commands.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const COMMAND_TIMEOUT: Duration = Duration::from_millis(700);
const EXTERNAL_DEADLINE: Duration = Duration::from_secs(12);
const IO_DEADLINE: Duration = Duration::from_secs(8);

fn script(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}.js"));
    std::fs::write(&path, "").expect("write empty serve entry");
    path
}

struct HttpRequest {
    method: String,
    path: String,
    body: String,
}

/// A one-request loopback endpoint. It reports the complete request before
/// holding the response behind an explicit test-controlled release barrier.
struct ControlledServer {
    port: u16,
    request: Receiver<Result<HttpRequest, String>>,
    release: Option<Sender<()>>,
    completed: Receiver<io::Result<()>>,
    worker: Option<JoinHandle<()>>,
}

impl ControlledServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback server");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let (request_tx, request) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (completed_tx, completed) = mpsc::channel();
        let worker = thread::spawn(move || {
            let accept_deadline = Instant::now() + IO_DEADLINE;
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(pair) => break pair,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        if Instant::now() >= accept_deadline {
                            return;
                        }
                        match release_rx.recv_timeout(Duration::from_millis(10)) {
                            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                            Err(RecvTimeoutError::Timeout) => {}
                        }
                    }
                    Err(_) => return,
                }
            };
            stream.set_read_timeout(Some(IO_DEADLINE)).unwrap();
            stream.set_write_timeout(Some(IO_DEADLINE)).unwrap();
            let parsed = read_request(&mut stream).map_err(|e| e.to_string());
            if request_tx.send(parsed).is_err() {
                return;
            }
            if release_rx.recv_timeout(IO_DEADLINE).is_ok() {
                let result = stream.write_all(
                    b"HTTP/1.1 201 Created\r\nContent-Type: text/plain\r\nContent-Length: 6\r\nConnection: close\r\n\r\nlate-A",
                );
                let _ = completed_tx.send(result);
            }
        });
        Self {
            port,
            request,
            release: Some(release),
            completed,
            worker: Some(worker),
        }
    }

    fn wait_for_request(&self) -> HttpRequest {
        self.request
            .recv_timeout(IO_DEADLINE)
            .expect("server did not receive request before deadline")
            .unwrap_or_else(|e| panic!("could not read controlled request: {e}"))
    }

    fn unblock_sender(&self) -> Option<Sender<()>> {
        self.release.as_ref().cloned()
    }

    /// Unblock the endpoint and wait until its late response write was tried.
    fn release_response(&mut self) -> io::Result<()> {
        self.release
            .take()
            .expect("response already released")
            .send(())
            .expect("server stopped before response release");
        let result = self
            .completed
            .recv_timeout(IO_DEADLINE)
            .expect("server did not finish its delayed response");
        if let Some(worker) = self.worker.take() {
            worker.join().expect("controlled server worker panicked");
        }
        result
    }
}

impl Drop for ControlledServer {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> io::Result<HttpRequest> {
    const MAX_REQUEST: usize = 16 * 1024;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    let (header_end, body_len) = loop {
        let count = stream.read(&mut chunk)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "EOF before complete HTTP request",
            ));
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.len() > MAX_REQUEST {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "controlled request exceeded bound",
            ));
        }
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let header = std::str::from_utf8(&bytes[..end])
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let body_len = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            break (end, body_len);
        }
    };
    let body_start = header_end + 4;
    let body_end = body_start
        .checked_add(body_len)
        .filter(|end| *end <= MAX_REQUEST)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "controlled request exceeded bound",
            )
        })?;
    while bytes.len() < body_end {
        let count = stream.read(&mut chunk)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "EOF before complete HTTP body",
            ));
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.len() > MAX_REQUEST {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "controlled request exceeded bound",
            ));
        }
    }
    let headers = std::str::from_utf8(&bytes[..header_end])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut request_line = headers
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace();
    let method = request_line.next().unwrap_or_default().to_owned();
    let path = request_line.next().unwrap_or_default().to_owned();
    let body = std::str::from_utf8(&bytes[body_start..body_end])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        .to_owned();
    Ok(HttpRequest { method, path, body })
}

fn assert_request(server: &ControlledServer) {
    let request = server.wait_for_request();
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/delay");
    assert_eq!(request.body, "started-A");
}

fn serve_args(path: &Path, port: u16) -> Vec<String> {
    vec![
        format!("--command-timeout={}", COMMAND_TIMEOUT.as_millis()),
        "--timeout=12000".to_owned(),
        "--net-timeout=10000".to_owned(),
        format!("--allow-call=delayed=http://127.0.0.1:{port}/delay,POST"),
        "--net-allow-private".to_owned(),
        "--serve".to_owned(),
        path.to_string_lossy().into_owned(),
    ]
}

struct ServeChild {
    child: Child,
    input: Option<ChildStdin>,
    unblock_server_on_drop: Option<Sender<()>>,
    stdout: Receiver<io::Result<String>>,
    stderr: Receiver<io::Result<String>>,
    readers: Vec<JoinHandle<()>>,
}

fn line_reader<R: Read + Send + 'static>(
    reader: R,
    sender: Sender<io::Result<String>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => return,
                Ok(_) => {
                    if sender.send(Ok(line)).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = sender.send(Err(e));
                    return;
                }
            }
        }
    })
}

impl ServeChild {
    fn start(path: &Path, port: u16, unblock_server_on_drop: Option<Sender<()>>) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tardigrade"))
            .args(serve_args(path, port))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn tardigrade serve child");
        let input = child.stdin.take().expect("piped child stdin");
        let stdout = child.stdout.take().expect("piped child stdout");
        let stderr = child.stderr.take().expect("piped child stderr");
        let (stdout_tx, stdout_rx) = mpsc::channel();
        let (stderr_tx, stderr_rx) = mpsc::channel();
        Self {
            child,
            input: Some(input),
            unblock_server_on_drop,
            stdout: stdout_rx,
            stderr: stderr_rx,
            readers: vec![
                line_reader(stdout, stdout_tx),
                line_reader(stderr, stderr_tx),
            ],
        }
    }

    fn command(&mut self, expression: &str) {
        let input = self.input.as_mut().expect("serve stdin is open");
        input
            .write_all(expression.as_bytes())
            .and_then(|_| input.write_all(b"\n"))
            .and_then(|_| input.flush())
            .expect("send serve command");
    }

    fn stdout_line(&self) -> String {
        match self.stdout.recv_timeout(EXTERNAL_DEADLINE) {
            Ok(Ok(line)) => line,
            Ok(Err(e)) => panic!("read serve stdout: {e}"),
            Err(e) => panic!("no serve reply before external deadline: {e}"),
        }
    }

    fn wait_for_stderr_marker(&self, marker: &str) {
        let deadline = Instant::now() + EXTERNAL_DEADLINE;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "stderr marker {marker:?} not received"
            );
            match self.stderr.recv_timeout(remaining) {
                Ok(Ok(line)) if line.contains(marker) => return,
                Ok(Ok(_)) => {}
                Ok(Err(e)) => panic!("read serve stderr: {e}"),
                Err(e) => panic!("stderr marker {marker:?} not received: {e}"),
            }
        }
    }

    fn close_stdin(&mut self) {
        self.input.take();
    }

    fn wait_exit(&mut self) -> i32 {
        let deadline = Instant::now() + EXTERNAL_DEADLINE;
        loop {
            match self.child.try_wait().expect("poll serve child") {
                Some(status) => return status.code().unwrap_or(-1),
                None if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                None => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    panic!("serve child did not exit before external deadline");
                }
            }
        }
    }

    fn assert_no_more_stdout(&self) {
        match self.stdout.recv_timeout(EXTERNAL_DEADLINE) {
            Err(RecvTimeoutError::Disconnected) => {}
            Ok(Ok(line)) => panic!("unexpected extra stdout reply: {line:?}"),
            Ok(Err(e)) => panic!("read serve stdout: {e}"),
            Err(RecvTimeoutError::Timeout) => panic!("stdout reader did not finish"),
        }
    }
}

impl Drop for ServeChild {
    fn drop(&mut self) {
        self.input.take();
        if let Some(unblock) = self.unblock_server_on_drop.take() {
            let _ = unblock.send(());
        }
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

#[test]
fn timed_out_command_keeps_session_io_and_reply_framing() {
    let mut server = ControlledServer::start();
    let path = script("serve-io-lifetime-recovery");
    let mut child = ServeChild::start(&path, server.port, server.unblock_sender());

    child.command("globalThis.saved = vm.net.request('delayed', 'started-A')");
    assert_request(&server);
    assert_eq!(
        child.stdout_line(),
        "{\"$error\":\"command timed out\"}\n",
        "command A must time out only after the endpoint received its request"
    );

    child.command("40 + 2");
    assert_eq!(child.stdout_line(), "42\n");

    child.command(
        "(console.error('awaiting-saved-request'), saved.then(({status, text}) => ({status, text})))",
    );
    child.wait_for_stderr_marker("awaiting-saved-request");
    server
        .release_response()
        .expect("the controlled endpoint should still have a live session request");
    assert_eq!(
        child.stdout_line(),
        "{\"status\":201,\"text\":\"late-A\"}\n"
    );

    child.command("6 * 7");
    assert_eq!(child.stdout_line(), "42\n");
    child.close_stdin();
    assert_eq!(child.wait_exit(), 0);
    child.assert_no_more_stdout();
}

#[test]
fn closed_session_io_cannot_complete_in_a_fresh_run() {
    let mut server = ControlledServer::start();
    let path = script("serve-io-lifetime-isolation");
    let mut old_session = ServeChild::start(&path, server.port, server.unblock_sender());

    old_session.command("globalThis.saved = vm.net.request('delayed', 'started-A')");
    assert_request(&server);
    assert_eq!(
        old_session.stdout_line(),
        "{\"$error\":\"command timed out\"}\n"
    );
    old_session.close_stdin();
    assert_eq!(old_session.wait_exit(), 0);

    // The server's delayed response is released only after the owning
    // session has exited, and its write attempt completes before a new
    // process is started.
    let _ = server.release_response();

    let mut fresh_run = ServeChild::start(&path, server.port, server.unblock_sender());
    fresh_run.command("typeof saved");
    assert_eq!(fresh_run.stdout_line(), "\"undefined\"\n");
    fresh_run.close_stdin();
    assert_eq!(fresh_run.wait_exit(), 0);
    fresh_run.assert_no_more_stdout();
}
