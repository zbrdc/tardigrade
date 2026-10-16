//! Scoped network capabilities: vm.net.request(name, body).
//!
//! Design (docs/NETWORK_CAPABILITIES.md): the operator grants named HTTP
//! endpoints with --allow-call; the guest addresses them by name alone and
//! never sees a URL. Egress runs through a "tardigrade net-sidecar" child
//! spawned before the sandbox layers are applied: the guest process keeps one
//! pre-open socketpair fd to it (sandbox::apply preserves that fd instead of
//! closing it) and never gains a socket syscall of its own. The sidecar owns
//! the policy and performs the real HTTP, so a parsing bug on the guest side
//! of the wall cannot bypass it.

use std::collections::{HashMap, VecDeque};
use std::io::{BufReader, Read, Write};
use std::net::ToSocketAddrs;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::json;

use crate::diag::throw;
use crate::guest_cb;
use crate::webapi::throw_range;
use crate::world::world_mut;

pub const DEFAULT_MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
pub const DEFAULT_TOTAL_BYTES: u64 = 8 * 1024 * 1024;
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_POLICY_FRAME_BYTES: u64 = 1024 * 1024;
const FRAME_HEADROOM_BYTES: u64 = 4096;

/// One operator-declared endpoint. The URL and method never reach the guest:
/// it addresses the capability by name alone and sends a body.
#[derive(Clone, Debug)]
pub struct NetCall {
    pub name: String,
    pub url: String,
    pub method: String,
}

/// Operator-declared limits, mirrored into the guest's world so the binding
/// can refuse before paying the sidecar round-trip.
#[derive(Clone, Debug)]
pub struct NetLimits {
    pub max_response_bytes: u64,
    pub total_bytes: u64,
    pub timeout_ms: u64,
    pub allow_private: bool,
}

impl Default for NetLimits {
    fn default() -> Self {
        NetLimits {
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            total_bytes: DEFAULT_TOTAL_BYTES,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            allow_private: false,
        }
    }
}

/// The operator's network plan: granted calls plus limits. Built from the
/// --allow-call/--net-* flags (or by a library embedder directly).
#[derive(Clone, Debug, Default)]
pub struct NetPlan {
    pub calls: Vec<NetCall>,
    pub limits: NetLimits,
}

impl NetPlan {
    /// Granted capability names, for the guest-side fast rejection.
    pub fn names(&self) -> Vec<String> {
        self.calls.iter().map(|c| c.name.clone()).collect()
    }

    /// The sidecar's policy: everything it must enforce on its own, as JSON
    /// (the guest process holds no URLs, by design).
    pub fn policy_json(&self) -> String {
        let mut calls = serde_json::Map::new();
        for c in &self.calls {
            calls.insert(c.name.clone(), json!({"url": c.url, "method": c.method}));
        }
        json!({
            "calls": calls,
            "max_response_bytes": self.limits.max_response_bytes,
            "total_bytes": self.limits.total_bytes,
            "timeout_ms": self.limits.timeout_ms,
            "allow_private": self.limits.allow_private,
        })
        .to_string()
    }
}

fn request_frame_cap(total_bytes: u64) -> u64 {
    total_bytes
        .saturating_mul(6)
        .saturating_add(MAX_POLICY_FRAME_BYTES)
        .saturating_add(FRAME_HEADROOM_BYTES)
}

/// Encodes the policy envelope before any child process or socket is created.
/// The limit includes the terminating newline.
fn policy_frame(plan: &NetPlan) -> Result<Vec<u8>, String> {
    let line = format!("{{\"policy\": {}}}\n", plan.policy_json());
    if line.len() as u64 > MAX_POLICY_FRAME_BYTES {
        return Err("network policy exceeds the 1 MiB frame limit".to_string());
    }
    Ok(line.into_bytes())
}

/// Parses one --allow-call flag: NAME=URL or NAME=URL,METHOD (default POST).
pub fn parse_call(flag: &str) -> Result<NetCall, String> {
    let bad = || format!("bad --allow-call {flag:?}: expected NAME=URL[,METHOD]");
    let (names_urls, method) = match flag.rsplit_once(',') {
        Some((rest, m)) => (rest, m.to_ascii_uppercase()),
        None => (flag, "POST".to_string()),
    };
    let (name, url) = names_urls.split_once('=').ok_or_else(bad)?;
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(bad());
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!("bad --allow-call {flag:?}: URL must be http(s)"));
    }
    if method != "GET" && method != "POST" && method != "PUT" {
        return Err(format!(
            "bad --allow-call {flag:?}: METHOD must be GET, POST or PUT"
        ));
    }
    Ok(NetCall {
        name: name.to_string(),
        url: url.to_string(),
        method,
    })
}

