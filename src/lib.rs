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
mod event_loop;
mod exit;
mod fs;
mod heap;
mod host;
mod net;
mod serve;
mod stdin;
mod timers;
mod watchdog;
mod world;

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
    /// Write snapshot() JSON to this file when the run ends.
    pub dump: Option<std::path::PathBuf>,
    /// True when the entry source was read from stdin, so guest stdin is
    /// already at EOF.
    pub stdin_consumed: bool,
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

/// Seeds vm.fs from a snapshot dump; Some(code) aborts the run.
fn apply_preload(scope: &mut v8::HandleScope, cfg: &Config) -> Option<i32> {
    let preload = cfg.preload.as_ref()?;
    let state = match read_path(preload) {
        Some(state) => state,
        None => {
            eprintln!("v8vm: cannot open preload {:?}", preload);
            return Some(2);
        }
    };
    let key = v8::String::new(scope, "__PRELOAD__").unwrap();
    let val = v8::String::new_from_utf8(scope, state.as_bytes(), v8::NewStringType::Normal).unwrap();
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
        return world_mut(&mut tc).terminated_exit();
    }
    report(&mut tc);
    1
}

fn write_dump(scope: &mut v8::HandleScope, dump: &std::path::Path) {
    let mut tc = v8::TryCatch::new(scope);
    match eval_src(&mut tc, "vm.snapshot()", "dump") {
        Some(v) => {
            let json = diag::to_rust_string(&mut tc, &v);
            if std::fs::write(dump, json).is_err() {
                eprintln!("v8vm: cannot write dump {:?}", dump);
            }
        }
        None if tc.is_execution_terminating() => {
            tc.cancel_terminate_execution();
        }
        None => {
            report(&mut tc);
            eprintln!("v8vm: snapshot failed; dump not written");
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
        write_dump(scope, dump);
    }
    code
}

fn execute(
    source: &str,
    entry: &str,
    cfg: &Config,
    replies: Option<&serve::ReplySink>,
) -> i32 {
    let mut world = World::new();
    // serve mode: stdin is the command channel, so guest stdin is at EOF.
    world.stdin_eof = cfg.stdin_consumed || replies.is_some();
    world.heap_mb = cfg.heap_mb.max(1);

    // V8 dies at isolate creation (a check failure, SIGTRAP) when the
    // initial heap exceeds the cap, so the initial size is clamped to it;
    // a 0 MB cap from library callers is clamped to 1 MB the same way.
    let cap_mb = cfg.heap_mb.max(1);
    let params = v8::CreateParams::default().heap_limits(cap_mb.min(64) << 20, cap_mb << 20);
    let mut isolate = v8::Isolate::new(params);
    let handle = isolate.thread_safe_handle();
    let mut guard = Box::new(heap::HeapGuard::new(handle.clone(), world.heap_oom.clone()));
    isolate.add_near_heap_limit_callback(heap::on_near_heap_limit, guard.callback_data());
    // The isolate owns the world through its embedder slot. On isolate
    // teardown the annex nulls its isolate pointer before the slots are
    // dropped, so the slot's Global handles see a disposed isolate and skip
    // resetting themselves (v8 handle.rs) - no manual clearing needed.
    isolate.set_slot(world);
    isolate.set_promise_reject_callback(promise_reject_callback);

    let spec = RunSpec { source, entry, cfg };
    let code = {
        let mut hs = v8::HandleScope::new(&mut isolate);
        let context = v8::Context::new(&mut hs, Default::default());
        let mut scope = v8::ContextScope::new(&mut hs, context);
        execute_inner(&mut scope, &handle, &spec, replies)
    };
    // The isolate is torn down while the guard is still alive: teardown can
    // run GC, and the near-heap-limit callback would otherwise read freed
    // memory through its data pointer. The guard drops after this point.
    drop(isolate);
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
