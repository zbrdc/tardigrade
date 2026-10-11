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
mod console;
mod diag;
mod dump;
mod event_loop;
mod exit;
mod fs;
mod fs_api;
mod host;
mod net;
mod serve;
mod snapshot;
mod stdin;
mod timers;
mod watchdog;
mod world;

pub mod sandbox;

pub use fs::DEFAULT_FS_QUOTA_BYTES;

use std::io::Write;
use std::sync::mpsc;

use api::install;
use diag::report;
use event_loop::{event_loop, promise_reject_callback};
use watchdog::Watchdog;
use world::{world_mut, World};

/// Host-side run configuration.
#[derive(Clone, Debug)]
pub struct Config {
    /// Wall-clock cap in ms; 0 disables. Default 60000.
    pub timeout_ms: u64,
    /// V8 heap cap in MB. Default 256.
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
    if eval_src(&mut tc, spec.source, spec.entry).is_some() {
        return 0;
    }
    if tc.is_execution_terminating() {
        tc.cancel_terminate_execution();
        let exit = world_mut(&mut tc).exit_code();
        return if exit >= 0 { exit } else { 124 };
    }
    report(&mut tc);
    1
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

    let watchdog = Watchdog::new(handle.clone(), spec.cfg.timeout_ms);

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

fn execute(
    source: &str,
    entry: &str,
    cfg: &Config,
    replies: Option<&serve::ReplySink>,
) -> i32 {
    let mut world = World::new(cfg.fs_quota_bytes);
    // serve mode: stdin is the command channel, so guest stdin is at EOF.
    world.stdin_eof = cfg.stdin_consumed || replies.is_some();

    // Heap cap; past it V8 aborts with its own out-of-memory report.
    let params = v8::CreateParams::default().heap_limits(64 << 20, cfg.heap_mb << 20);
    let mut isolate = v8::Isolate::new(params);
    // The isolate owns the world through its embedder slot. On isolate
    // teardown the annex nulls its isolate pointer before the slots are
    // dropped, so the slot's Global handles see a disposed isolate and skip
    // resetting themselves (v8 handle.rs) - no manual clearing needed.
    isolate.set_slot(world);
    isolate.set_promise_reject_callback(promise_reject_callback);
    let handle = isolate.thread_safe_handle();

    let spec = RunSpec { source, entry, cfg };
    let mut hs = v8::HandleScope::new(&mut isolate);
    let context = v8::Context::new(&mut hs, Default::default());
    let scope = &mut v8::ContextScope::new(&mut hs, context);
    execute_inner(scope, &handle, &spec, replies)
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
