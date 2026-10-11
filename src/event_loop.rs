//! The guest event loop: network generations, timers, microtasks, stdin.
//! Keeps the process alive while any guest work is pending.

use std::time::{Duration, Instant};

use crate::diag::{report, to_rust_string};
use crate::stdin;
use crate::world::{world_mut, NetMsg, World};
use v8::HandleScope;

enum Outcome {
    Ok,
    Exception,
    Terminated,
}

/// Terminated means guest-requested vm.exit() if set, else the heap cap
/// (137) or the watchdog (124).
fn stop_code(world: &mut World, oc: Outcome) -> Option<i32> {
    match oc {
        Outcome::Ok => None,
        Outcome::Terminated => Some(world.take_terminated_exit()),
        Outcome::Exception => Some(1),
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
        let idx = idx?;
        // Move the timer out: the callback may add/clear timers. Whether it
        // cleared itself mid-callback is tracked in world.running_timer.
        let mut timer = world_mut(scope).timers.swap_remove(idx);
        let (id, repeat, interval_ms) = (timer.id, timer.repeat, timer.interval_ms);
        world_mut(scope).running_timer.enter(id);
        let oc = call_fn(scope, &timer.cb);
        world_mut(scope).running_timer.exit();
        if let Some(code) = stop_code(world_mut(scope), oc) {
            return Some(code);
        }
        let world = world_mut(scope);
        if repeat && !world.running_timer.self_cleared() {
            timer.when = Instant::now() + Duration::from_millis(interval_ms.max(1));
            world.timers.push(timer);
        }
    }
}

fn drain_microtasks(scope: &mut HandleScope) -> Option<i32> {
    scope.perform_microtask_checkpoint();
    if !scope.is_execution_terminating() {
        // Node-style checkpoint boundary: handlers that arrived during the
        // drain have already been forgotten; what is left has failed.
        return check_unhandled(scope);
    }
    scope.cancel_terminate_execution();
    Some(world_mut(scope).take_terminated_exit())
}

// ---- unhandled rejections ----

/// Removes the remembered rejection for `msg`'s promise, if any: a handler
/// attached before the next checkpoint un-fails the run.
fn forget_rejection(scope: &mut v8::CallbackScope, msg: &v8::PromiseRejectMessage) {
    let promise = msg.get_promise();
    let hash = promise.get_identity_hash().get();
    // Bucketed by identity hash so a mass catch does not scan every
    // remembered rejection; identity is confirmed with == inside the
    // bucket (hashes are stable but not unique).
    let mut bucket = match world_mut(scope).pending_rejections.remove(&hash) {
        Some(b) => b,
        None => return,
    };
    bucket.retain(|e| e.promise.open(scope) != &*promise);
    let world = world_mut(scope);
    if !bucket.is_empty() {
        world.pending_rejections.insert(hash, bucket);
    }
}

pub(crate) extern "C" fn promise_reject_callback(msg: v8::PromiseRejectMessage) {
    use v8::PromiseRejectEvent::*;
    // The callback runs inside V8's own scope; opening a second HandleScope
    // on the isolate would panic ("active scope can't be dropped") and abort.
    let scope = &mut unsafe { v8::CallbackScope::new(&msg) };
    match msg.get_event() {
        PromiseRejectWithNoHandler => {
            let promise = v8::Global::new(scope, msg.get_promise());
            let value = msg.get_value().map(|v| v8::Global::new(scope, v));
            let hash = msg.get_promise().get_identity_hash().get();
            let world = world_mut(scope);
            let seq = world.next_rejection_seq;
            world.next_rejection_seq += 1;
            world
                .pending_rejections
                .entry(hash)
                .or_default()
                .push(crate::world::PendingRejection { seq, promise, value });
        }
        PromiseHandlerAddedAfterReject => forget_rejection(scope, &msg),
        _ => {}
    }
}

/// After a microtask checkpoint: the first (lowest seq) rejection still
/// missing a handler fails the run with 1.
fn check_unhandled(scope: &mut HandleScope) -> Option<i32> {
    let first = {
        let world = world_mut(scope);
        // Buckets keep insertion order, so each bucket's front is its
        // earliest rejection; the minimum over the fronts is the first.
        let hash = world
            .pending_rejections
            .iter()
            .filter(|(_, b)| !b.is_empty())
            .min_by_key(|(_, b)| b[0].seq)
            .map(|(h, _)| *h)?;
        let bucket = world.pending_rejections.get_mut(&hash).unwrap();
        let first = bucket.remove(0);
        if bucket.is_empty() {
            world.pending_rejections.remove(&hash);
        }
        first
    };
    let text = match &first.value {
        Some(v) => {
            let val = v.open(scope);
            to_rust_string(scope, val)
        }
        None => "unknown".to_string(),
    };
    eprintln!("v8vm: unhandled promise rejection: {}", text);
    Some(1)
}

pub(crate) fn event_loop(scope: &mut HandleScope) -> i32 {
    loop {
        if scope.is_execution_terminating() {
            scope.cancel_terminate_execution();
            return world_mut(scope).take_terminated_exit();
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

