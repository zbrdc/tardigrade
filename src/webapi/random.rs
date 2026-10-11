//! performance.now and the seeded Math.random. performance.now reports
//! monotonic milliseconds (std Instant) since the run started - the
//! origin is created with the run's World, so each run's timeline
//! starts near zero and stays comparable within the run.

use crate::guest_cb;
use crate::world::world_mut;

/// The splitmix64 increment: the golden-ratio constant that keeps
/// successive counter values well spread.
const GOLDEN_GAMMA: u64 = 0x9e3779b97f4a7c15;

fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}

/// One Math.random() draw for a seeded run: advance the counter, mix it,
/// and scale the top 53 bits to a uniform value in [0, 1). Returns the
/// next counter state with the draw so callers keep the state explicitly.
pub(crate) fn random_draw(state: u64) -> (u64, f64) {
    let next = state.wrapping_add(GOLDEN_GAMMA);
    let draw = (mix64(next) >> 11) as f64 * (1.0 / (1u64 << 53) as f64);
    (next, draw)
}

guest_cb!(performance_now, |_args, rv, scope| {
    let ms = world_mut(scope).perf_origin.elapsed().as_secs_f64() * 1000.0;
    rv.set(v8::Number::new(scope, ms).into());
});

guest_cb!(math_random_seeded, |_args, rv, scope| {
    // Registered only for seeded runs (see webapi::install); the guard
    // keeps this callback non-panicking if that pairing ever breaks.
    if let Some(state) = world_mut(scope).random_state.as_mut() {
        let (next, draw) = random_draw(*state);
        *state = next;
        rv.set(v8::Number::new(scope, draw).into());
    }
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix64_draws_are_reproducible_and_uniform_shaped() {
        // Pinned against canonical splitmix64 (independently computed):
        // any change to the mixer, the increment or the scaling breaks this.
        let pinned: Vec<f64> = (0..4)
            .scan(42u64, |s, _| {
                let (next, d) = random_draw(*s);
                *s = next;
                Some(d)
            })
            .collect();
        assert_eq!(
            pinned,
            vec![
                0.7415648787718233,
                0.1599103928769201,
                0.27860113025513866,
                0.34419071652363753
            ]
        );
        // Different seeds differ immediately.
        let (_, a) = random_draw(7);
        let (_, b) = random_draw(8);
        assert_ne!(a, b);
        // Range and integer-bit sanity over a stretch of draws.
        let mut s = 1u64;
        for _ in 0..10_000 {
            let (next, d) = random_draw(s);
            assert!((0.0..1.0).contains(&d));
            s = next;
        }
    }
}
