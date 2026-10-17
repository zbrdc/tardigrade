//! tardigrade: run small JavaScript applications entirely in RAM on the Chromium
//! V8 engine, embedded through the v8 crate.
//!
//! Library surface: [run] executes one guest program and returns its exit
//! code; [serve] runs the console protocol over any reader/writer pair.
//! V8 is initialized once on a dedicated host thread that also begets every
//! guest thread (a pkey requirement of the default platform), so callers may
//! live on any thread. Each run owns its isolate and its world state, so
//! guests can run concurrently in one process without seeing each other.
//! serve commands and vm.stdin share one process-wide stdin hub (see the
//! stdin module); serve replies rendezvous with the caller's writer so
//! output ordering matches a single-threaded embedder. Disk is touched only
//! at the explicit host boundary (entry source, preload/dump files). Guests
//! never make a syscall of their own.

mod api;
pub mod bridge_http;
pub mod bundle;

/// Compile-time build identity: set via TARDIGRADE_BUILD_ID during a
/// reproducible build (FU-19). None means the identity is unknown — the
/// binary was built without an injected identity, and callers must not
/// treat two such binaries as the same build.
pub const BUILD_ID: Option<&str> = option_env!("TARDIGRADE_BUILD_ID");

mod clock;
mod console;
mod diag;
mod dump;
mod event_loop;
mod exit;
mod fs;
mod fs_api;
mod heap;
mod host;
mod module;
mod net;
pub mod netcap;
mod serve;
mod snapshot;
mod stdin;
mod timers;
mod watchdog;
mod webapi;
mod world;

pub mod sandbox;

/// Fuzz entry points (cargo-fuzz targets in `fuzz/`). These only drive
/// the same parsers a guest or operator can reach — snapshot restore,
/// RAM-filesystem paths — with attacker-shaped bytes; they exist so the
/// fuzz crate can name them, and are not a supported API.
#[doc(hidden)]
pub mod fuzz_hooks {
    /// Parse and restore a snapshot, then re-encode what landed: the
    /// restore path a `--preload` file or `vm.restore` call takes.
    pub fn snapshot_restore(data: &[u8]) {
        let Ok(text) = std::str::from_utf8(data) else {
            return;
        };
        let mut fs = crate::fs::RamFs::new(crate::DEFAULT_FS_QUOTA_BYTES);
        let _ = fs.restore(text);
        let _ = crate::snapshot::encode(&fs.files, &fs.dirs);
    }

    /// Drive the RAM filesystem with NUL-separated path-shaped chunks:
    /// normalization, quota accounting and the mutating operations must
    /// only ever return values or errors, never panic.
    pub fn fs_paths(data: &[u8]) {
        let mut fs = crate::fs::RamFs::new(crate::DEFAULT_FS_QUOTA_BYTES);
        let chunks: Vec<String> = data
            .split(|b| *b == 0)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect();
        for (i, path) in chunks.iter().enumerate() {
            match i % 5 {
                0 => {
                    let _ = fs.write(path, vec![b'x'; i % 64]);
                }
                1 => {
                    let _ = fs.mkdir(path, true);
                }
                2 => {
                    let _ = fs.read_string(path);
                    let _ = fs.stat(path);
                }
                3 => {
                    if let Some(next) = chunks.get(i + 1) {
                        let _ = fs.rename(path, next);
                    }
                }
                _ => {
                    let _ = fs.remove(path, true);
                }
            }
        }
        let _ = fs.list("/");
    }
}

pub use fs::DEFAULT_FS_QUOTA_BYTES;
pub use netcap::{NetConfig, NetLimits, NetPlan, NetSession};

use std::io::Write;
use std::sync::{mpsc, Arc};

use api::install;
use clock::RunClock;
use diag::report;
use event_loop::{event_loop, promise_reject_callback, stopped};
use watchdog::Watchdog;
use world::{world_mut, World};

