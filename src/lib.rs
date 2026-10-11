//! v8vm: run small JavaScript applications entirely in RAM on the Chromium
//! V8 engine, embedded through the v8 crate.
//!
//! Library surface: [run] executes one guest program and returns its exit
//! code; [serve] runs the console protocol over any reader/writer pair.
//! Each call owns its isolate and its world state, so guests can run
//! concurrently in one process without seeing each other. Disk is touched
//! only at the explicit host boundary (entry source, preload/dump files).
//! Guests never make a syscall of their own.

mod api;
mod console;
mod event_loop;
mod exit;
mod fs;
mod net;
mod serve;
mod stdin;
mod timers;
mod watchdog;
mod world;

use std::io::Write;
use std::sync::LazyLock;

use api::install;
use event_loop::{event_loop, report};
use watchdog::Watchdog;
use world::{to_rust_string, world_mut, World, promise_reject_callback};

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

static V8_INIT: LazyLock<()> = LazyLock::new(|| {
    // Unprotected platform: the default one enforces thread-isolated (pkey
    // write-protected) code allocations, whose threading guarantees assume a
    // pumped message loop per isolate. Guests run isolates on arbitrary
    // user threads without pumping, so two concurrent runs crashed with
    // SEGV_PKUERR in V8's shared code range. The v8 crate ships this
    // variant for exactly that embedding style (its own multi-threaded
    // tests use it); the cost is losing JIT write-protection hardening.
    let platform = v8::new_unprotected_default_platform(0, false).make_shared();
    v8::V8::initialize_platform(platform);
    v8::V8::initialize();
});

fn eval_src<'s>(
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
        let exit = world_mut(&mut tc).exit_code();
        return if exit >= 0 { exit } else { 124 };
    }
    report(&mut tc);
    1
}

fn write_dump(scope: &mut v8::HandleScope, dump: &std::path::Path) {
    let mut tc = v8::TryCatch::new(scope);
    match eval_src(&mut tc, "vm.snapshot()", "dump") {
        Some(v) => {
            let json = to_rust_string(&mut tc, &v);
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
    output: Option<&mut dyn Write>,
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
        code = match output {
            Some(out) => serve::serve_loop(scope, std::io::stdin().lock(), out),
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
    output: Option<&mut dyn Write>,
) -> i32 {
    LazyLock::force(&V8_INIT);

    let mut world = World::new();
    // serve mode: stdin is the command channel, so guest stdin is at EOF.
    world.stdin_eof = cfg.stdin_consumed || output.is_some();

    // Heap cap; past it V8 aborts with its own out-of-memory report.
    let params = v8::CreateParams::default().heap_limits(64 << 20, cfg.heap_mb << 20);
    let mut isolate = v8::Isolate::new(params);
    // The isolate owns the world through the slot; slot teardown drops it
    // (and its Global handles) while the isolate is still alive, replacing
    // the old manual clearing against a process global.
    isolate.set_slot(world);
    isolate.set_promise_reject_callback(promise_reject_callback);
    let handle = isolate.thread_safe_handle();

    let spec = RunSpec { source, entry, cfg };
    let mut hs = v8::HandleScope::new(&mut isolate);
    let context = v8::Context::new(&mut hs, Default::default());
    let scope = &mut v8::ContextScope::new(&mut hs, context);
    execute_inner(scope, &handle, &spec, output)
}

/// Run one guest program and return its exit code.
pub fn run(source: &str, entry: &str, cfg: &Config) -> i32 {
    execute(source, entry, cfg, None)
}

/// Serve the console protocol on the given writer (reads commands from
/// stdin). One JS expression per line in, one JSON line out.
pub fn serve<W: Write>(output: W, source: &str, entry: &str, cfg: &Config) -> i32 {
    let mut output = output;
    let out: &mut dyn Write = &mut output;
    execute(source, entry, cfg, Some(out))
}
