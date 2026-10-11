//! v8vm: run small JavaScript applications entirely in RAM on the Chromium
//! V8 engine, embedded through the v8 crate.
//!
//! Library surface: [run] executes one guest program and returns its exit
//! code; [serve] runs the console protocol over any reader/writer pair.
//! Disk is touched only at the explicit host boundary (entry source,
//! preload/dump files). Guests never make a syscall of their own.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicI32, AtomicPtr, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

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

struct Timer {
    id: u64,
    repeat: bool,
    interval_ms: u64,
    when: Instant,
    cb: v8::Global<v8::Function>,
}

struct NetMsg {
    from: u64,
    to: u64,
    payload: String,
}

/// All guest world state. One JS thread owns it; the watchdog thread only
/// ever calls Isolate::terminate_execution().
struct World {
    fs: BTreeMap<String, String>,
    timers: Vec<Timer>,
    next_timer_id: u64,
    net: VecDeque<NetMsg>,
    listeners: BTreeMap<u64, v8::Global<v8::Function>>,
    next_port: u64,
    stdin_waiters: Vec<v8::Global<v8::PromiseResolver>>,
    stdin_eof: bool,
    stdin_thread: Option<Sender<Option<Vec<u8>>>>,
    // ids cleared via clear_timer while their callback was on the stack
    cleared: Vec<u64>,
    exit_code: AtomicI32,
    unhandled_rejection: Option<String>,
    unhandled_rejection_exc: Option<v8::Global<v8::Value>>,
}

impl World {
    fn new() -> Self {
        World {
            fs: BTreeMap::new(),
            timers: Vec::new(),
            next_timer_id: 1,
            net: VecDeque::new(),
            listeners: BTreeMap::new(),
            next_port: 1,
            stdin_waiters: Vec::new(),
            stdin_eof: false,
            stdin_thread: None,
            cleared: Vec::new(),
            exit_code: AtomicI32::new(-1),
            unhandled_rejection: None,
            unhandled_rejection_exc: None,
        }
    }

    fn exit_code(&self) -> i32 {
        self.exit_code.load(Ordering::SeqCst)
    }
}

// Callbacks are raw extern "C" without captures, so the world and isolate
// are reached through process-lifetime pointers valid for the duration of a
// run (single JS thread; watchdog only terminates).
static RAW_ISOLATE: AtomicPtr<v8::Isolate> = AtomicPtr::new(std::ptr::null_mut());
static RAW_WORLD: AtomicPtr<World> = AtomicPtr::new(std::ptr::null_mut());

struct SendPtr(*mut v8::Isolate);
unsafe impl Send for SendPtr {}

fn world() -> &'static mut World {
    unsafe { &mut *RAW_WORLD.load(Ordering::SeqCst) }
}

fn to_rust_string(scope: &mut v8::HandleScope, v: &v8::Value) -> String {
    match v.to_string(scope) {
        Some(s) => s.to_rust_string_lossy(scope),
        None => String::new(),
    }
}

fn throw(scope: &mut v8::HandleScope, msg: &str) {
    let msg = v8::String::new(scope, msg).unwrap();
    let exc = v8::Exception::error(scope, msg);
    scope.throw_exception(exc.into());
}

/// Declares a guest callback in the crate's closure-based callback style.
macro_rules! guest_cb {
    ($name:ident, |$args:ident, $rv:ident, $scope:ident, $world:ident| $body:block) => {
        extern "C" fn $name(info: *const v8::FunctionCallbackInfo) {
            let info = unsafe { &*info };
            let $scope = unsafe { &mut v8::CallbackScope::new(info) };
            let $args = v8::FunctionCallbackArguments::from_function_callback_info(info);
            let mut $rv = v8::ReturnValue::from_function_callback_info(info);
            let $world = world();
            $body
        }
    };
}

fn print_line(
    args: &v8::FunctionCallbackArguments,
    scope: &mut v8::HandleScope,
    out: &mut dyn Write,
) {
    let mut line = String::new();
    for i in 0..args.length() {
        if i > 0 {
            line.push(' ');
        }
        line.push_str(&to_rust_string(scope, &args.get(i)));
    }
    line.push('\n');
    let _ = out.write_all(line.as_bytes());
}

// ---- console ----

guest_cb!(console_log, |args, _rv, scope, _world| {
    let mut stdout = std::io::stdout();
    print_line(&args, scope, &mut stdout);
});