/// The run's clock: real time, or the virtual clock of --clock=virtual
/// (see docs/OPTIONS.md, --clock=virtual, for its exact semantics).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Clock {
    /// Real time: Date reads V8's own clock and performance.now() a
    /// monotonic Instant (the default).
    Real,
    /// Virtual time: Date.now(), new Date() (no argument) and
    /// performance.now() read a clock that starts at `epoch_ms` for Date
    /// and at 0 for performance.now(), and advances only when the event
    /// loop jumps it to the next due timer. Epochs beyond
    /// [Clock::MAX_EPOCH_MS] are the CLI's to refuse; the Date timeline
    /// itself clamps there.
    Virtual { epoch_ms: u64 },
}

impl Clock {
    /// True for the virtual clock.
    pub fn is_virtual(&self) -> bool {
        matches!(self, Clock::Virtual { .. })
    }

    /// The largest epoch --epoch accepts and Date.now() reports under the
    /// virtual clock: the ECMAScript maximum time value (ES2023 21.4.2,
    /// 8.64e15 ms). A virtual Date timeline saturates here instead of
    /// leaving the Date range.
    pub const MAX_EPOCH_MS: u64 = 8_640_000_000_000_000;
}

/// Host-side run configuration.
#[derive(Clone, Debug)]
pub struct Config {
    /// Wall-clock cap in ms; 0 disables. Default 60000.
    pub timeout_ms: u64,
    /// V8 heap cap in MB. Values below 8 are clamped up to 8 (V8 cannot
    /// sustain a smaller heap). Default 256.
    pub heap_mb: usize,
    /// Cap on total guest console output (stdout plus stderr) in bytes;
    /// past it, console calls throw a catchable RangeError and write
    /// nothing. 0 disables. Default 64 MiB.
    pub max_output_bytes: u64,
    /// Parse the entry as an ES module (import/export, top-level await;
    /// specifiers resolve in the RAM filesystem) instead of a classic
    /// script. Default false. The CLI also enables it for `.mjs` entries.
    pub module: bool,
    /// Process RSS cap in MB (Linux only): the watchdog samples
    /// /proc/self/status and stops the run as a heap-cap hit (exit 137)
    /// past it. It bounds memory no in-process allocator can see —
    /// module-internal WebAssembly memories — but counts the *whole
    /// process*, so it fits one-guest-per-process use (the CLI), not
    /// concurrent guests in one embedder. 0 disables. Default 0.
    pub rss_cap_mb: usize,
    /// Arguments exposed to the guest as vm.args.
    pub args: Vec<String>,
    /// Seed vm.fs from a snapshot() JSON file before the guest runs.
    pub preload: Option<std::path::PathBuf>,
    /// Write snapshot() JSON to this file when the run ends. The file is
    /// replaced atomically (temp file in the same directory, fsync, rename)
    /// unless it is not a regular file or [`sandbox::apply`] confined the
    /// process, and then it is rewritten in place.
    pub dump: Option<std::path::PathBuf>,
    /// True when the entry source was read from stdin, so guest stdin is
    /// already at EOF.
    pub stdin_consumed: bool,
    /// Snapshot JSON to seed vm.fs from, for hosts that read the preload file
    /// before applying [`sandbox::apply`] (which then denies re-opening it).
    /// Used instead of reading `preload`, and works without it.
    pub preload_data: Option<String>,
    /// RAM filesystem quota in bytes: per entry, content plus path bytes
    /// plus 64 of bookkeeping. 0 disables it. Default 64 MiB.
    pub fs_quota_bytes: u64,
    /// Deterministic Math.random seed for agent loops: two runs with the
    /// same seed draw identical sequences, different seeds differ. None
    /// (the default) keeps V8's own Math.random.
    pub seed: Option<u64>,
    /// Per-command wall-clock cap in ms for serve mode; 0 disables.
    /// Default 0. On expiry the command's reply is
    /// `{"$error":"command timed out"}`, V8's pending termination is
    /// cancelled and the session continues with its state intact - unlike
    /// `timeout_ms`, which ends the run.
    pub command_timeout_ms: u64,
    /// The guest's clock. Real (the default) keeps real time everywhere;
    /// Virtual makes Date, performance.now and the timer timeline virtual
    /// so runs are reproducible and timer-heavy guests finish instantly.
    pub clock: Clock,
    /// Scoped network capabilities (vm.net.request); None (the default)
    /// keeps the guest fully offline. See netcap and
    /// docs/NETWORK_CAPABILITIES.md.
    pub net: Option<netcap::NetConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            timeout_ms: 60000,
            heap_mb: 256,
            max_output_bytes: 64 * 1024 * 1024,
            module: false,
            rss_cap_mb: 0,
            args: Vec::new(),
            preload: None,
            dump: None,
            stdin_consumed: false,
            preload_data: None,
            fs_quota_bytes: DEFAULT_FS_QUOTA_BYTES,
            seed: None,
            command_timeout_ms: 0,
            clock: Clock::Real,
            net: None,
        }
    }
}

