//! The run's clock. Real runs measure a monotonic Instant from the start
//! of the run; virtual runs (--clock=virtual) hold an integer
//! millisecond timeline that starts at 0 and moves only when the event
//! loop jumps it to the next due timer. Date, performance.now and the
//! timers all read it through the World, so a virtual-time guest cannot
//! observe the host clock.

use std::time::Instant;

use crate::guest_cb;
use crate::world::world_mut;

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
    /// Milliseconds elapsed on the run's timeline: the timers' timeline
    /// and performance.now()'s value.
    pub(crate) fn now_ms(&self) -> u64 {
        match self {
            RunClock::Real(origin) => origin.elapsed().as_millis() as u64,
            RunClock::Virtual { now_ms, .. } => *now_ms,
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

    /// Date.now(): the virtual timeline shifted by the epoch. Real runs
    /// never read this (they keep V8's own Date); the 0 guards the
    /// pairing instead of panicking inside a V8 callback.
    pub(crate) fn date_ms(&self) -> u64 {
        match self {
            RunClock::Virtual { epoch_ms, now_ms } => epoch_ms.saturating_add(*now_ms),
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
        assert_eq!(c.now_ms(), 0);
        assert_eq!(c.perf_ms(), 0.0);
        assert_eq!(c.date_ms(), 5, "the epoch is the Date zero point");
        c.advance_to(300);
        assert_eq!((c.now_ms(), c.perf_ms(), c.date_ms()), (300, 300.0, 305));
    }

    #[test]
    fn advance_to_never_moves_the_clock_backwards() {
        let mut c = RunClock::Virtual { epoch_ms: 0, now_ms: 100 };
        c.advance_to(40);
        assert_eq!(c.now_ms(), 100, "a stale deadline must not rewind time");
    }

    #[test]
    fn the_epoch_is_added_with_saturation() {
        let mut c = RunClock::Virtual {
            epoch_ms: u64::MAX,
            now_ms: 0,
        };
        c.advance_to(10);
        assert_eq!(c.date_ms(), u64::MAX, "no overflow past the epoch");
        assert_eq!(c.now_ms(), 10, "the timeline itself keeps counting");
    }

    #[test]
    fn a_real_clock_measures_elapsed_time() {
        let before = std::time::Instant::now();
        let c = RunClock::Real(before);
        assert_eq!(c.now_ms(), 0, "a fresh real clock starts at zero");
        assert!(!c.is_virtual());
        std::thread::sleep(std::time::Duration::from_millis(15));
        assert!(c.now_ms() >= 10, "roughly the slept time has passed");
        assert!(c.perf_ms() >= 10.0);
        assert_eq!(c.date_ms(), 0, "real runs never read the virtual Date");
        // Mutating a real clock is a no-op, so shared loop code stays safe.
        let mut c2 = RunClock::Real(std::time::Instant::now());
        c2.advance_to(1000);
        assert!(c2.now_ms() < 1000);
    }
}