guest_cb!(console_err, |args, _rv, scope, _world| {
    let mut stderr = std::io::stderr();
    print_line(&args, scope, &mut stderr);
});

// ---- filesystem ----

guest_cb!(fs_read, |args, rv, scope, world| {
    let path = to_rust_string(scope, &args.get(0));
    match world.fs.get(&path) {
        Some(contents) => {
            let s =
                v8::String::new_from_utf8(scope, contents.as_bytes(), v8::NewStringType::Normal)
                    .unwrap();
            rv.set(s.into());
        }
        None => throw(scope, &format!("no such file: {}", path)),
    }
});

guest_cb!(fs_write, |args, _rv, scope, world| {
    let path = to_rust_string(scope, &args.get(0));
    let data = to_rust_string(scope, &args.get(1));
    world.fs.insert(path, data);
});

guest_cb!(fs_list, |args, rv, scope, world| {
    let mut dir = if args.length() > 0 && !args.get(0).is_undefined() {
        to_rust_string(scope, &args.get(0))
    } else {
        "/".to_string()
    };
    if !dir.ends_with('/') {
        dir.push('/');
    }
    let names: Vec<_> = world.fs.keys().filter(|p| p.starts_with(&dir)).collect();
    let array = v8::Array::new(scope, names.len() as i32);
    for (i, p) in names.iter().enumerate() {
        let s = v8::String::new(scope, p).unwrap();
        array.set_index(scope, i as u32, s.into());
    }
    rv.set(array.into());
});

guest_cb!(fs_remove, |args, _rv, scope, world| {
    let path = to_rust_string(scope, &args.get(0));
    if world.fs.remove(&path).is_none() {
        throw(scope, &format!("no such file: {}", path));
    }
});

// ---- exit ----

guest_cb!(vm_exit, |args, _rv, scope, world| {
    let code = if args.length() > 0 {
        args.get(0).int32_value(scope).unwrap_or(0)
    } else {
        0
    };
    world.exit_code.store(code, Ordering::SeqCst);
    scope.terminate_execution();
});

// ---- timers ----

fn set_timer_common(
    args: &v8::FunctionCallbackArguments,
    rv: &mut v8::ReturnValue,
    scope: &mut v8::HandleScope,
    world: &mut World,
    repeat: bool,
) {
    if args.length() == 0 || !args.get(0).is_function() {
        throw(scope, "timer callback must be a function");
        return;
    }
    let delay = if args.length() > 1 {
        args.get(1).number_value(scope).unwrap_or(0.0).max(0.0)
    } else {
        0.0
    };
    let interval_ms = delay as u64;
    let id = world.next_timer_id;
    world.next_timer_id += 1;
    // Extra arguments are captured guest-side by the prelude wrapper, so the
    // host only ever stores the callback itself.
    let f: v8::Local<v8::Function> = args.get(0).try_into().unwrap();
    let cb = v8::Global::new(scope, f);
    world.timers.push(Timer {
        id,
        repeat,
        interval_ms,
        when: Instant::now() + Duration::from_millis(interval_ms.max(1)),
        cb,
    });
    rv.set(v8::Number::new(scope, id as f64).into());
}

guest_cb!(set_timeout, |args, rv, scope, world| {
    set_timer_common(&args, &mut rv, scope, world, false);
});

guest_cb!(set_interval, |args, rv, scope, world| {
    set_timer_common(&args, &mut rv, scope, world, true);
});

guest_cb!(clear_timer, |args, _rv, scope, world| {
    if args.length() == 0 {
        return;
    }
    let id = args.get(0).number_value(scope).unwrap_or(-1.0);
    world.timers.retain(|t| t.id as f64 != id);
    world.cleared.push(id as u64);
});

// ---- in-memory network ----

guest_cb!(net_port, |_args, rv, scope, world| {
    let port = world.next_port;
    world.next_port += 1;
    rv.set(v8::Number::new(scope, port as f64).into());
});

guest_cb!(net_listen, |args, _rv, scope, world| {
    let port = args.get(0).number_value(scope).unwrap_or(-1.0);
    if port < 0.0 || !args.get(1).is_function() {
        throw(scope, "net.listen(port, handler) requires a function");
        return;
    }
    let cb: v8::Local<v8::Function> = args.get(1).try_into().unwrap();
    world
        .listeners
        .insert(port as u64, v8::Global::new(scope, cb));
});

