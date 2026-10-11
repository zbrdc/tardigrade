//! The guest event loop: network generations, timers, microtasks, stdin.
//! Keeps the process alive while any guest work is pending.

use std::time::Duration;

use crate::diag::{rejection_text, report};
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
        Outcome::Exception => Some(world.exception_exit()),
    }
}

/// True when an outside termination (watchdog, heap cap) or a direct V8
/// termination should end the run right now: the shared reason is checked
/// because V8's own flag can be swallowed inside microtask checkpoints and
/// GC callbacks, and may not be delivered at all while the loop is parked
/// outside V8. Cancels V8's flag and consumes the run's conclusion.
pub(crate) fn stopped(scope: &mut HandleScope) -> Option<i32> {
    let pending = world_mut(scope).term.reason().is_some();
    if !pending && !scope.is_execution_terminating() {
        return None;
    }
    if scope.is_execution_terminating() {
        scope.cancel_terminate_execution();
    }
    Some(world_mut(scope).take_terminated_exit())
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
    if world_mut(&mut tc).exception_decides() {
        report(&mut tc);
    }
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
            if world_mut(&mut tc).exception_decides() {
                report(&mut tc);
            }
            Outcome::Exception
        };
        drop(tc);
        // V8 drains microtasks after each callback; the checkpoint decision
        // is made right here (Node timing), not once per loop turn.
        if let Some(code) = stopped(scope) {
            return Some(code);
        }
        if let Some(code) = stop_code(world_mut(scope), oc) {
            return Some(code);
        }
        if let Some(code) = check_unhandled(scope) {
            return Some(code);
        }
    }
    None
}

fn run_timers(scope: &mut HandleScope) -> Option<i32> {
    loop {
        let now = world_mut(scope).clock.now_ms();
        let idx = {
            let world = world_mut(scope);
            world
                .timers
                .iter()
                .enumerate()
                .filter(|(_, t)| t.when_ms <= now)
                .min_by_key(|(_, t)| (t.when_ms, t.seq))
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
        if let Some(code) = stopped(scope) {
            return Some(code);
        }
        if let Some(code) = stop_code(world_mut(scope), oc) {
            return Some(code);
        }
        // V8 drains microtasks after each callback; the checkpoint decision
        // is made right here (Node timing), not once per loop turn.
        if let Some(code) = check_unhandled(scope) {
            return Some(code);
        }
        let world = world_mut(scope);
        if repeat && !world.running_timer.self_cleared() {
            // A fresh seq on reschedule: the repeat queues behind timers
            // registered while it ran, keeping one FIFO order for ties.
            timer.seq = world.next_timer_seq;
            world.next_timer_seq += 1;
            timer.when_ms = world.clock.deadline_after(interval_ms.max(1));
            world.timers.push(timer);
        }
    }
}

fn drain_microtasks(scope: &mut HandleScope) -> Option<i32> {
    scope.perform_microtask_checkpoint();
    stopped(scope).or_else(|| check_unhandled(scope))
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
/// missing a handler fails the run with 1. A guest exit code (vm.exit)
/// outranks the decision, matching Node's process.exit.
pub(crate) fn check_unhandled(scope: &mut HandleScope) -> Option<i32> {
    if world_mut(scope).exit_code() >= 0 {
        return None;
    }
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
            rejection_text(scope, val)
        }
        None => "unknown".to_string(),
    };
    eprintln!("v8vm: unhandled promise rejection: {}", text);
    Some(1)
}

pub(crate) fn event_loop(scope: &mut HandleScope) -> i32 {
    loop {
        if let Some(c) = stopped(scope) {
            return c;
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
        if stdin::poll_stdin(scope) {
            // Resolving a waiter runs its continuations; a rejection or a
            // termination raised there meets the same checkpoint check.
            if let Some(c) = drain_microtasks(scope) {
                return c;
            }
        }
        let (has_timers, has_net, has_stdin) = {
            let world = world_mut(scope);
            (
                !world.timers.is_empty(),
                !world.net.is_empty(),
                !world.stdin_waiters.is_empty(),
            )
        };
        match park_between_turns(scope, has_timers, has_net, has_stdin) {
            Park::Done => break,
            Park::Spin => continue,
            Park::Sleep(wait) => std::thread::sleep(wait),
        }
    }
    // Final sweep: continuations queued by the very last stdin delivery
    // still count, and a termination that landed as the loop wound down
    // (idle runs notice the watchdog only through the reason flag) ends
    // the run instead of falling through to a clean exit.
    scope.perform_microtask_checkpoint();
    if let Some(c) = stopped(scope).or_else(|| check_unhandled(scope)) {
        return c;
    }
    let code = world_mut(scope).exit_code();
    if code >= 0 {
        code
    } else {
        0
    }
}

/// What the loop does at a turn boundary.
enum Park {
    /// Nothing is pending: leave the loop.
    Done,
    /// Go straight to the next turn without sleeping.
    Spin,
    /// Sleep this long in real time.
    Sleep(Duration),
}

/// Decides the turn boundary: exit when nothing is pending, and otherwise
/// when (and whether) to wait. Real runs sleep until the next timer or the
/// stdin poll cadence. Virtual time never sleeps to wait for a timer:
/// runnable network work is delivered on the next turn as-is, and with no
/// runnable work left the clock jumps straight to the next due timer, so a
/// timer-heavy run finishes without real waiting. Only a lone pending
/// stdin read waits, in real time: chunks arrive on the real clock, the
/// read stays pending until one does, and the clock has no deadline to
/// jump to anyway.
fn park_between_turns(
    scope: &mut HandleScope,
    has_timers: bool,
    has_net: bool,
    has_stdin: bool,
) -> Park {
    if !has_timers && !has_net && !has_stdin {
        return Park::Done;
    }
    if world_mut(scope).clock.is_virtual() {
        if has_net {
            return Park::Spin;
        }
        if has_timers {
            let world = world_mut(scope);
            if let Some(next) = world.timers.iter().map(|t| t.when_ms).min() {
                world.clock.advance_to(next);
            }
            return Park::Spin;
        }
        return Park::Sleep(Duration::from_millis(2));
    }
    Park::Sleep(wake_deadline(scope, has_timers, has_stdin))
}

/// When to wake for the next loop turn: poll stdin at 2 ms while a reader
/// waits, else at most 50 ms, never past the earliest timer. The 50 ms
/// ceiling keeps an otherwise idle run responsive to the watchdog: a far
/// timer must not park the loop past the wall-clock timeout.
fn wake_deadline(scope: &mut HandleScope, has_timers: bool, has_stdin: bool) -> Duration {
    let cap = if has_stdin { 2 } else { 50 };
    let mut wake = Duration::from_millis(cap);
    if has_timers {
        let world = world_mut(scope);
        let now = world.clock.now_ms();
        // A deadline already reached (or saturated past u64) leaves the
        // poll cadence in charge.
        if let Some(till) = world
            .timers
            .iter()
            .map(|t| t.when_ms)
            .min()
            .unwrap_or(u64::MAX)
            .checked_sub(now)
        {
            let till = Duration::from_millis(till);
            if till < wake {
                wake = till;
            }
        }
    }
    wake
}

