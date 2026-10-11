//! setTimeout/setInterval/clearTimeout/clearInterval.

use std::time::{Duration, Instant};

use crate::diag::throw;
use crate::guest_cb;
use crate::world::{world_mut, Timer};

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
    world.cleared.push(id as u64);
});