guest_cb!(net_send, |args, _rv, scope, world| {
    let from = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let to = args.get(1).number_value(scope).unwrap_or(-1.0);
    let to_port = to as u64;
    if to < 0.0 || !world.listeners.contains_key(&to_port) {
        throw(scope, &format!("no listener on port {}", to_port));
        return;
    }
    world.net.push_back(NetMsg {
        from,
        to: to_port,
        payload: to_rust_string(scope, &args.get(2)),
    });
});

guest_cb!(net_close, |args, _rv, scope, world| {
    let port = args.get(0).number_value(scope).unwrap_or(-1.0) as u64;
    world.listeners.remove(&port);
});

// ---- stdin ----

guest_cb!(stdin_read, |_args, rv, scope, world| {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    if world.stdin_eof {
        let empty = v8::String::new(scope, "").unwrap();
        resolver.resolve(scope, empty.into());
    } else {
        world.stdin_waiters.push(v8::Global::new(scope, resolver));
        start_stdin_thread(world);
    }
    rv.set(resolver.get_promise(scope).into());
});

/// Lazily spawns the blocking stdin reader. It sends chunks down a channel
/// the event loop polls; on EOF it sends None and exits. No libc needed.
static STDIN_CHUNKS: LazyLock<Mutex<Option<Receiver<Option<Vec<u8>>>>>> =
    LazyLock::new(|| Mutex::new(None));

fn start_stdin_thread(world: &mut World) {
    if world.stdin_thread.is_some() {
        return;
    }
    let (tx, rx) = mpsc::channel::<Option<Vec<u8>>>();
    let sender = tx.clone();
    *STDIN_CHUNKS.lock().unwrap() = Some(rx);
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 65536];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = sender.send(None);
                    break;
                }
                Ok(n) => {
                    if sender.send(Some(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
    });
    world.stdin_thread = Some(tx);
}

// ---- unhandled rejections ----

extern "C" fn promise_reject_callback(msg: v8::PromiseRejectMessage) {
    if msg.get_event() != v8::PromiseRejectEvent::PromiseRejectWithNoHandler {
        return;
    }
    let world = world();
    if world.unhandled_rejection.is_some() {
        return;
    }
    let isolate = unsafe { &mut *RAW_ISOLATE.load(Ordering::SeqCst) };
    let scope = &mut v8::HandleScope::new(isolate);
    // ponytail: first rejection wins; a later handler does not un-fail the
    // run. The value is stringified later, inside the guest context scope.
    match msg.get_value() {
        Some(v) => world.unhandled_rejection_exc = Some(v8::Global::new(scope, v)),
        None => world.unhandled_rejection = Some("unknown".to_string()),
    }
}

// ---- API installation ----

const PRELUDE: &str = r#"
globalThis.queueMicrotask = (fn) => Promise.resolve().then(fn);
const __setTimeout = setTimeout, __setInterval = setInterval;
globalThis.setTimeout = (fn, delay, ...args) =>
  __setTimeout(args.length ? () => fn(...args) : fn, delay);
globalThis.setInterval = (fn, delay, ...args) =>
  __setInterval(args.length ? () => fn(...args) : fn, delay);
vm.snapshot = () => JSON.stringify(
  Object.fromEntries(vm.fs.list("/").map((p) => [p, vm.fs.read(p)])));
vm.restore = (s) => {
  const m = JSON.parse(s);
  for (const p in m) vm.fs.write(p, m[p]);
};
vm.run = (p) => (0, eval)(vm.fs.read(p));
"#;

fn make_fn<'s>(
    scope: &mut v8::HandleScope<'s>,
    cb: v8::FunctionCallback,
) -> Option<v8::Local<'s, v8::Function>> {
    v8::Function::new_raw(scope, cb)
}

fn set(
    obj: v8::Local<v8::Object>,
    scope: &mut v8::HandleScope,
    name: &str,
    value: v8::Local<v8::Value>,
) {
    let key = v8::String::new(scope, name).unwrap();
    obj.set(scope, key.into(), value);
}

fn put_fn(
    obj: v8::Local<v8::Object>,
    scope: &mut v8::HandleScope,
    name: &str,
    cb: v8::FunctionCallback,
) {
    let f = make_fn(scope, cb).unwrap();
    set(obj, scope, name, f.into());
}

