//! v8vm: run small JavaScript applications entirely in RAM on the Chromium
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
mod net;
mod serve;
mod snapshot;
mod stdin;
mod timers;
mod watchdog;
mod webapi;
mod world;

pub mod sandbox;

pub use fs::DEFAULT_FS_QUOTA_BYTES;

use std::io::Write;
use std::sync::{mpsc, Arc};

use api::install;
use clock::RunClock;
use diag::report;
use event_loop::{event_loop, promise_reject_callback, stopped};
use watchdog::Watchdog;
use world::{world_mut, World};

/// The run's clock: real time, or the virtual clock of --clock=virtual
/// (see the README's Use section for its exact semantics).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Clock {
    /// Real time: Date reads V8's own clock and performance.now() a
    /// monotonic Instant (the default).
    Real,
    /// Virtual time: Date.now(), new Date() (no argument) and
    /// performance.now() read a clock that starts at `epoch_ms` for Date
    /// and at 0 for performance.now(), and advances only when the event
    /// loop jumps it to the next due timer.
    Virtual { epoch_ms: u64 },
}

impl Clock {
    /// True for the virtual clock.
    pub fn is_virtual(&self) -> bool {
        matches!(self, Clock::Virtual { .. })
    }
}

/// Host-side run configuration.
#[derive(Clone, Debug)]
pub struct Config {
    /// Wall-clock cap in ms; 0 disables. Default 60000.
    pub timeout_ms: u64,
    /// V8 heap cap in MB. Values below 8 are clamped up to 8 (V8 cannot
    /// sustain a smaller heap). Default 256.
    pub heap_mb: usize,
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
    /// The guest's clock. Real (the default) keeps real time everywhere;
    /// Virtual makes Date, performance.now and the timer timeline virtual
    /// so runs are reproducible and timer-heavy guests finish instantly.
    pub clock: Clock,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            timeout_ms: 60000,
            heap_mb: 256,
            args: Vec::new(),
            preload: None,
            dump: None,
            stdin_consumed: false,
            preload_data: None,
            fs_quota_bytes: DEFAULT_FS_QUOTA_BYTES,
            seed: None,
            clock: Clock::Real,
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
        pub(crate) extern "C" fn $name(info: *const v8::FunctionCallbackInfo) {
            let info = unsafe { &*info };
            let $scope = unsafe { &mut v8::CallbackScope::new(info) };
            // Once the run is leaving (vm.exit, timeout, heap cap), guest
            // callbacks return without side effects: the outcome is decided
            // and nothing after that may print, write, or schedule. V8 only
            // observes the termination at its next stack check, so blocked
            // callbacks also re-request it - otherwise the guest keeps
            // running JS on undefined results until it throws.
            if crate::world::world_mut($scope).is_leaving() {
                $scope.terminate_execution();
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
    scope: &mut v8::HandleScope<'s>,
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

fn eval_prelude(scope: &mut v8::HandleScope) -> bool {
    let mut tc = v8::TryCatch::new(scope);
    if eval_src(&mut tc, PRELUDE, "prelude").is_none() {
        report(&mut tc);
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
            eprintln!("v8vm: cannot open preload {}", name);
            Err(2)
        }
    }
}

/// Seeds vm.fs from a snapshot dump; Some(code) aborts the run.
fn apply_preload(scope: &mut v8::HandleScope, cfg: &Config) -> Option<i32> {
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
            eprintln!("v8vm: preload {} is too large to load", name);
            return Some(2);
        }
    };
    let global = scope.get_current_context().global(scope);
    global.set(scope, key.into(), val.into());
    let mut tc = v8::TryCatch::new(scope);
    if eval_src(
        &mut tc,
        "vm.restore(__PRELOAD__); globalThis.__PRELOAD__ = undefined;",
        "preload",
    )
    .is_none()
    {
        report(&mut tc);
        return Some(2);
    }
    None
}