/// True when the address is not routable on the public internet: loopback,
/// private ranges, link-local (including the cloud-metadata address), CGNAT,
/// unspecified and unique-local v6. The sidecar refuses endpoints that
/// resolve only to these unless the operator passed --net-allow-private.
pub fn addr_is_private(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let [a, b, _, _] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                // "this network" 0.0.0.0/8 (covers ::1 via to_ipv4)
                || a == 0
                // CGNAT 100.64.0.0/10
                || (a == 100 && (64..128).contains(&b))
                // IETF special registry 192.0.0.0/24, benchmarking
                // 198.18.0.0/15, documentation TEST-NETs, reserved 240/4
                || (a == 192 && b == 0)
                || (a == 198 && (18..20).contains(&b))
                || matches!((a, b), (192, 0) | (198, 51) | (203, 0))
                || a >= 240
        }
        std::net::IpAddr::V6(v6) => {
            // IPv4-mapped and IPv4-compatible forms inherit the v4 verdict:
            // ::ffff:127.0.0.1 must not slip past as a "public" v6 address.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return addr_is_private(std::net::IpAddr::V4(v4));
            }
            if let Some(v4) = v6.to_ipv4() {
                return addr_is_private(std::net::IpAddr::V4(v4));
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // unique local fc00::/7
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // link local fe80::/10
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // documentation 2001:db8::/32
                || (v6.segments()[0] == 0x2001 && v6.segments()[1] == 0x0db8)
        }
    }
}

/// The host:port a URL's authority names, parsed without a URL crate
/// (scheme://[user@]host[:port]/...). Private-address checks run on the
/// host before the port matters.
fn authority_host(url: &str) -> Result<String, String> {
    let rest = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .ok_or_else(|| format!("URL has no scheme: {url}"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    if let Some(rest) = authority.strip_prefix('[') {
        // [v6]:port
        let host = rest.split(']').next().ok_or("unterminated [host]")?;
        return Ok(host.to_string());
    }
    let host = authority.split(':').next().unwrap_or("");
    if host.is_empty() {
        return Err(format!("URL has no host: {url}"));
    }
    Ok(host.to_string())
}

/// True when any address the URL's host resolves to is private. Resolved
/// here, before the request: the sidecar's HTTP client resolves again, so
/// this is best effort against a racing resolver (documented in
/// KNOWN_RISKS.md once shipped).
pub fn url_resolves_private(url: &str) -> Result<bool, String> {
    let host = authority_host(url)?;
    let addrs: Vec<std::net::SocketAddr> = (host.as_str(), 0u16)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {host}: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("host does not resolve: {host}"));
    }
    Ok(addrs.iter().any(|a| addr_is_private(a.ip())))
}

/// The process-wide wire to the sidecar: one socketpair end, set by
/// [spawn_sidecar] before the sandbox layers close every other fd. The
/// guest's runs in this process all share it (one sidecar per process).
static WIRE: OnceLock<Arc<Mutex<UnixStream>>> = OnceLock::new();

/// DNS resolver for the sidecar that checks the addresses it returns:
/// ureq connects to exactly these addresses, so a name that rebinds
/// between a check and a connect cannot reach a private address. Any
/// private address in the answer fails the whole resolution (fail closed
/// on mixed answers) unless the operator allowed private endpoints.
struct CheckedResolver {
    allow_private: bool,
}

impl ureq::Resolver for CheckedResolver {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
        let addrs: Vec<std::net::SocketAddr> = netloc
            .to_socket_addrs()
            .map_err(|e| std::io::Error::new(e.kind(), format!("cannot resolve {netloc}: {e}")))?
            .collect();
        if addrs.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("host does not resolve: {netloc}"),
            ));
        }
        if !self.allow_private && addrs.iter().any(|a| addr_is_private(a.ip())) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "endpoint resolves to a private address",
            ));
        }
        Ok(addrs)
    }
}

/// The guest-visible slice of the plan, carried in Config: names for fast
/// rejection, limits for local refusal, and the wire fd the sandbox layer
/// preserved (see sandbox::apply).
#[derive(Clone, Debug)]
pub struct NetConfig {
    pub names: Vec<String>,
    pub max_response_bytes: u64,
    pub total_bytes: u64,
    pub timeout_ms: u64,
    pub allow_private: bool,
    pub wire_fd: RawFd,
}

/// The fd the guest's binding reads and writes. The sandbox layer must
/// preserve it (close_range skips it) - it is the one channel into the
/// sidecar, and it is kept open here for the process's lifetime.
pub struct NetSession {
    pub wire_fd: RawFd,
}