fn install(scope: &mut v8::HandleScope, cfg: &Config) {
    let global = scope.get_current_context().global(scope);

    // The context snapshot bundled with V8 already provides a native console
    // whose output goes nowhere (no inspector is wired up); direct global
    // assignment overrides it.
    let console = v8::Object::new(scope);
    put_fn(console, scope, "log", console_log);
    put_fn(console, scope, "info", console_log);
    put_fn(console, scope, "warn", console_err);
    put_fn(console, scope, "error", console_err);
    set(global, scope, "console", console.into());

    let vm = v8::Object::new(scope);
    let version = v8::String::new(scope, v8::V8::get_version()).unwrap();
    set(vm, scope, "version", version.into());

    let args = v8::Array::new(scope, cfg.args.len() as i32);
    for (i, a) in cfg.args.iter().enumerate() {
        let s = v8::String::new(scope, a).unwrap();
        args.set_index(scope, i as u32, s.into());
    }
    set(vm, scope, "args", args.into());

    let fs = v8::Object::new(scope);
    put_fn(fs, scope, "read", fs_read);
    put_fn(fs, scope, "write", fs_write);
    put_fn(fs, scope, "list", fs_list);
    put_fn(fs, scope, "remove", fs_remove);
    set(vm, scope, "fs", fs.into());

    let net = v8::Object::new(scope);
    put_fn(net, scope, "port", net_port);
    put_fn(net, scope, "listen", net_listen);
    put_fn(net, scope, "send", net_send);
    put_fn(net, scope, "close", net_close);
    set(vm, scope, "net", net.into());

    let stdin_obj = v8::Object::new(scope);
    put_fn(stdin_obj, scope, "read", stdin_read);
    set(vm, scope, "stdin", stdin_obj.into());

    put_fn(vm, scope, "exit", vm_exit);
    set(global, scope, "vm", vm.into());

    put_fn(global, scope, "setTimeout", set_timeout);
    put_fn(global, scope, "setInterval", set_interval);
    put_fn(global, scope, "clearTimeout", clear_timer);
    put_fn(global, scope, "clearInterval", clear_timer);
}

// ---- diagnostics ----

fn report(tc: &mut v8::TryCatch<'_, v8::HandleScope<'_>>) {
    match tc.message() {
        Some(msg) => {
            let resource = msg
                .get_script_resource_name(tc)
                .map(|v| to_rust_string(tc, &v))
                .unwrap_or_default();
            let line = msg.get_line_number(tc).unwrap_or(0);
            let m: v8::Local<v8::Value> = msg.get(tc).into();
            let text = to_rust_string(tc, &m);
            eprintln!("v8vm: uncaught exception: {}:{}: {}", resource, line, text);
        }
        None => eprintln!("v8vm: uncaught exception"),
    }
}

// ---- event loop ----

enum Outcome {
    Ok,
    Exception,
    Terminated,
}

fn stop_code(oc: Outcome) -> Option<i32> {
    match oc {
        Outcome::Ok => None,
        // Terminated: guest-requested vm.exit() if set, else the watchdog.
        Outcome::Terminated => {
            let code = world().exit_code();
            Some(if code >= 0 { code } else { 124 })
        }
        Outcome::Exception => Some(1),
    }
}

fn call_fn(scope: &mut v8::HandleScope, g: &v8::Global<v8::Function>) -> Outcome {
    let mut tc = v8::TryCatch::new(scope);
    let this = v8::undefined(&mut tc).into();
    let result = g.open(&mut tc).call(&mut tc, this, &[]);
    if result.is_some() {
        return Outcome::Ok;
    }
    if tc.is_execution_terminating() {
        tc.cancel_terminate_execution();
        return Outcome::Terminated;
    }
    report(&mut tc);
    Outcome::Exception
}

