//! The guest event loop: network generations, timers, microtasks, stdin.
//! Keeps the process alive while any guest work is pending.

use std::time::{Duration, Instant};

use crate::stdin;
use crate::world::{to_rust_string, world_mut, NetMsg, World};
use v8::HandleScope;

pub(crate) enum Outcome {
    Ok,
    Exception,
    Terminated,
}

/// Terminated means guest-requested vm.exit() if set, else the watchdog.
fn stop_code(world: &World, oc: Outcome) -> Option<i32> {
    match oc {
        Outcome::Ok => None,
        Outcome::Terminated => {
            let code = world.exit_code();
            Some(if code >= 0 { code } else { 124 })
        }
        Outcome::Exception => Some(1),
    }
}

pub(crate) fn report(tc: &mut v8::TryCatch<'_, v8::HandleScope<'_>>) {
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

fn call_fn(scope: &mut HandleScope, g: &v8::Global<v8::Function>) -> Outcome {
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

fn run_net(scope: &mut HandleScope) -> Option<i32> {
    if world_mut(scope).net.is_empty() {
        return None;
    }
    // One generation per loop turn: replies arrive on the next turn.
    let batch: Vec<NetMsg> = std::mem::take(&mut world_mut(scope).net).into();
    for m in batch {
        let listener = {
            let world = world_mut(scope);
            // Local copy roots the function even if the listener is replaced.
            match world.listeners.get(&m.to) {
                Some(l) => l.clone(),
                None => continue, // listener closed meanwhile
            }
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
        if let Some(code) = stop_code(world_mut(&mut tc), oc) {
            return Some(code);
        }
    }
    None
}

fn run_timers(scope: &mut HandleScope) -> Option<i32> {
    loop {
        let now = Instant::now();
        let idx = {
            let world = world_mut(scope);
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
        let mut timer = world_mut(scope).timers.swap_remove(idx);
        let (id, repeat, interval_ms) = (timer.id, timer.repeat, timer.interval_ms);
        let oc = call_fn(scope, &timer.cb);
        if let Some(code) = stop_code(world_mut(scope), oc) {
            return Some(code);
        }
        let world = world_mut(scope);
        if repeat && !world.cleared.contains(&id) {
            timer.when = Instant::now() + Duration::from_millis(interval_ms.max(1));
            world.timers.push(timer);
        }
        world.cleared.retain(|&c| c != id);
    }
}

fn drain_microtasks(scope: &mut HandleScope) -> Option<i32> {
    scope.perform_microtask_checkpoint();
    if !scope.is_execution_terminating() {
        return None;
    }
    scope.cancel_terminate_execution();
    let code = world_mut(scope).exit_code();
    Some(if code >= 0 { code } else { 124 })
}

/// After the loop ends: an unhandled rejection fails the run with 1.
fn finish_unhandled(scope: &mut HandleScope) -> Option<i32> {
    let exc = world_mut(scope).unhandled_rejection_exc.take();
    if let Some(exc) = exc {
        let v = exc.open(scope);
        let text = to_rust_string(scope, v);
        world_mut(scope).unhandled_rejection = Some(text);
    }
    let rejection = world_mut(scope).unhandled_rejection.clone();
    rejection.map(|r| {
        eprintln!("v8vm: unhandled promise rejection: {}", r);
        1
    })
}

pub(crate) fn event_loop(scope: &mut HandleScope) -> i32 {
    loop {
        if scope.is_execution_terminating() {
            scope.cancel_terminate_execution();
            let code = world_mut(scope).exit_code();
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
        if let Some(c) = stdin::poll_stdin(scope) {
            return c;
        }
        let (has_timers, has_net, has_stdin) = {
            let world = world_mut(scope);
            (
                !world.timers.is_empty(),
                !world.net.is_empty(),
                !world.stdin_waiters.is_empty(),
            )
        };
        if !has_timers && !has_net && !has_stdin {
            break;
        }
        std::thread::sleep(wake_deadline(scope, has_timers, has_stdin));
    }
    if let Some(code) = finish_unhandled(scope) {
        return code;
    }
    let code = world_mut(scope).exit_code();
    if code >= 0 {
        code
    } else {
        0
    }
}

/// When to wake for the next loop turn. ponytail: poll stdin at 2 ms while a
/// reader waits; else idle up to 50 ms; never past the earliest timer.
fn wake_deadline(scope: &mut HandleScope, has_timers: bool, has_stdin: bool) -> Duration {
    let cap = if has_stdin { 2 } else { 50 };
    let mut wake = Instant::now() + Duration::from_millis(cap);
    if has_timers {
        let earliest = world_mut(scope)
            .timers
            .iter()
            .map(|t| t.when)
            .min()
            .unwrap();
        if earliest < wake {
            wake = earliest;
        }
    }
    wake.saturating_duration_since(Instant::now())
}