/// Creates the socketpair, spawns the sidecar, and registers the wire. Call
/// before sandbox::apply (the sidecar spawn needs to fork before seccomp, and
/// apply must preserve the returned wire_fd).
pub fn spawn_sidecar(exe: &std::path::Path, plan: &NetPlan) -> Result<NetSession, String> {
    let policy_line = policy_frame(plan)?;
    let mut pair = [0 as libc::c_int; 2];
    // SAFETY: plain socketpair on AF_UNIX; both fds are ours. Called before
    // the sandbox is applied, where socket syscalls are still permitted.
    // SOCK_CLOEXEC is load-bearing: without it the sidecar inherits a copy
    // of *both* ends at spawn, so the parent's end never reaches EOF when
    // the main process exits and the sidecar leaks forever (holding the
    // process's stderr pipe open with it). Rust's Command dup2s the stdio
    // fds it is given onto 0/1, which clears CLOEXEC on those targets only.
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            pair.as_mut_ptr(),
        )
    } != 0
    {
        return Err("cannot create the network socketpair".to_string());
    }
    // SAFETY: each fd is owned by this call; both are immediately wrapped
    // (UnixStream owns and closes its fd) and the child's copies are dups.
    let mut wire_stream = unsafe { UnixStream::from_raw_fd(pair[0]) };
    let guest_end = unsafe { UnixStream::from_raw_fd(pair[1]) };
    let guest_read = guest_end
        .try_clone()
        .map_err(|e| format!("cannot duplicate the network socket: {e}"))?;
    let guest_write = guest_end
        .try_clone()
        .map_err(|e| format!("cannot duplicate the network socket: {e}"))?;
    drop(guest_end);

    let exe = exe.display().to_string();
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("net-sidecar")
        // into_raw_fd consumes each stream, so the File is the sole owner
        // of the fd (from_raw_fd on a borrowed fd would double-close it).
        .stdin(std::process::Stdio::from(unsafe {
            std::fs::File::from_raw_fd(guest_read.into_raw_fd())
        }))
        .stdout(std::process::Stdio::from(unsafe {
            std::fs::File::from_raw_fd(guest_write.into_raw_fd())
        }))
        .stderr(std::process::Stdio::inherit());
    // Intentionally detached: no handle is kept. The sidecar exits when the
    // socket closes (at process exit), so there is nothing to reap here.
    cmd.spawn()
        .map_err(|e| format!("cannot spawn net-sidecar: {e}"))?;
    // The sidecar reads the policy as the first line on the wire, before
    // any request; the socket buffers it, so this cannot race the
    // child startup - and no policy file ever touches disk.
    wire_stream
        .write_all(&policy_line)
        .and_then(|_| wire_stream.flush())
        .map_err(|e| format!("network: {e}"))?;

    wire_stream
        .set_read_timeout(Some(Duration::from_millis(
            plan.limits.timeout_ms.saturating_add(10_000),
        )))
        .map_err(|e| format!("network: {e}"))?;
    let wire_fd = wire_stream.as_raw_fd();
    let _ = WIRE.set(Arc::new(Mutex::new(wire_stream)));
    Ok(NetSession { wire_fd })
}

pub(crate) fn wire() -> Result<Arc<Mutex<UnixStream>>, String> {
    WIRE.get()
        .cloned()
        .ok_or_else(|| "network sidecar is not running".to_string())
}

/// Fuzz entry point (see the `fuzz/` crate): feed attacker-shaped bytes
/// through the wire line reader and the sidecar reply parser. Not a
/// supported API.
#[doc(hidden)]
pub fn fuzz_wire(data: &[u8]) {
    let Ok((a, mut b)) = UnixStream::pair() else {
        return;
    };
    let bytes = data.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = b.write_all(&bytes);
    });
    let mut reader = BufReader::new(a);
    let _ = read_frame_capped(&mut reader, 64 * 1024);
    let _ = writer.join();
    // The sidecar's request/reply shaping is serde_json over lossy text.
    let line = String::from_utf8_lossy(data);
    let _ = serde_json::from_str::<serde_json::Value>(&line);
}

/// One completed request, delivered from the worker thread to the event
/// loop, which resolves the guest's promise with it.
pub(crate) struct NetOutcome {
    pub(crate) id: u64,
    pub(crate) result: Result<(u16, String), String>,
}

