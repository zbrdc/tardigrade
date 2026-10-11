//! setTimeout/setInterval/clearTimeout/clearInterval.

use std::time::{Duration, Instant};

use crate::diag::throw;
use crate::guest_cb;
use crate::world::{world_mut, Timer};

/// Bookkeeping for the one timer whose callback is on the stack: a
/// clearTimeout from inside its own callback must stop a repeating timer
/// from rescheduling. Unlike a list of cleared ids, this cannot grow with
/// the number of clear calls.
pub(crate) struct RunningTimer {
    id: Option<u64>,
    cleared: bool,
}

impl RunningTimer {
    pub(crate) fn new() -> Self {
        RunningTimer { id: None, cleared: false }
    }

    /// Timer `id` starts its callback run, initially not self-cleared.
    pub(crate) fn enter(&mut self, id: u64) {
        self.id = Some(id);
        self.cleared = false;
    }

    /// The callback returned; no id matches until the next enter().
    pub(crate) fn exit(&mut self) {
        self.id = None;
    }

    /// Records a clearTimeout; true when it cleared the running timer.
    pub(crate) fn clear(&mut self, id: u64) -> bool {
        let hit = self.id == Some(id);
        self.cleared |= hit;
        hit
    }

    /// Whether the running timer cleared itself; the decision is consumed
    /// once per callback.
    pub(crate) fn self_cleared(&mut self) -> bool {
        std::mem::take(&mut self.cleared)
    }
}

fn set_timer_common(
    args: &v8::FunctionCallbackArguments,
    rv: &mut v8::ReturnValue,
    scope: &mut v8::HandleScope,
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
    // Extra arguments are captured guest-side by the prelude wrapper, so the
    // host only ever stores the callback itself.
    let f: v8::Local<v8::Function> = args.get(0).try_into().unwrap();
    let cb = v8::Global::new(scope, f);
    let id = {
        let world = world_mut(scope);
        let id = world.next_timer_id;
        world.next_timer_id += 1;
        world.timers.push(Timer {
            id,
            repeat,
            interval_ms,
            when: Instant::now() + Duration::from_millis(interval_ms.max(1)),
            cb,
        });
        id
    };
    rv.set(v8::Number::new(scope, id as f64).into());
}

guest_cb!(set_timeout, |args, rv, scope| {
    set_timer_common(&args, &mut rv, scope, false);
});

guest_cb!(set_interval, |args, rv, scope| {
    set_timer_common(&args, &mut rv, scope, true);
});

guest_cb!(clear_timer, |args, _rv, scope| {
    if args.length() == 0 {
        return;
    }
    let id = args.get(0).number_value(scope).unwrap_or(-1.0);
    let world = world_mut(scope);
    world.timers.retain(|t| t.id as f64 != id);
    world.running_timer.clear(id as u64);
});

#[cfg(test)]
mod tests {
    use super::RunningTimer;

    #[test]
    fn clearing_the_running_timer_from_inside_its_callback_sticks() {
        let mut rt = RunningTimer::new();
        rt.enter(7);
        assert!(rt.clear(7));
        assert!(rt.self_cleared(), "the running timer cleared itself");
        assert!(!rt.self_cleared(), "the decision is consumed once");
    }

    #[test]
    fn clearing_other_timers_does_not_touch_the_running_one() {
        let mut rt = RunningTimer::new();
        rt.enter(7);
        assert!(!rt.clear(8));
        assert!(!rt.self_cleared());
    }

    #[test]
    fn nothing_matches_between_callbacks() {
        let mut rt = RunningTimer::new();
        rt.enter(7);
        rt.clear(8);
        rt.exit();
        assert!(!rt.clear(7), "no timer is on the stack between callbacks");
        assert!(!rt.self_cleared());
    }

    #[test]
    fn each_callback_starts_uncleared() {
        let mut rt = RunningTimer::new();
        rt.enter(1);
        rt.clear(1);
        rt.exit();
        rt.enter(2);
        assert!(!rt.self_cleared());
    }

    #[test]
    fn a_self_clear_survives_exit_until_it_is_read() {
        let mut rt = RunningTimer::new();
        rt.enter(7);
        rt.clear(7);
        rt.exit();
        assert!(rt.self_cleared(), "run_timers reads the decision after exit");
    }
}