fn run_net(scope: &mut v8::HandleScope) -> Option<i32> {
    let w = world();
    if w.net.is_empty() {
        return None;
    }
    // One generation per loop turn: replies arrive on the next turn.
    let batch: Vec<NetMsg> = std::mem::take(&mut w.net).into();
    for m in batch {
        let w = world();
        // Local copy roots the function even if the listener is replaced.
        let listener = match w.listeners.get(&m.to) {
            Some(l) => l.clone(),
            None => continue, // listener closed meanwhile
        };
        let payload =
            v8::String::new_from_utf8(scope, m.payload.as_bytes(), v8::NewStringType::Normal)
                .unwrap();
        let from = v8::Number::new(scope, m.from as f64);
        let mut tc = v8::TryCatch::new(scope);
        let this = v8::undefined(&mut tc).into();
        let argv = [payload.into(), from.into()];
        let result = listener.open(&mut tc).call(&mut tc, this, &argv);
        let oc = if result.is_some() {
            Outcome::Ok
        } else if tc.is_execution_terminating() {
            tc.cancel_terminate_execution();
            Outcome::Terminated
        } else {
            report(&mut tc);
            Outcome::Exception
        };
        if let Some(code) = stop_code(oc) {
            return Some(code);
        }
    }
    None
}

fn run_timers(scope: &mut v8::HandleScope) -> Option<i32> {
    loop {
        let now = Instant::now();
        let idx = {
            let world = world();
            world
                .timers
                .iter()
                .enumerate()
                .filter(|(_, t)| t.when <= now)
                .min_by_key(|(_, t)| t.when)
                .map(|(i, _)| i)
        };
        let idx = match idx {
            Some(i) => i,
            None => return None,
        };
        // Move the timer out: the callback may add/clear timers. Whether it
        // was cleared during its own callback is tracked in world.cleared.
        let mut timer = {
            let world = world();
            world.timers.swap_remove(idx)
        };
        let (id, repeat, interval_ms) = (timer.id, timer.repeat, timer.interval_ms);
        let oc = call_fn(scope, &timer.cb);
        if let Some(code) = stop_code(oc) {
            return Some(code);
        }
        let world = world();
        if repeat && !world.cleared.contains(&id) {
            timer.when = Instant::now() + Duration::from_millis(interval_ms.max(1));
            world.timers.push(timer);
        }
        world.cleared.retain(|&c| c != id);
    }
}

fn drain_microtasks(scope: &mut v8::HandleScope) -> Option<i32> {
    scope.perform_microtask_checkpoint();
    if !scope.is_execution_terminating() {
        return None;
    }
    scope.cancel_terminate_execution();
    let code = world().exit_code();
    Some(if code >= 0 { code } else { 124 })
}

fn resolve_stdin_waiters(scope: &mut v8::HandleScope, chunk: Option<&[u8]>) {
    let waiters: Vec<v8::Global<v8::PromiseResolver>> = {
        let w = world();
        if chunk.is_some() && !w.stdin_waiters.is_empty() {
            // Data case: hand the chunk to exactly one waiting reader.
            let resolver = w.stdin_waiters.pop().unwrap();
            let r = resolver.open(scope);
            let s = v8::String::new_from_utf8(scope, chunk.unwrap(), v8::NewStringType::Normal)
                .unwrap();
            r.resolve(scope, s.into());
            return;
        }
        std::mem::take(&mut w.stdin_waiters)
    };
    // EOF case: everyone still waiting resolves with "".
    let empty = v8::String::new(scope, "").unwrap();
    for w in waiters {
        w.open(scope).resolve(scope, empty.into());
    }
}

fn poll_stdin(scope: &mut v8::HandleScope) -> Option<i32> {
    {
        let world = world();
        if world.stdin_waiters.is_empty() {
            return None;
        }
        if world.stdin_eof {
            resolve_stdin_waiters(scope, None);
            return None;
        }
    }
    let chunk = STDIN_CHUNKS
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|rx| rx.try_recv().ok());
    match chunk {
        Some(Some(bytes)) => resolve_stdin_waiters(scope, Some(&bytes)),
        Some(None) => {
            world().stdin_eof = true;
            resolve_stdin_waiters(scope, None);
        }
        None => {}
    }
    None
}