/// The worker's blocking round-trip: write the request line, read one
/// response line. Single-flight per wire (the mutex spans write and read),
/// so request and response lines can never interleave.
fn perform(
    wire: &Mutex<UnixStream>,
    id: u64,
    name: &str,
    body: &str,
    limits: &NetLimits,
) -> Result<(u16, String), String> {
    let stream = wire.lock().unwrap();
    let req = json!({"id": id, "cap": name, "body": body});
    write_all_wire(&stream, format!("{req}\n").as_bytes())?;
    // The reply line carries the response text JSON-escaped (up to 6 bytes
    // per response byte) plus id/status framing; cap it there so a broken
    // or hostile sidecar cannot balloon host memory before the size check.
    let line_cap = limits
        .max_response_bytes
        .saturating_mul(6)
        .saturating_add(FRAME_HEADROOM_BYTES);
    // The sandbox's seccomp deny list refuses every socket syscall in
    // this process, so the reply cannot be read as a UnixStream: its Read
    // maps to recvfrom(2), refused with EPERM. A File over the dup fd
    // reads with plain read(2), which the sandbox allows; the dup shares
    // the socket (and its SO_RCVTIMEO) with the wire stream.
    let read_fd =
        std::os::fd::OwnedFd::from(stream.try_clone().map_err(|e| format!("network: {e}"))?);
    let mut reader = BufReader::new(std::fs::File::from(read_fd));
    let line = match read_frame_capped(&mut reader, line_cap) {
        Ok(Some(line)) => line,
        Ok(None) | Err(FrameError::Truncated) => {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return Err("sidecar".to_string());
        }
        Err(FrameError::TooLarge) => {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return Err("overflow".to_string());
        }
        Err(FrameError::Allocation) => {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return Err("overflow".to_string());
        }
        Err(FrameError::Io(error)) => {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            if error.kind() == std::io::ErrorKind::WouldBlock
                || error.kind() == std::io::ErrorKind::TimedOut
            {
                return Err("timeout".to_string());
            }
            return Err(format!("network: {error}"));
        }
    };
    let v: serde_json::Value = match serde_json::from_slice(&line) {
        Ok(v) => v,
        Err(e) => {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return Err(format!("network: bad sidecar reply: {e}"));
        }
    };
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        return Err(err.to_string());
    }
    let status = v.get("status").and_then(|s| s.as_u64()).unwrap_or(0) as u16;
    let text = v
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    if text.len() as u64 > limits.max_response_bytes {
        let _ = stream.shutdown(std::net::Shutdown::Both);
        return Err("overflow".to_string());
    }
    Ok((status, text))
}

fn write_all_wire(stream: &UnixStream, bytes: &[u8]) -> Result<(), String> {
    let fd = stream.as_raw_fd();
    let mut offset = 0;
    while offset < bytes.len() {
        let count =
            unsafe { libc::write(fd, bytes[offset..].as_ptr().cast(), bytes.len() - offset) };
        if count > 0 {
            offset += count as usize;
            continue;
        }
        if count == 0 {
            return Err("sidecar".to_string());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(format!("network: {error}"));
    }
    Ok(())
}

/// The sidecar's authoritative payload allowance for one wire/channel grant.
/// It is intentionally local to the sidecar loop, not guest-resettable.
#[derive(Debug)]
struct SidecarBudget {
    remaining: u64,
    max_response: u64,
}

impl SidecarBudget {
    fn new(total: u64, max_response: u64) -> Self {
        Self {
            remaining: total,
            max_response,
        }
    }

    /// Admitted IPC body bytes are charged even for GET capabilities, whose
    /// HTTP request intentionally has no body.
    fn admit_request(&mut self, body: &str) -> bool {
        let bytes = body.len() as u64;
        if bytes > self.remaining {
            return false;
        }
        self.remaining -= bytes;
        true
    }

    fn response_allowance(&self) -> u64 {
        self.max_response.min(self.remaining)
    }

    fn debit_response(&mut self, bytes: u64) {
        self.remaining -= bytes;
    }

    fn exhaust(&mut self) {
        self.remaining = 0;
    }
}

#[derive(Debug)]
enum FrameError {
    TooLarge,
    Truncated,
    Allocation,
    Io(std::io::Error),
}

/// Reads one newline-terminated frame, where `cap` includes the newline.
/// EOF with a partial frame is an error. The reader consumes no bytes after
/// the first newline, and an oversized frame is rejected without draining
/// its tail so callers can fail closed instead of resuming out of sync.
fn read_frame_capped<R: std::io::BufRead>(
    reader: &mut R,
    cap: u64,
) -> Result<Option<Vec<u8>>, FrameError> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(FrameError::Io)?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(FrameError::Truncated)
            };
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            let frame_len = (frame.len() as u64).saturating_add(pos as u64 + 1);
            if frame_len > cap {
                return Err(FrameError::TooLarge);
            }
            frame
                .try_reserve_exact(pos)
                .map_err(|_| FrameError::Allocation)?;
            frame.extend_from_slice(&available[..pos]);
            reader.consume(pos + 1);
            return Ok(Some(frame));
        }
        let frame_len = (frame.len() as u64).saturating_add(available.len() as u64);
        if frame_len > cap {
            return Err(FrameError::TooLarge);
        }
        frame
            .try_reserve_exact(available.len())
            .map_err(|_| FrameError::Allocation)?;
        let consumed = available.len();
        frame.extend_from_slice(available);
        reader.consume(consumed);
    }
}

