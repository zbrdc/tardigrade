//! The guest event loop: network generations, timers, microtasks, stdin.
//! Keeps the process alive while any guest work is pending.

use std::time::Duration;

use crate::diag::{rejection_text, report};
use crate::netcap;
use crate::stdin;
use crate::world::{world_mut, NetMsg, World};
use v8::PinScope;

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
pub(crate) fn stopped(scope: &mut PinScope<'_, '_>) -> Option<i32> {
    let pending = world_mut(scope).term.reason().is_some();
    if !pending && !scope.as_mut().is_execution_terminating() {
        return None;
    }
    if scope.as_mut().is_execution_terminating() {
        scope.as_mut().cancel_terminate_execution();
    }
    Some(world_mut(scope).take_terminated_exit())
}

fn call_fn(scope: &mut PinScope<'_, '_>, g: &v8::Global<v8::Function>) -> Outcome {
    v8::tc_scope!(let tc, scope);
    let this = v8::undefined(tc).into();
    // SAFETY: the global was rooted in this run's isolate; opening it in
    // the current scope is the documented use of the unsafe Global::open.
    let result = unsafe { g.open(tc) }.call(tc, this, &[]);
    if result.is_some() {
        return Outcome::Ok;
    }
    if tc.as_mut().is_execution_terminating() {
        tc.as_mut().cancel_terminate_execution();
        return Outcome::Terminated;
    }
    if world_mut(tc).exception_decides() {
        report(tc);
    }
    Outcome::Exception
}

fn run_net(scope: &mut PinScope<'_, '_>) -> Option<i32> {
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
        let oc = {
            v8::tc_scope!(let tc, scope);
            let this = v8::undefined(tc).into();
            let argv = [payload.into(), from.into()];
            // SAFETY: the listener global belongs to this isolate.
            let result = unsafe { listener.open(tc) }.call(tc, this, &argv);
            if result.is_some() {
                Outcome::Ok
            } else if tc.as_mut().is_execution_terminating() {
                tc.as_mut().cancel_terminate_execution();
                Outcome::Terminated
            } else {
                if world_mut(tc).exception_decides() {
                    report(tc);
                }
                Outcome::Exception
            }
        };
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

fn run_timers(scope: &mut PinScope<'_, '_>) -> Option<i32> {
    loop {
        let now = world_mut(scope).clock.now();
        let idx = {
            let world = world_mut(scope);
            world
                .timers
                .iter()
                .enumerate()
                .filter(|(_, t)| t.when <= now)
                .min_by_key(|(_, t)| (t.when, t.seq))
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
            timer.when = world.clock.deadline_after(interval_ms.max(1));
            world.timers.push(timer);
        }
    }
}

fn drain_microtasks(scope: &mut PinScope<'_, '_>) -> Option<i32> {
    scope.perform_microtask_checkpoint();
    stopped(scope).or_else(|| check_unhandled(scope))
}

// ---- unhandled rejections ----

/// Drops the remembered rejection for `promise`, as an attached handler
/// would: the host itself consumed the rejection (the serve console
/// answers with it), so it must not fail the run at the next checkpoint.
pub(crate) fn forget_rejection_of(scope: &mut v8::Isolate, promise: &v8::Promise) {
    let hash = promise.get_identity_hash().get();
    // Bucketed by identity hash so a mass catch does not scan every
    // remembered rejection; identity is confirmed with == inside the
    // bucket (hashes are stable but not unique).
    let mut bucket = match world_mut(scope).pending_rejections.remove(&hash) {
        Some(b) => b,
        None => return,
    };
    // SAFETY: the remembered globals belong to this isolate.
    bucket.retain(|e| unsafe { e.promise.open(scope) } != promise);
    let world = world_mut(scope);
    if !bucket.is_empty() {
        world.pending_rejections.insert(hash, bucket);
    }
}

/// Removes the remembered rejection for `msg`'s promise, if any: a handler
/// attached before the next checkpoint un-fails the run.
fn forget_rejection(scope: &mut v8::PinScope<'_, '_>, msg: &v8::PromiseRejectMessage) {
    forget_rejection_of(scope, &msg.get_promise());
}

pub(crate) extern "C" fn promise_reject_callback(msg: v8::PromiseRejectMessage) {
    use v8::PromiseRejectEvent::*;
    // The callback runs inside V8's own scope; opening a second HandleScope
    // on the isolate would panic ("active scope can't be dropped") and abort.
    let storage = std::pin::pin!(unsafe { v8::CallbackScope::new(&msg) });
    let mut scope = storage.init();
    let scope = &mut scope;
    match msg.get_event() {
        PromiseRejectWithNoHandler => {
            let promise = v8::Global::new(scope, msg.get_promise());
            let value = msg.get_value().map(|v| v8::Global::new(scope, v));
            let hash = msg.get_promise().get_identity_hash().get();
            let world = world_mut(scope);
            let seq = world.next_rejection_seq;
            world.next_rejection_seq += 1;
            world.pending_rejections.entry(hash).or_default().push(
                crate::world::PendingRejection {
                    seq,
                    promise,
                    value,
                },
            );
        }
        PromiseHandlerAddedAfterReject => forget_rejection(scope, &msg),
        _ => {}
    }
}

/// After a microtask checkpoint: the first (lowest seq) rejection still
/// missing a handler fails the run with 1. A guest exit code (vm.exit)
/// outranks the decision, matching Node's process.exit.
pub(crate) fn check_unhandled(scope: &mut PinScope<'_, '_>) -> Option<i32> {
    if world_mut(scope).has_exit() {
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
            // SAFETY: the rejection value global belongs to this isolate.
            let val = unsafe { v.open(scope) };
            rejection_text(scope, val)
        }
        None => "unknown".to_string(),
    };
    eprintln!("tardigrade: unhandled promise rejection: {}", text);
    Some(1)
}