/// Keeps the process alive while any guest work is pending.
fn event_loop(scope: &mut v8::HandleScope) -> i32 {
    loop {
        if scope.is_execution_terminating() {
            scope.cancel_terminate_execution();
            let code = world().exit_code();
            return if code >= 0 { code } else { 124 };
        }
        if let Some(c) = run_net(scope) {
            return c;
        }
        if let Some(c) = run_timers(scope) {
            return c;
        }
        if let Some(c) = drain_microtasks(scope) {
            return c;
        }
        if let Some(c) = poll_stdin(scope) {
            return c;
        }
        let (has_timers, has_net, has_stdin) = {
            let world = world();
            (
                !world.timers.is_empty(),
                !world.net.is_empty(),
                !world.stdin_waiters.is_empty(),
            )
        };
        if !has_timers && !has_net && !has_stdin {
            break;
        }
        // ponytail: poll stdin at 2 ms while a reader waits; else idle up to 50 ms.
        let cap = if has_stdin { 2 } else { 50 };
        let mut wake = Instant::now() + Duration::from_millis(cap);
        if has_timers {
            let earliest = world().timers.iter().map(|t| t.when).min().unwrap();
            if earliest < wake {
                wake = earliest;
            }
        }
        std::thread::sleep(wake.saturating_duration_since(Instant::now()));
    }
    let world = world();
    if let Some(exc) = world.unhandled_rejection_exc.take() {
        let v = exc.open(scope);
        let text = to_rust_string(scope, v);
        world.unhandled_rejection = Some(text);
    }
    if let Some(rej) = &world.unhandled_rejection {
        eprintln!("v8vm: unhandled promise rejection: {}", rej);
        return 1;
    }
    let code = world.exit_code();
    if code >= 0 {
        code
    } else {
        0
    }
}

// ---- serve mode ----

/// One JS expression per input line, one JSON value per output line. All
/// encoding happens guest-side (JSON.stringify), so the host stays JSON-free.
fn serve_loop<R: BufRead, W: Write>(scope: &mut v8::HandleScope, input: R, mut output: W) -> i32 {
    for line in input.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.is_empty() {
            continue;
        }
        // Concatenation, not format!: keeps the wrapper free of brace-escaping
        // mistakes (a missing brace here becomes a guest SyntaxError).
        let wrapped = String::from("(function(){try{return JSON.stringify((")
            + &line
            + "))}catch(e){return JSON.stringify({$error:String(e&&e.message||e)})}})()";
        let src = v8::String::new_from_utf8(scope, wrapped.as_bytes(), v8::NewStringType::Normal)
            .unwrap();
        let name = v8::String::new(scope, "serve").unwrap();
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
        let mut tc = v8::TryCatch::new(scope);
        let result = match v8::Script::compile(&mut tc, src, Some(&origin)) {
            Some(script) => script.run(&mut tc),
            None => None,
        };
        match result {
            Some(v) => {
                let out = to_rust_string(&mut tc, &v);
                let _ = writeln!(output, "{}", out);
                let _ = output.flush();
            }
            None => {
                if tc.is_execution_terminating() {
                    tc.cancel_terminate_execution();
                    let code = world().exit_code();
                    return if code >= 0 { code } else { 124 };
                }
                report(&mut tc);
                return 1;
            }
        }
        drop(tc);
        let code = event_loop(scope); // settle timers/net between commands
        let exit = world().exit_code();
        if exit >= 0 {
            return exit;
        }
        if code != 0 {
            return code;
        }
    }
    let exit = world().exit_code();
    if exit >= 0 {
        exit
    } else {
        0
    }
}

// ---- watchdog ----