fn eval_entry(scope: &mut v8::HandleScope, spec: &RunSpec) -> i32 {
    if spec.source.is_empty() {
        return 0;
    }
    let mut tc = v8::TryCatch::new(scope);
    let result = eval_src(&mut tc, spec.source, spec.entry);
    if tc.is_execution_terminating() {
        tc.cancel_terminate_execution();
        return world_mut(&mut tc).take_terminated_exit();
    }
    match result {
        Some(_) => {
            drop(tc);
            // A vm.exit inside the script can be swallowed along with its
            // termination; the guest's exit code still wins over the
            // checkpoint decision (Node: process.exit beats a pending
            // rejection).
            let exit = world_mut(scope).exit_code();
            if exit >= 0 {
                return exit;
            }
            // End of the entry tick: V8 has drained the script's
            // microtasks, so the checkpoint decision applies now, before
            // any timer or network turn can rescue a rejection.
            stopped(scope)
                .or_else(|| event_loop::check_unhandled(scope))
                .unwrap_or(0)
        }
        None => {
            let decides = world_mut(&mut tc).exception_decides();
            let code = world_mut(&mut tc).exception_exit();
            if decides {
                report(&mut tc);
            }
            code
        }
    }
}

/// Writes the dump straight from the run's RamFs: serialization is pure
/// Rust over the embedder slot, so no V8 call, no JS, and nothing that
/// could depend on how the run ended (normal exit, vm.exit, uncaught
/// exception, timeout). False means the dump could not be produced
/// (serialize failure or the file could not be written).
fn write_dump(scope: &mut v8::HandleScope, dump: &std::path::Path) -> bool {
    let fs = &world_mut(scope).fs;
    let json = match snapshot::encode(&fs.files, &fs.dirs) {
        Ok(json) => json,
        Err(e) => {
            eprintln!("v8vm: snapshot failed; dump not written: {}", e);
            return false;
        }
    };
    match dump::write(dump, &json) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("v8vm: cannot write dump {}: {}", dump.display(), e);
            false
        }
    }
}

/// Shared body of run() and serve(): returns the guest exit code. Early
/// returns are safe here; the caller owns teardown ordering.
fn execute_inner(
    scope: &mut v8::HandleScope,
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

    let watchdog = Watchdog::new(
        handle.clone(),
        spec.cfg.timeout_ms,
        world_mut(scope).term.clone(),
    );

    let mut code = eval_entry(scope, spec);
    if code == 0 {
        code = match replies {
            Some(replies) => serve::serve_loop(scope, replies),
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
        let mut guard = Box::new(heap::HeapGuard::new(handle.clone(), term.clone(), &plan));
        guard.register(&mut isolate);
        // The isolate owns the world through its embedder slot. On isolate
        // teardown the annex nulls its isolate pointer before the slots
        // are dropped, so the slot's Global handles see a disposed isolate
        // and skip resetting themselves (v8 handle.rs) - no manual
        // clearing needed.
        isolate.set_slot(world);
        isolate.set_promise_reject_callback(promise_reject_callback);
        (Guest { isolate, _guard: guard }, handle)
    }
}

fn execute(source: &str, entry: &str, cfg: &Config, replies: Option<&serve::ReplySink>) -> i32 {
    let clock = match cfg.clock {
        Clock::Real => RunClock::Real(std::time::Instant::now()),
        Clock::Virtual { epoch_ms } => RunClock::Virtual { epoch_ms, now_ms: 0 },
    };
    let mut world = World::new(cfg.fs_quota_bytes, clock);
    // serve mode: stdin is the command channel, so guest stdin is at EOF.
    world.stdin_eof = cfg.stdin_consumed || replies.is_some();
    // Seeded runs keep the PRNG counter in the World, so concurrent runs
    // with different seeds stay independent.
    world.random_state = cfg.seed;
    let term = world.term.clone();

    let spec = RunSpec { source, entry, cfg };
    let (mut guest, handle) = Guest::new(cfg, world);
    let code = {
        let mut hs = v8::HandleScope::new(&mut guest.isolate);
        let context = v8::Context::new(&mut hs, Default::default());
        let mut scope = v8::ContextScope::new(&mut hs, context);
        execute_inner(&mut scope, &handle, &spec, replies)
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
    finish(rx.recv().expect("v8vm guest finished"))
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
    finish(rx.recv().expect("v8vm guest finished"))
}