/// Outcome queue: the worker pushes, the event loop drains on the JS
/// thread. Shared between the run's world and its request threads.
pub(crate) type NetQueue = Arc<Mutex<VecDeque<NetOutcome>>>;

/// Spawns the blocking round-trip on its own thread and returns
/// immediately; the event loop drains results through [poll_net].
pub(crate) fn request_async(
    id: u64,
    name: String,
    body: String,
    limits: NetLimits,
    wire: Arc<Mutex<UnixStream>>,
    queue: NetQueue,
    wake: Arc<crate::world::LoopWake>,
) {
    std::thread::spawn(move || {
        let result = perform(&wire, id, &name, &body, &limits);
        if let Ok(mut q) = queue.lock() {
            q.push_back(NetOutcome { id, result });
        }
        // The event loop may be parked on the loop wake: the completion
        // is deliverable now, so say so instead of letting it poll.
        wake.signal();
    });
}

/// Drains completed requests and resolves their promises. True when any
/// promise was resolved, so the caller runs a microtask checkpoint (the
/// continuations may throw, exit, or schedule more work).
pub(crate) fn poll_net(scope: &mut v8::PinScope<'_, '_>) -> bool {
    let outcomes: Vec<NetOutcome> = {
        let world = world_mut(scope);
        let mut q = world.net_done.lock().unwrap();
        q.drain(..).collect()
    };
    if outcomes.is_empty() {
        return false;
    }
    for o in outcomes {
        let Some(resolver) = world_mut(scope).net_pending.remove(&o.id) else {
            continue;
        };
        // SAFETY: the resolver global belongs to this isolate.
        let r = unsafe { resolver.open(scope) };
        match o.result {
            Ok((status, text)) => {
                let world = world_mut(scope);
                world.net_bytes_used = world.net_bytes_used.saturating_add(text.len() as u64);
                // The guest API is Promise<{status, text}>: resolve with an
                // object, not a JSON string the guest would have to parse.
                let obj = v8::Object::new(scope);
                let status_key = v8::String::new(scope, "status").unwrap();
                let status_val: v8::Local<v8::Value> = v8::Number::new(scope, status as f64).into();
                obj.set(scope, status_key.into(), status_val);
                let text_key = v8::String::new(scope, "text").unwrap();
                let text_val =
                    v8::String::new_from_utf8(scope, text.as_bytes(), v8::NewStringType::Normal)
                        .unwrap();
                obj.set(scope, text_key.into(), text_val.into());
                r.resolve(scope, obj.into());
            }
            Err(kind) => {
                let e = v8::String::new(scope, &format!("network {kind}")).unwrap();
                r.reject(scope, e.into());
            }
        }
    }
    true
}

/// Maximum requests in flight per run. Each one owns a worker thread
/// blocked on the single-flight wire, so an unbounded guest loop creating
/// promises would otherwise spawn unbounded host threads.
pub(crate) const MAX_NET_IN_FLIGHT: usize = 8;

guest_cb!(net_request, |args, rv, scope| {
    let Some(_fd) = world_mut(scope).net_fd else {
        throw(scope, "network capabilities are not enabled");
        return;
    };
    if args.length() < 2 {
        throw(
            scope,
            "net.request(name, body) needs a capability name and a body",
        );
        return;
    }
    let name = crate::diag::to_rust_string(scope, &args.get(0));
    let body = crate::diag::to_rust_string(scope, &args.get(1));
    let (_names, limits, wire, queue, wake) = {
        let world = world_mut(scope);
        if world.net_bytes_used.saturating_add(body.len() as u64) > world.net_total_bytes {
            throw_range(scope, "network byte budget exceeded");
            return;
        }
        if !world.net_names.iter().any(|n| n == &name) {
            // The URL never appears in guest-visible errors.
            throw(scope, &format!("no such capability: {name}"));
            return;
        }
        if world.net_pending.len() >= MAX_NET_IN_FLIGHT {
            throw_range(scope, "too many network requests in flight");
            return;
        }
        (
            name.clone(),
            world.net_limits.clone(),
            world.net_wire.clone(),
            world.net_done.clone(),
            world.loop_wake.clone(),
        )
    };
    let Some(wire) = wire else {
        throw(scope, "network sidecar is not running");
        return;
    };
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let id = {
        let world = world_mut(scope);
        let id = world.next_net_id;
        world.next_net_id += 1;
        world.net_bytes_used = world.net_bytes_used.saturating_add(body.len() as u64);
        id
    };
    let resolver_global = v8::Global::new(scope, resolver);
    world_mut(scope).net_pending.insert(id, resolver_global);
    rv.set(resolver.get_promise(scope).into());
    request_async(id, name, body, limits, wire, queue, wake);
});