pub(crate) fn event_loop(scope: &mut PinScope<'_, '_>) -> i32 {
    event_loop_until(scope, &mut |_| false)
}

/// The loop with a stop condition: it returns 0 as soon as `done` is
/// satisfied (checked at each turn and after each microtask drain), so a
/// caller waiting for one promise does not also wait out every other
/// pending job. Leftover work stays scheduled for the next caller,
/// exactly as an idle return would leave it.
pub(crate) fn event_loop_until(
    scope: &mut PinScope<'_, '_>,
    done: &mut dyn FnMut(&mut PinScope<'_, '_>) -> bool,
) -> i32 {
    loop {
        // Snapshot the wake generation before the turn: work signalled
        // from another thread (stdin chunk, network completion) while
        // this turn runs then makes the park below return at once
        // instead of sleeping through its timeout.
        let wake = world_mut(scope).loop_wake.clone();
        let seen = wake.generation();
        // The turn first, and the park only when the loop continues: a
        // tuple match would evaluate park_between_turns even for a
        // finished loop, and under --clock=virtual that jumps the clock
        // to the next pending timer - past work the caller never waited
        // for (an awaited promise settling while a far timer is queued
        // would leave the clock at the far timer's time).
        if let Some(c) = turn(scope, done) {
            return c;
        }
        match park_between_turns(scope) {
            Park::Done => break,
            Park::Spin => continue,
            Park::Sleep(wait) => wake.wait(seen, wait),
        }
    }
    sweep(scope)
}

/// One loop turn: due network messages, due timers, microtasks, stdin.
/// Some(code) ends the loop (a stop condition met, an exit decided);
/// None means the turn ran and the caller decides whether to park.
fn turn(
    scope: &mut PinScope<'_, '_>,
    done: &mut dyn FnMut(&mut PinScope<'_, '_>) -> bool,
) -> Option<i32> {
    if done(scope) {
        return Some(0);
    }
    if let Some(c) = stopped(scope) {
        return Some(c);
    }
    if let Some(c) = run_net(scope) {
        return Some(c);
    }
    if let Some(c) = run_timers(scope) {
        return Some(c);
    }
    if let Some(c) = drain_microtasks(scope) {
        return Some(c);
    }
    if done(scope) {
        return Some(0);
    }
    if stdin::poll_stdin(scope) {
        // Resolving a waiter runs its continuations; a rejection or a
        // termination raised there meets the same checkpoint check.
        if let Some(c) = drain_microtasks(scope) {
            return Some(c);
        }
    }
    if netcap::poll_net(scope) {
        // A settled request runs the same continuations-and-checkpoints
        // dance as a stdin delivery.
        if let Some(c) = drain_microtasks(scope) {
            return Some(c);
        }
    }
    None
}

/// The final sweep after the loop goes idle: continuations queued by the
/// very last stdin delivery still count, a termination that landed as the
/// loop wound down (idle runs notice the watchdog only through the reason
/// flag) ends the run, and an unhandled rejection or a decided exit code
/// conclude it.
fn sweep(scope: &mut PinScope<'_, '_>) -> i32 {
    scope.perform_microtask_checkpoint();
    if let Some(c) = stopped(scope).or_else(|| check_unhandled(scope)) {
        return c;
    }
    let code = world_mut(scope).exit_code();
    if world_mut(scope).has_exit() {
        code
    } else {
        0
    }
}

