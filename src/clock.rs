//! The run's clock. Real runs measure a monotonic Instant from the start
//! of the run; virtual runs (--clock=virtual) hold an integer
//! millisecond timeline that starts at 0 and moves only when the event
//! loop jumps it to the next due timer. Date, performance.now and the
//! timers all read it through the World, so a virtual-time guest cannot
//! observe the host clock.

use std::time::Instant;

use crate::guest_cb;
use crate::world::world_mut;
use crate::Clock;

/// A run's time source.
pub(crate) enum RunClock {
    /// Monotonic real time; the zero point is the start of the run.
    Real(Instant),
    Virtual {
        /// Date.now()'s zero point: --epoch=<ms>, added to the timeline so
        /// Date values sit on the wall-clock timeline the guest asked for.
        epoch_ms: u64,
        /// Virtual milliseconds elapsed since the run started. Moves only
        /// through [RunClock::advance_to].
        now_ms: u64,
    },
}

impl RunClock {
    /// The current point on the run's timer timeline, in the timeline's
    /// own ticks: nanoseconds since the run started under Real (so a
    /// deadline can sit exactly `delay` away, neither rounded up to a
    /// millisecond nor truncated), whole milliseconds under Virtual.
    /// Timers, the loop's park decision and wake deadlines all use these
    /// ticks; ticks of one clock are only ever compared within a run.
    pub(crate) fn now(&self) -> u64 {
        match self {
            RunClock::Real(origin) => origin.elapsed().as_nanos() as u64,
            RunClock::Virtual { now_ms, .. } => *now_ms,
        }
    }

    /// A deadline `delay_ms` into the future on the same timeline: exact
    /// addition, so a timer never fires before its full delay (the +1 ns
    /// covers the sub-nanosecond fraction the clock floors away) and
    /// never pays rounding on top of the delay.
    pub(crate) fn deadline_after(&self, delay_ms: u64) -> u64 {
        match self {
            RunClock::Real(origin) => u64::try_from(
                origin
                    .elapsed()
                    .as_nanos()
                    .saturating_add(u128::from(delay_ms).saturating_mul(1_000_000))
                    + 1,
            )
            .unwrap_or(u64::MAX),
            RunClock::Virtual { now_ms, .. } => now_ms.saturating_add(delay_ms),
        }
    }

    /// performance.now(): elapsed ms since the run started. Virtual runs
    /// report the timeline, so it starts at exactly 0 and moves only with
    /// the virtual clock.
    pub(crate) fn perf_ms(&self) -> f64 {
        match self {
            RunClock::Real(origin) => origin.elapsed().as_secs_f64() * 1000.0,
            RunClock::Virtual { now_ms, .. } => *now_ms as f64,
        }
    }

    /// Date.now(): the virtual timeline shifted by the epoch, clamped at
    /// the ECMAScript maximum time value so a saturated timeline cannot
    /// hand the guest a number outside the Date range. Real runs never
    /// read this (they keep V8's own Date); the 0 guards the pairing
    /// instead of panicking inside a V8 callback.
    pub(crate) fn date_ms(&self) -> u64 {
        match self {
            RunClock::Virtual { epoch_ms, now_ms } => epoch_ms
                .saturating_add(*now_ms)
                .min(Clock::MAX_EPOCH_MS),
            RunClock::Real(_) => 0,
        }
    }

    /// True under --clock=virtual.
    pub(crate) fn is_virtual(&self) -> bool {
        matches!(self, RunClock::Virtual { .. })
    }

    /// Jumps the virtual clock to `ms`, never backwards: a timer scheduled
    /// into the past fires on the next loop turn at the newer time.
    pub(crate) fn advance_to(&mut self, ms: u64) {
        match self {
            RunClock::Virtual { now_ms, .. } => *now_ms = (*now_ms).max(ms),
            RunClock::Real(_) => {}
        }
    }
}

// The prelude's Date override reads this (epoch + elapsed virtual ms);
// webapi installs it only for virtual runs, and the prelude removes it
// from the global object after capturing it.
guest_cb!(virtual_date_now, |_args, rv, scope| {
    let ms = world_mut(scope).clock.date_ms();
    rv.set(v8::Number::new(scope, ms as f64).into());
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_virtual_clock_advances_only_through_advance_to() {
        let mut c = RunClock::Virtual { epoch_ms: 5, now_ms: 0 };
        assert_eq!(c.now(), 0);
        assert_eq!(c.perf_ms(), 0.0);
        assert_eq!(c.date_ms(), 5, "the epoch is the Date zero point");
        c.advance_to(300);
        assert_eq!((c.now(), c.perf_ms(), c.date_ms()), (300, 300.0, 305));
    }

    #[test]
    fn advance_to_never_moves_the_clock_backwards() {
        let mut c = RunClock::Virtual { epoch_ms: 0, now_ms: 100 };
        c.advance_to(40);
        assert_eq!(c.now(), 100, "a stale deadline must not rewind time");
    }

    #[test]
    fn the_epoch_is_added_with_saturation() {
        let mut c = RunClock::Virtual {
            epoch_ms: u64::MAX,
            now_ms: 0,
        };
        c.advance_to(10);
        assert_eq!(
            c.date_ms(),
            Clock::MAX_EPOCH_MS,
            "past the ECMAScript maximum the Date timeline clamps"
        );
        assert_eq!(c.now(), 10, "the timeline itself keeps counting");
    }

    #[test]
    fn real_deadlines_add_the_delay_exactly() {
        // Two properties, pinned without a millisecond timeline in the
        // way: the deadline is never before the scheduling instant plus
        // the delay (a timer cannot fire early), and it carries no
        // rounding on top (a hop does not pay an extra millisecond). The
        // nanosecond ticks cover both exactly; the +1 ns is the
        // floored-fraction allowance.
        let origin = std::time::Instant::now();
        let clock = RunClock::Real(origin);
        let before = origin.elapsed().as_nanos();
        let deadline = clock.deadline_after(5) as u128;
        let after = origin.elapsed().as_nanos();
        let delay_ns = 5_000_000u128;
        assert!(
            deadline + 1 >= before + delay_ns,
            "deadline {} does not cover {} + 5 ms",
            deadline,
            before
        );
        assert!(
            deadline <= after + delay_ns + 1,
            "deadline {} pads beyond {} + 5 ms",
            deadline,
            after
        );
    }

    #[test]
    fn virtual_deadlines_are_the_timeline_plus_the_delay() {
        let mut c = RunClock::Virtual { epoch_ms: 0, now_ms: 40 };
        assert_eq!(c.deadline_after(60), 100);
        c.advance_to(100);
        assert_eq!(c.deadline_after(0), 100, "zero delay keeps the clock");
    }

    #[test]
    fn a_real_clock_measures_elapsed_time() {
        let before = std::time::Instant::now();
        let c = RunClock::Real(before);
        assert!(c.now() < 1_000_000, "a fresh real clock is under a millisecond in");
        assert!(!c.is_virtual());
        std::thread::sleep(std::time::Duration::from_millis(15));
        // Real ticks are nanoseconds; 10 ms past the sleep is 10 ms of ns.
        assert!(c.now() >= 10_000_000, "roughly the slept time has passed");
        assert!(c.perf_ms() >= 10.0);
        assert_eq!(c.date_ms(), 0, "real runs never read the virtual Date");
        // Mutating a real clock is a no-op, so shared loop code stays safe.
        let mut c2 = RunClock::Real(std::time::Instant::now());
        c2.advance_to(1_000_000_000_000);
        assert!(c2.now() < 1_000_000_000_000);
    }
}