impl Config {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Declares a guest callback in the crate's callback style: the body gets
/// `args`, `rv`, and `scope` (the callback scope, usable as a HandleScope).
/// The per-run World is reached with `world_mut(scope)` at each use; a
/// reference into the isolate slot borrows the scope, so it cannot be held
/// across other uses of the scope the way the old process global could.
macro_rules! guest_cb {
    ($name:ident, |$args:ident, $rv:ident, $scope:ident| $body:block) => {
        pub(crate) unsafe extern "C" fn $name(info: *const v8::FunctionCallbackInfo) {
            let info = unsafe { &*info };
            // Same bootstrap rusty_v8's own callback trampoline uses: the
            // storage is pinned on the stack, init() hands back the pinned
            // HandleScope every V8 API call now takes (v8::PinScope).
            let storage = std::pin::pin!(unsafe { v8::CallbackScope::new(info) });
            let mut $scope = storage.init();
            let $scope = &mut $scope;
            // Once the run is leaving (vm.exit, timeout, heap cap), guest
            // callbacks return without side effects: the outcome is decided
            // and nothing after that may print, write, or schedule. V8 only
            // observes the termination at its next stack check, so blocked
            // callbacks also re-request it - otherwise the guest keeps
            // running JS on undefined results until it throws.
            if crate::world::world_mut($scope).is_leaving() {
                $scope.as_mut().terminate_execution();
                return;
            }
            let $args = v8::FunctionCallbackArguments::from_function_callback_info(info);
            let mut $rv = v8::ReturnValue::from_function_callback_info(info);
            $body
        }
    };
}
pub(crate) use guest_cb;

/// Guest-side glue evaluated before the entry source.
const PRELUDE: &str = include_str!("prelude.js");

pub(crate) fn eval_src<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    src: &str,
    name: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    let src = v8::String::new_from_utf8(scope, src.as_bytes(), v8::NewStringType::Normal)?;
    let name = v8::String::new(scope, name).unwrap();
    let origin = v8::ScriptOrigin::new(
        scope,
        name.into(),
        0,
        0,
        false,
        0,
        None,
        false,
        false,
        false,
        None,
    );
    let script = v8::Script::compile(scope, src, Some(&origin))?;
    script.run(scope)
}

fn read_path(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// One guest run request: entry source plus host configuration.
struct RunSpec<'a> {
    source: &'a str,
    entry: &'a str,
    cfg: &'a Config,
}

fn eval_prelude(scope: &mut v8::PinScope<'_, '_>) -> bool {
    v8::tc_scope!(let tc, scope);
    if eval_src(tc, PRELUDE, "prelude").is_none() {
        report(tc);
        return false;
    }
    true
}

/// The preload snapshot text and a name for it in messages; Ok(None) when
/// no preload was requested, Err(code) for a run that must not start.
fn preload_text(cfg: &Config) -> Result<Option<(String, String)>, i32> {
    // preload_data is the file the host already read before it applied the
    // sandbox (the sandbox denies re-opening the path), so it wins over, and
    // works without, the path. Both feed the same restore path in
    // apply_preload, so sandboxed and plain runs accept and refuse exactly
    // the same snapshots.
    let name = match &cfg.preload {
        Some(path) => format!("{:?}", path),
        None => "data".to_string(),
    };
    if let Some(data) = &cfg.preload_data {
        return Ok(Some((data.clone(), name)));
    }
    let Some(path) = &cfg.preload else {
        return Ok(None);
    };
    match read_path(path) {
        Some(state) => Ok(Some((state, name))),
        None => {
            eprintln!("tardigrade: cannot open preload {}", name);
            Err(2)
        }
    }
}