/// The net-sidecar subcommand: owns the policy, performs the HTTP. Reads
/// line-delimited requests on stdin, writes line-delimited responses on
/// stdout, exits on EOF. Returns the process exit code.
pub fn run_sidecar() -> i32 {
    // Confine this process before it touches the policy or the network:
    // it is the one component with real sockets, so it gets its own
    // Landlock/seccomp profile (see sandbox::apply_sidecar_profile).
    // Where the layers cannot exist (non-Linux, old kernel) it still
    // runs — the operator asked for network egress — but says so once.
    match crate::sandbox::apply_sidecar_profile() {
        Ok(()) => {}
        Err(e) if crate::sandbox::supported() => {
            eprintln!("net-sidecar: sandbox profile refused: {e}");
            return 2;
        }
        Err(e) => {
            eprintln!("net-sidecar: running without its sandbox profile: {e}");
        }
    }
    // Line 1 on the wire is the policy; lines 2+ are requests. Bound the
    // frame before allocating its contents or invoking the JSON parser.
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let policy_frame = match read_frame_capped(&mut reader, MAX_POLICY_FRAME_BYTES) {
        Ok(Some(frame)) => frame,
        Ok(None) | Err(_) => {
            eprintln!("net-sidecar: EOF before policy");
            return 2;
        }
    };
    // Never log the policy: it contains operator URLs, which may carry
    // credentials or tokens and must not reach logs or the guest.
    let envelope: serde_json::Value = match serde_json::from_slice(&policy_frame) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("net-sidecar: bad policy line: {e}");
            return 2;
        }
    };
    let policy = envelope.get("policy").cloned().unwrap_or(envelope.clone());
    let mut calls: HashMap<String, (String, String)> = HashMap::new();
    if let Some(map) = policy.get("calls").and_then(|c| c.as_object()) {
        for (name, spec) in map {
            let url = spec.get("url").and_then(|u| u.as_str()).unwrap_or_default();
            let method = spec
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("POST")
                .to_string();
            calls.insert(name.clone(), (url.to_string(), method));
        }
    }
    if calls.is_empty() {
        eprintln!("net-sidecar: policy grants no capabilities");
        return 2;
    }
    let max_response = policy
        .get("max_response_bytes")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_MAX_RESPONSE_BYTES);
    let timeout_ms = policy
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    let total_bytes = policy
        .get("total_bytes")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_TOTAL_BYTES);
    let allow_private = policy
        .get("allow_private")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(Duration::from_millis(timeout_ms))
        // CheckedResolver returns only the addresses ureq may connect to,
        // so the private-address check cannot be raced by DNS rebinding;
        // the per-request pre-check below only shapes the error message.
        .resolver(CheckedResolver { allow_private })
        .build();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let request_cap = request_frame_cap(total_bytes);
    // This ledger lives as long as this sidecar/channel grant. Embedders
    // sharing the wire share this budget; no guest-controlled reset exists.
    let mut budget = SidecarBudget::new(total_bytes, max_response);
    loop {
        let frame = match read_frame_capped(&mut reader, request_cap) {
            Ok(Some(frame)) => frame,
            Ok(None) => return 0,
            Err(_) => {
                eprintln!("net-sidecar: truncated or oversized request frame");
                return 2;
            }
        };
        let req: serde_json::Value = match serde_json::from_slice(&frame) {
            Ok(v) => v,
            Err(_) => {
                eprintln!("net-sidecar: malformed request frame");
                return 2;
            }
        };
        let Some(id) = req.get("id").and_then(|id| id.as_u64()) else {
            eprintln!("net-sidecar: malformed request frame");
            return 2;
        };
        let Some(cap) = req.get("cap").and_then(|c| c.as_str()) else {
            eprintln!("net-sidecar: malformed request frame");
            return 2;
        };
        let Some(body) = req.get("body").and_then(|b| b.as_str()) else {
            eprintln!("net-sidecar: malformed request frame");
            return 2;
        };
        let id = json!(id);
        let Some((url, method)) = calls.get(cap) else {
            write_json(
                &mut out,
                &json!({"id": id, "error": "policy: unknown capability"}),
            );
            continue;
        };
        if !budget.admit_request(body) {
            write_json(
                &mut out,
                &json!({"id": id, "error": "policy: total byte budget exceeded"}),
            );
            continue;
        }
        if !allow_private {
            match url_resolves_private(url) {
                Ok(true) => {
                    write_json(
                        &mut out,
                        &json!({"id": id, "error": "policy: endpoint resolves to a private address"}),
                    );
                    continue;
                }
                Err(e) => {
                    write_json(&mut out, &json!({"id": id, "error": e}));
                    continue;
                }
                Ok(false) => {}
            }
        }
        let request = agent
            .request(method.as_str(), url)
            .set("Content-Type", "application/json");
        let response = if method == "GET" {
            request.call()
        } else {
            request
                .set("Content-Type", "application/json")
                .send_string(body)
        };
        match response {
            Ok(resp) => reply_with_body(&mut out, id, resp, &mut budget),
            Err(ureq::Error::Status(_code, resp)) => {
                reply_with_body(&mut out, id, resp, &mut budget)
            }
            Err(e) => {
                let kind = match &e {
                    ureq::Error::Transport(t) if t.kind() == ureq::ErrorKind::Dns => "dns",
                    _ => "network",
                };
                write_json(&mut out, &json!({"id": id, "error": kind}));
            }
        }
    }
}