struct Watchdog {
    stop: std::sync::Arc<(Mutex<bool>, std::sync::Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watchdog {
    fn new(iso: *mut v8::Isolate, timeout_ms: u64) -> Self {
        let state = std::sync::Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let thread = if timeout_ms == 0 {
            None
        } else {
            let state2 = state.clone();
            let iso = SendPtr(iso);
            Some(std::thread::spawn(move || {
                let iso = iso; // force whole-struct capture; edition 2021 disjoint capture would take the raw field, which is !Send
                let (lock, cv) = &*state2;
                let stopped = lock.lock().unwrap();
                // Predicate-aware wait: handles notify-before-wait and
                // spurious wakeups, unlike a bare wait_timeout.
                let (_guard, result) = cv
                    .wait_timeout_while(stopped, Duration::from_millis(timeout_ms), |s| !*s)
                    .unwrap();
                if result.timed_out() {
                    unsafe { (*iso.0).terminate_execution() };
                }
            }))
        };
        Watchdog {
            stop: state,
            thread,
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        *self.stop.0.lock().unwrap() = true;
        self.stop.1.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---- entry points ----

static V8_INIT: LazyLock<()> = LazyLock::new(|| {
    let platform = v8::new_default_platform(0, false).make_shared();
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

/// Shared body of run() and serve(): returns the exit code.
/// Inner body: returns the guest exit code. Early returns are safe here;
/// the caller owns teardown ordering.
fn execute_inner(
    scope: &mut v8::HandleScope,
    iso_ptr: *mut v8::Isolate,
    source: &str,
    entry: &str,
    cfg: &Config,
    output: Option<&mut dyn Write>,
) -> i32 {
    LazyLock::force(&V8_INIT);
    install(scope, cfg);
    {
        let mut tc = v8::TryCatch::new(scope);
        if eval_src(&mut tc, PRELUDE, "prelude").is_none() {
            report(&mut tc);
            return 1;
        }
    }

    if let Some(preload) = &cfg.preload {
        match read_path(preload) {
            Some(state) => {
                let key = v8::String::new(scope, "__PRELOAD__").unwrap();
                let val =
                    v8::String::new_from_utf8(scope, state.as_bytes(), v8::NewStringType::Normal)
                        .unwrap();
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
                    return 2;
                }
            }
            None => {
                eprintln!("v8vm: cannot open preload {:?}", preload);
                return 2;
            }
        }
    }

    let watchdog = Watchdog::new(iso_ptr, cfg.timeout_ms);

    let mut code = 0;
    if !source.is_empty() {
        let mut tc = v8::TryCatch::new(scope);
        if let None = eval_src(&mut tc, source, entry) {
            if tc.is_execution_terminating() {
                tc.cancel_terminate_execution();
                let exit = world().exit_code();
                code = if exit >= 0 { exit } else { 124 };
            } else {
                report(&mut tc);
                code = 1;
            }
        }
    }
    if code == 0 {
        code = match output {
            Some(out) => serve_loop(scope, std::io::stdin().lock(), out),
            None => event_loop(scope),
        };
    }
    drop(watchdog);

    if let Some(dump) = &cfg.dump {
        let mut tc = v8::TryCatch::new(scope);
        if let Some(v) = eval_src(&mut tc, "vm.snapshot()", "dump") {
            let json = to_rust_string(&mut tc, &v);
            if std::fs::write(dump, json).is_err() {
                eprintln!("v8vm: cannot write dump {:?}", dump);
            }
        } else if tc.is_execution_terminating() {
            tc.cancel_terminate_execution();
        } else {
            report(&mut tc);
            eprintln!("v8vm: snapshot failed; dump not written");
        }
    }
    code
}

fn execute(
    source: &str,
    entry: &str,
    cfg: &Config,
    serve: bool,
    output: Option<&mut dyn Write>,
) -> i32 {
    LazyLock::force(&V8_INIT);

    let mut world_box = Box::new(World::new());
    world_box.stdin_eof = cfg.stdin_consumed || serve; // serve: stdin is the command channel
    RAW_WORLD.store(&mut *world_box, Ordering::SeqCst);

    // Heap cap; past it V8 aborts with its own out-of-memory report.
    let params = v8::CreateParams::default().heap_limits(64 << 20, cfg.heap_mb << 20);
    let mut isolate = v8::Isolate::new(params);
    let iso_ptr: *mut v8::Isolate = &mut *isolate;
    RAW_ISOLATE.store(iso_ptr, Ordering::SeqCst);
    isolate.set_promise_reject_callback(promise_reject_callback);

    let code = {
        let mut hs = v8::HandleScope::new(&mut isolate);
        let context = v8::Context::new(&mut hs, Default::default());
        let scope = &mut v8::ContextScope::new(&mut hs, context);
        execute_inner(scope, iso_ptr, source, entry, cfg, output)
    };

    // Drop all Global handles while the isolate is still alive; their
    // destructors would otherwise touch a disposed isolate.
    world_box.timers.clear();
    world_box.listeners.clear();
    world_box.stdin_waiters.clear();
    RAW_WORLD.store(std::ptr::null_mut(), Ordering::SeqCst);
    RAW_ISOLATE.store(std::ptr::null_mut(), Ordering::SeqCst);
    code
}

/// Run one guest program and return its exit code.
pub fn run(source: &str, entry: &str, cfg: &Config) -> i32 {
    execute(source, entry, cfg, false, None)
}

/// Serve the console protocol on the given writer (reads commands from
/// stdin). One JS expression per line in, one JSON line out.
pub fn serve<W: Write>(output: W, source: &str, entry: &str, cfg: &Config) -> i32 {
    let mut output = output;
    let out: &mut dyn Write = &mut output;
    execute(source, entry, cfg, true, Some(out))
}