/// Seeds vm.fs from a snapshot dump; Some(code) aborts the run.
fn apply_preload(scope: &mut v8::PinScope<'_, '_>, cfg: &Config) -> Option<i32> {
    let (state, name) = match preload_text(cfg) {
        Ok(Some(found)) => found,
        Ok(None) => return None,
        Err(code) => return Some(code),
    };
    let key = v8::String::new(scope, "__PRELOAD__").unwrap();
    let val = match v8::String::new_from_utf8(scope, state.as_bytes(), v8::NewStringType::Normal) {
        Some(v) => v,
        // Past V8's maximum string length: refuse the run, do not panic.
        None => {
            eprintln!("tardigrade: preload {} is too large to load", name);
            return Some(2);
        }
    };
    let global = scope.get_current_context().global(scope);
    global.set(scope, key.into(), val.into());
    v8::tc_scope!(let tc, scope);
    if eval_src(
        tc,
        "vm.restore(__PRELOAD__); globalThis.__PRELOAD__ = undefined;",
        "preload",
    )
    .is_none()
    {
        report(tc);
        return Some(2);
    }
    None
}

fn eval_entry(scope: &mut v8::PinScope<'_, '_>, spec: &RunSpec) -> i32 {
    if spec.source.is_empty() {
        return 0;
    }
    if spec.cfg.module {
        return module::eval_entry_module(scope, spec.source, spec.entry);
    }
    // The TryCatch borrows the scope, so it lives in its own block: the
    // success path below reuses the outer scope once the block ends.
    let exception_code = {
        v8::tc_scope!(let tc, scope);
        let result = eval_src(tc, spec.source, spec.entry);
        if tc.as_mut().is_execution_terminating() {
            tc.as_mut().cancel_terminate_execution();
            return world_mut(tc).take_terminated_exit();
        }
        match result {
            Some(_) => None,
            None => {
                let decides = world_mut(tc).exception_decides();
                let code = world_mut(tc).exception_exit();
                if decides {
                    report(tc);
                }
                Some(code)
            }
        }
    };
    if let Some(code) = exception_code {
        return code;
    }
    // A vm.exit inside the script can be swallowed along with its
    // termination; the guest's exit code still wins over the
    // checkpoint decision (Node: process.exit beats a pending
    // rejection).
    let exit = world_mut(scope).exit_code();
    if world_mut(scope).has_exit() {
        return exit;
    }
    // End of the entry tick: V8 has drained the script's
    // microtasks, so the checkpoint decision applies now, before
    // any timer or network turn can rescue a rejection.
    stopped(scope)
        .or_else(|| event_loop::check_unhandled(scope))
        .unwrap_or(0)
}

/// Writes the dump straight from the run's RamFs: serialization is pure
/// Rust over the embedder slot, so no V8 call, no JS, and nothing that
/// could depend on how the run ended (normal exit, vm.exit, uncaught
/// exception, timeout). False means the dump could not be produced
/// (serialize failure or the file could not be written).
fn write_dump(scope: &mut v8::PinScope<'_, '_>, dump: &std::path::Path) -> bool {
    let fs = &world_mut(scope).fs;
    let json = match snapshot::encode(&fs.files, &fs.dirs) {
        Ok(json) => json,
        Err(e) => {
            eprintln!("tardigrade: snapshot failed; dump not written: {}", e);
            return false;
        }
    };
    match dump::write(dump, &json) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("tardigrade: cannot write dump {}: {}", dump.display(), e);
            false
        }
    }
}

