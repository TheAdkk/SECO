/// A slew-rate limiter: follows its target exactly while the target moves
/// slowly enough, and caps the speed when it does not.
///
/// This is the right shape of tool for a gain that is *already* drawn to be
/// click-free. A one-pole smoother cannot tell "the curve is doing its job"
/// from "something jumped": it lags everything by roughly its time
/// constant, so a duck aimed exactly at the beat lands a couple of
/// milliseconds late. A slew limiter is transparent below its rate — the
/// curve arrives on time — and only intervenes on the discontinuities that
/// actually click: parameter flips, transport jumps, preset changes.
#[derive(Clone, Copy, Debug)]
pub struct Slew {
    value: f32,
    max_step: f32,
}

impl Slew {
    /// Starts at `value` with no limiting (set a rate before use).
    pub fn new(value: f32) -> Self {
        Self { value, max_step: f32::INFINITY }
    }

    /// Caps the change to `per_second` units of value per second.
    pub fn set_max_rate(&mut self, per_second: f32, sample_rate: f32) {
        self.max_step =
            if sample_rate > 0.0 { (per_second / sample_rate).max(0.0) } else { f32::INFINITY };
    }

    /// The current value, without advancing.
    pub fn value(&self) -> f32 {
        self.value
    }

    /// Jumps straight to `value`, ignoring the limit. For fresh streams
    /// only: there is no previous output to click against.
    pub fn snap_to(&mut self, value: f32) {
        self.value = value;
    }

    /// Advances one sample toward `target` and returns the new value.
    pub fn process(&mut self, target: f32) -> f32 {
        let delta = target - self.value;
        self.value += delta.clamp(-self.max_step, self.max_step);
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Below the limit the output *is* the target — no lag, no smoothing.
    /// This is the property a one-pole cannot offer, and the reason the
    /// duck now lands on the beat.
    #[test]
    fn follows_a_slow_target_exactly() {
        let mut slew = Slew::new(1.0);
        slew.set_max_rate(200.0, 48_000.0); // 1/240 per sample
        let mut value = 1.0_f32;
        for i in 0..100 {
            let target = 1.0 - i as f32 / 1000.0; // 1/1000 per sample
            value = slew.process(target);
            assert_eq!(value, target, "lagged at step {i}");
        }
        assert!(value < 1.0);
    }

    /// A step is turned into a ramp at exactly the configured rate.
    #[test]
    fn caps_a_jump_at_the_rate() {
        const SAMPLE_RATE: f32 = 48_000.0;
        let mut slew = Slew::new(1.0);
        slew.set_max_rate(200.0, SAMPLE_RATE); // full scale in 5 ms
        let mut previous = 1.0;
        let mut samples = 0;
        while slew.value() > 0.0001 {
            let value = slew.process(0.0);
            assert!(previous - value <= 200.0 / SAMPLE_RATE + 1e-6, "stepped too far");
            previous = value;
            samples += 1;
            assert!(samples < 1000, "never arrived");
        }
        let seconds = samples as f32 / SAMPLE_RATE;
        assert!((seconds - 0.005).abs() < 1e-4, "took {seconds}s to cross, expected 5 ms");
    }

    /// Arrival is exact, not asymptotic: a limiter that overshoots would
    /// oscillate around the target forever.
    #[test]
    fn arrives_without_overshooting() {
        let mut slew = Slew::new(0.0);
        slew.set_max_rate(1000.0, 48_000.0);
        for _ in 0..1000 {
            slew.process(0.5);
        }
        assert_eq!(slew.value(), 0.5);
    }

    /// A fresh stream has nothing to click against.
    #[test]
    fn snap_ignores_the_limit() {
        let mut slew = Slew::new(1.0);
        slew.set_max_rate(1.0, 48_000.0);
        slew.snap_to(0.0);
        assert_eq!(slew.value(), 0.0);
    }
}