/// Settles only work already due at the current point of the timeline:
/// due timers, one generation of queued network messages, microtasks,
/// stdin. The clock is never advanced, so under --clock=virtual a later
/// timer stays queued instead of being jumped to - a serve command's
/// post-reply settling uses this, where jumping to a leftover repeating
/// timer would spin virtual time (and the CPU) without bound.
pub(crate) fn settle_due(scope: &mut PinScope<'_, '_>) -> i32 {
    loop {
        let wake = world_mut(scope).loop_wake.clone();
        let seen = wake.generation();
        if let Some(c) = turn(scope, &mut |_| false) {
            return c;
        }
        // Work that became due during this turn still belongs to "now"
        // (a listener answering into the net queue, or a timer whose
        // deadline was reached while the clock stood still); anything
        // later waits for whoever moves the clock. Zero-delay timers do
        // not qualify: their deadlines are clamped to at least 1 ms, so
        // they are never due at the instant they are scheduled.
        let due = {
            let world = world_mut(scope);
            let now = world.clock.now();
            world.timers.iter().any(|t| t.when <= now) || !world.net.is_empty()
        };
        if !due {
            if world_mut(scope).net_pending.is_empty() {
                break;
            }
            // An in-flight network request completes on a real thread
            // and signals the loop wake; wait for that (50 ms cap keeps
            // deadline checks in the caller responsive).
            wake.wait(seen, Duration::from_millis(50));
            continue;
        }
    }
    sweep(scope)
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
/// when (and whether) to wait. Reads the pending flags itself - the
/// flags live in the World, so the decision belongs with the policy.
/// Real runs sleep until the next timer or the
/// stdin poll cadence. Virtual time never sleeps to wait for a timer:
/// runnable network work is delivered on the next turn as-is, and with no
/// runnable work left the clock jumps straight to the next due timer, so a
/// timer-heavy run finishes without real waiting. Only a lone pending
/// stdin read waits, in real time: chunks arrive on the real clock, the
/// read stays pending until one does, and the clock has no deadline to
/// jump to anyway.
fn park_between_turns(scope: &mut PinScope<'_, '_>) -> Park {
    let (has_timers, has_net, has_stdin, has_netreq) = {
        let world = world_mut(scope);
        (
            !world.timers.is_empty(),
            !world.net.is_empty(),
            !world.stdin_waiters.is_empty(),
            !world.net_pending.is_empty(),
        )
    };
    if !has_timers && !has_net && !has_stdin && !has_netreq {
        return Park::Done;
    }
    // Queued vm.net messages are already in this process and are
    // delivered by the next turn: sleeping here would add up to 50 ms
    // of pure latency per hop (the old poll). Spin instead — a
    // self-sustaining ping-pong then runs at full speed (and full CPU,
    // bounded by --timeout), exactly as under the virtual clock.
    if has_net {
        return Park::Spin;
    }
    if world_mut(scope).clock.is_virtual() {
        if has_netreq {
            // The request rides a real worker thread and signals the
            // loop wake; virtual time stands still while it is in flight.
            return Park::Sleep(Duration::from_millis(50));
        }
        if has_timers {
            let world = world_mut(scope);
            if let Some(next) = world.timers.iter().map(|t| t.when).min() {
                world.clock.advance_to(next);
            }
            return Park::Spin;
        }
        return Park::Sleep(Duration::from_millis(50));
    }
    Park::Sleep(wake_deadline(scope, has_timers))
}

/// How long the loop may wait before the next turn: until the earliest
/// timer, at most 50 ms. Cross-thread work (stdin chunks, network
/// completions) wakes the wait early through the loop wake, so this is
/// only a ceiling — it also keeps an otherwise idle run responsive to
/// the watchdog: a far timer must not park the loop past the wall-clock
/// timeout.
fn wake_deadline(scope: &mut PinScope<'_, '_>, has_timers: bool) -> Duration {
    let mut wake = Duration::from_millis(50);
    if has_timers {
        let world = world_mut(scope);
        let now = world.clock.now();
        // A deadline already reached (or saturated past u64) leaves the
        // poll cadence in charge. Ticks are nanoseconds in real mode,
        // the only mode that sleeps to a timer.
        if let Some(till) = world
            .timers
            .iter()
            .map(|t| t.when)
            .min()
            .unwrap_or(u64::MAX)
            .checked_sub(now)
        {
            let till = Duration::from_nanos(till);
            if till < wake {
                wake = till;
            }
        }
    }
    wake
}