/// Shared body of run() and serve(): returns the guest exit code. Early
/// returns are safe here; the caller owns teardown ordering.
fn execute_inner(
    scope: &mut v8::PinScope<'_, '_>,
    handle: &v8::IsolateHandle,
    spec: &RunSpec,
    replies: Option<&serve::ReplySink>,
) -> i32 {
    install(scope, spec.cfg);
    if !eval_prelude(scope) {
        return 1;
    }
    if let Some(code) = apply_preload(scope, spec.cfg) {
        return code;
    }

    let watchdog = match Watchdog::new(
        handle.clone(),
        spec.cfg.timeout_ms,
        world_mut(scope).term.clone(),
        (spec.cfg.rss_cap_mb as u64).saturating_mul(1024 * 1024),
    ) {
        Ok(watchdog) => watchdog,
        Err(_) => {
            eprintln!("tardigrade: cannot start run watchdog");
            return 70;
        }
    };

    let mut code = eval_entry(scope, spec);
    // vm.exit(0) returns 0 from eval_entry, which is also "keep going": only
    // enter the loop when the guest has not already decided to exit.
    if code == 0 && !world_mut(scope).has_exit() {
        code = match replies {
            Some(replies) => serve::serve_loop(scope, handle, replies, spec.cfg.command_timeout_ms),
            None => event_loop(scope),
        };
    }
    drop(watchdog);

    if let Some(dump) = &spec.cfg.dump {
        // EX_IOERR: the run was asked to produce a dump and could not.
        // A guest that already failed keeps its own exit code.
        if !write_dump(scope, dump) && code == 0 {
            code = 74;
        }
    }
    code
}

/// Owns the isolate plus its heap guard with the drop order that keeps
/// the guard (the V8 callbacks' data pointer) alive through the isolate's
/// teardown GC, even on a panic unwind: struct fields drop in declaration
/// order, so the isolate goes first.
struct Guest {
    isolate: v8::OwnedIsolate,
    /// Kept for drop order: the guard (the callback's data pointer) must
    /// outlive the isolate through its teardown GC. Never read.
    _guard: Box<heap::HeapGuard>,
}

impl Guest {
    fn new(cfg: &Config, mut world: World) -> (Guest, v8::IsolateHandle) {
        // --memory caps the JS heap and ArrayBuffer memory separately,
        // each at N. V8's own limit sits above the cap with headroom: the
        // near-heap-limit callback fires only after V8's collections
        // failed under that limit, and the headroom gives the raise room
        // to unwind into (no headroom left several growth shapes aborting
        // inside V8's allocation retry, which ignores termination). The
        // heap starts small: a large explicit initial size makes this V8
        // saturate the old generation far below the maximum.
        let plan = heap::plan(cfg.heap_mb);
        world.heap_mb = plan.cap_mb;
        let term = world.term.clone();
        world.ab_budget = Arc::new(heap::AllocState::new(plan.cap_bytes));
        let params = v8::CreateParams::default()
            .heap_limits(0, plan.v8_max_bytes)
            .array_buffer_allocator(heap::counting_allocator(&world.ab_budget));

        let mut isolate = v8::Isolate::new(params);
        let handle = isolate.thread_safe_handle();
        let mut guard = Box::new(heap::HeapGuard::new(
            handle.clone(),
            term.clone(),
            &plan,
            world.ab_budget.clone(),
        ));
        guard.register(&mut isolate);
        // The isolate owns the world through its embedder slot. On isolate
        // teardown the annex nulls its isolate pointer before the slots
        // are dropped, so the slot's Global handles see a disposed isolate
        // and skip resetting themselves (v8 handle.rs) - no manual
        // clearing needed.
        isolate.set_slot(world);
        isolate.set_promise_reject_callback(promise_reject_callback);
        isolate.set_host_import_module_dynamically_callback(module::import_dynamically);
        (
            Guest {
                isolate,
                _guard: guard,
            },
            handle,
        )
    }
}