/// Sends one HTTP response as a sidecar reply. Non-2xx statuses are
/// responses, not errors: the guest inspects the status itself.
fn reply_with_body<W: Write>(
    out: &mut W,
    id: serde_json::Value,
    resp: ureq::Response,
    budget: &mut SidecarBudget,
) {
    let status = resp.status();
    let allowance = budget.response_allowance();
    let mut text = String::new();
    let read_failed = resp
        .into_reader()
        .take(allowance.saturating_add(1))
        .read_to_string(&mut text)
        .is_err();
    if read_failed || text.len() as u64 > allowance {
        budget.exhaust();
        write_json(
            out,
            &json!({"id": id, "error": "overflow: response exceeds the response cap"}),
        );
        return;
    }
    budget.debit_response(text.len() as u64);
    write_json(out, &json!({"id": id, "status": status, "text": text}));
}

fn write_json<W: Write>(out: &mut W, value: &serde_json::Value) {
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn plan_with_policy_frame_size(size: usize) -> NetPlan {
        let mut plan = NetPlan {
            calls: vec![NetCall {
                name: "cap".to_string(),
                url: String::new(),
                method: "POST".to_string(),
            }],
            limits: NetLimits::default(),
        };
        let base_size = format!("{{\"policy\": {}}}\n", plan.policy_json()).len();
        assert!(size >= base_size);
        plan.calls[0].url = "x".repeat(size - base_size);
        assert_eq!(
            format!("{{\"policy\": {}}}\n", plan.policy_json()).len(),
            size
        );
        plan
    }

    #[test]
    fn policy_frame_is_bounded_before_sidecar_spawn() {
        let exact = plan_with_policy_frame_size(MAX_POLICY_FRAME_BYTES as usize);
        let exact_frame = policy_frame(&exact).unwrap();
        assert_eq!(exact_frame.len() as u64, MAX_POLICY_FRAME_BYTES);
        let missing_exe = std::path::Path::new("/definitely/missing/tardigrade");
        let err = spawn_sidecar(missing_exe, &exact).err().unwrap();
        assert!(
            err.contains("cannot spawn net-sidecar"),
            "exact-boundary policy should pass the frame check: {err}"
        );

        let oversized = plan_with_policy_frame_size(MAX_POLICY_FRAME_BYTES as usize + 1);
        let err = spawn_sidecar(missing_exe, &oversized).err().unwrap();
        assert_eq!(err, "network policy exceeds the 1 MiB frame limit");
    }

    #[test]
    fn policy_mirrors_total_byte_budget_and_request_cap_saturates() {
        let mut plan = NetPlan::default();
        plan.limits.total_bytes = 123_456;
        let policy: serde_json::Value = serde_json::from_str(&plan.policy_json()).unwrap();
        assert_eq!(policy["total_bytes"], 123_456);

        assert_eq!(
            request_frame_cap(123_456),
            6 * 123_456 + MAX_POLICY_FRAME_BYTES + FRAME_HEADROOM_BYTES
        );
        assert_eq!(
            request_frame_cap(u64::MAX),
            u64::MAX,
            "large budgets must saturate rather than wrap"
        );
    }

    #[test]
    fn sidecar_budget_counts_utf8_and_preserves_credit_on_refusal() {
        let mut budget = SidecarBudget::new(3, 2);
        assert!(!budget.admit_request("éé"));
        assert_eq!(budget.remaining, 3);
        assert!(budget.admit_request("é"));
        assert_eq!(budget.remaining, 1);
        assert_eq!(budget.response_allowance(), 1);
        budget.debit_response(1);
        assert_eq!(budget.remaining, 0);
        assert!(budget.admit_request(""));
        assert!(!budget.admit_request("x"));

        let mut zero = SidecarBudget::new(0, 0);
        assert!(zero.admit_request(""));
        assert_eq!(zero.response_allowance(), 0);
        assert!(!zero.admit_request("x"));
    }

    #[test]
    fn bounded_frame_reader_checks_delimiter_inclusive_boundary_and_eof() {
        let mut reader = BufReader::new(std::io::Cursor::new(b"abc\nTAIL"));
        assert_eq!(read_frame_capped(&mut reader, 4).unwrap().unwrap(), b"abc");
        assert!(matches!(
            read_frame_capped(&mut reader, 4),
            Err(FrameError::Truncated)
        ));

        let mut reader = BufReader::new(std::io::Cursor::new(b"abc\n"));
        assert!(matches!(
            read_frame_capped(&mut reader, 3),
            Err(FrameError::TooLarge)
        ));

        let mut reader = BufReader::new(std::io::Cursor::new(b"abc"));
        assert!(matches!(
            read_frame_capped(&mut reader, 4),
            Err(FrameError::Truncated)
        ));
        let mut reader = BufReader::new(std::io::Cursor::new(b""));
        assert!(read_frame_capped(&mut reader, 4).unwrap().is_none());
    }

    #[test]
    fn oversized_sidecar_reply_poisoning_prevents_parsing_its_tail() {
        #[derive(Debug)]
        enum PeerAfterReply {
            Eof,
            Request,
            TimedOut,
            ReadError,
        }

        let (client, mut server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        server
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let wire = Mutex::new(client);
        let server_thread = std::thread::spawn(move || {
            let mut reader = BufReader::new(server.try_clone().unwrap());
            let request = read_frame_capped(&mut reader, 1024).unwrap().unwrap();
            let request: serde_json::Value = serde_json::from_slice(&request).unwrap();
            assert_eq!(request["id"], 1);

            let too_large = json!({"id": 1, "status": 200, "text": "x".repeat(5000)});
            // This tail is within the one-byte response cap: if the client
            // sends another request it can parse a successful response.
            let tail = json!({"id": 2, "status": 200, "text": "t"});
            server
                .write_all(format!("{too_large}\n").as_bytes())
                .unwrap();
            match read_frame_capped(&mut reader, 1024) {
                Ok(None) => PeerAfterReply::Eof,
                Ok(Some(_request)) => {
                    server.write_all(format!("{tail}\n").as_bytes()).unwrap();
                    PeerAfterReply::Request
                }
                Err(FrameError::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    PeerAfterReply::TimedOut
                }
                Err(_error) => PeerAfterReply::ReadError,
            }
        });
        let limits = NetLimits {
            max_response_bytes: 1,
            ..NetLimits::default()
        };
        assert_eq!(
            perform(&wire, 1, "cap", "", &limits),
            Err("overflow".to_string())
        );
        let later_result = perform(&wire, 2, "cap", "", &limits);
        let peer = server_thread.join().unwrap();
        assert!(
            matches!(peer, PeerAfterReply::Eof),
            "server must observe peer EOF without another request or timeout: {peer:?}"
        );
        assert!(
            later_result.is_err(),
            "a later request must not parse a tail after a bad frame"
        );
    }

    #[test]
    fn private_ranges_include_cgnat_and_mapped_v6() {
        for s in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.254",
            "198.18.0.1",
            "224.0.0.1",
            "0.0.0.0",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:8.8.8.8",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
        ] {
            // ::ffff:8.8.8.8 is a mapped *public* address: not private.
            if s == "::ffff:8.8.8.8" {
                assert!(!addr_is_private(ip(s)), "{s} must stay public");
            } else {
                assert!(addr_is_private(ip(s)), "{s} must be private");
            }
        }
        for s in [
            "8.8.8.8",
            "1.1.1.1",
            "100.63.0.1",
            "100.128.0.1",
            "2606:4700:4700::1111",
        ] {
            assert!(!addr_is_private(ip(s)), "{s} must stay public");
        }
    }

    #[test]
    fn parse_call_accepts_only_named_http_endpoints() {
        let c = parse_call("ingest=https://api.example.com/x").unwrap();
        assert_eq!((c.name.as_str(), c.method.as_str()), ("ingest", "POST"));
        assert_eq!(parse_call("a=https://h/x,get").unwrap().method, "GET");
        assert!(parse_call("a=ftp://h/x").is_err());
        assert!(parse_call("a b=https://h/x").is_err());
        assert!(parse_call("a=https://h/x,DELETE").is_err());
        assert!(parse_call("noequals").is_err());
    }
}