fn execute(source: &str, entry: &str, cfg: &Config, replies: Option<&serve::ReplySink>) -> i32 {
    let clock = match cfg.clock {
        Clock::Real => RunClock::Real(std::time::Instant::now()),
        Clock::Virtual { epoch_ms } => RunClock::Virtual {
            epoch_ms,
            now_ms: 0,
        },
    };
    let mut world = World::new(cfg.fs_quota_bytes, clock);
    // serve mode: stdin is the command channel, so guest stdin is at EOF.
    world.stdin_eof = cfg.stdin_consumed || replies.is_some();
    world.serve_mode = replies.is_some();
    world.output_cap = if cfg.max_output_bytes == 0 {
        u64::MAX
    } else {
        cfg.max_output_bytes
    };
    // Seeded runs keep the PRNG counter in the World, so concurrent runs
    // with different seeds stay independent.
    world.random_state = cfg.seed;
    if let Some(net) = &cfg.net {
        world.net_limits = crate::netcap::NetLimits {
            max_response_bytes: net.max_response_bytes,
            total_bytes: net.total_bytes,
            timeout_ms: net.timeout_ms,
            allow_private: net.allow_private,
        };
        world.net_names = net.names.clone();
        world.net_max_response = net.max_response_bytes;
        world.net_total_bytes = net.total_bytes;
        world.net_fd = Some(net.wire_fd);
        world.net_wire = crate::netcap::wire().ok();
    }
    let term = world.term.clone();

    let spec = RunSpec { source, entry, cfg };
    let (mut guest, handle) = Guest::new(cfg, world);
    let code = {
        v8::scope!(let hs, &mut guest.isolate);
        let context = v8::Context::new(hs, Default::default());
        let scope = &mut v8::ContextScope::new(hs, context);
        execute_inner(scope, &handle, &spec, replies)
    };
    // Normally Watchdog::drop already finished the run; the early returns
    // (prelude or preload failure) get here without one. Either way heap
    // callbacks for the teardown GC stay inert, and the guard drops after
    // the isolate.
    term.finish();
    code
}

/// Run one guest program and return its exit code.
pub fn run(source: &str, entry: &str, cfg: &Config) -> i32 {
    let (done, rx) = mpsc::channel();
    host::submit(host::Job {
        source: source.to_string(),
        entry: entry.to_string(),
        cfg: cfg.clone(),
        replies: None,
        done,
    });
    finish(rx.recv().expect("tardigrade guest finished"))
}

/// Resumes a guest panic on the calling thread, preserving its payload.
fn finish(outcome: Result<i32, Box<dyn std::any::Any + Send>>) -> i32 {
    match outcome {
        Ok(code) => code,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// Serve the console protocol on the given writer (reads commands from
/// process stdin through the stdin hub). One JS expression per line in, one
/// JSON line out. The guest runs on the host thread's descendants; W never
/// leaves the calling thread.
pub fn serve<W: Write>(mut output: W, source: &str, entry: &str, cfg: &Config) -> i32 {
    // Zero-capacity channel plus an ack per reply: the guest cannot run past
    // a reply until the caller has written and flushed it, keeping stdout
    // ordering identical to a single-threaded embedder.
    let (replies, reply_rx) = mpsc::sync_channel::<serve::Reply>(0);
    let (done, rx) = mpsc::channel();
    host::submit(host::Job {
        source: source.to_string(),
        entry: entry.to_string(),
        cfg: cfg.clone(),
        replies: Some(replies),
        done,
    });
    for (line, ack) in reply_rx {
        let _ = output.write_all(line.as_bytes());
        let _ = output.flush();
        let _ = ack.send(());
    }
    finish(rx.recv().expect("tardigrade guest finished"))
}

#[cfg(test)]
mod watchdog_failure_tests {
    use super::*;
    use std::sync::Mutex;

    static RUNTIME_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn run_watchdog_spawn_failure_fails_before_entry_evaluation() {
        let _lock = RUNTIME_TEST_LOCK.lock().unwrap();
        watchdog::fail_next_run_spawn_for_test();
        let cfg = Config::default();
        assert_eq!(run("vm.exit(42);", "watchdog-failure.js", &cfg), 70);
    }

    #[test]
    fn command_watchdog_spawn_failure_fails_before_command_evaluation() {
        let _lock = RUNTIME_TEST_LOCK.lock().unwrap();
        watchdog::fail_next_command_spawn_for_test();
        stdin::push_front(b"vm.exit(42)\n".to_vec());
        let cfg = Config {
            command_timeout_ms: 1_000,
            ..Config::default()
        };

        let mut output = Vec::new();
        assert_eq!(serve(&mut output, "", "watchdog-failure.js", &cfg), 70);
        assert!(output.is_empty(), "failed command must not get a reply");
    }
}
