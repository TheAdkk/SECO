/// One-pole lowpass smoother: `y += a · (x − y)`.
///
/// The classic declicker. Feeding it a target gain per sample turns steps —
/// parameter jumps, curve discontinuities, transport-resync jumps — into
/// exponential glides with time constant τ. After τ seconds the output has
/// covered ~63% of a step; after 5τ, ~99%.
#[derive(Clone, Copy, Debug)]
pub struct OnePole {
    value: f32,
    coeff: f32,
}

impl OnePole {
    /// A smoother resting at `initial` that snaps instantly (coefficient 1)
    /// until [`set_tau`](OnePole::set_tau) configures it.
    pub fn new(initial: f32) -> Self {
        Self { value: initial, coeff: 1.0 }
    }

    /// Configures the time constant `tau` (seconds) at `sample_rate`.
    /// `a = 1 − e^(−1/(τ·fs))`, the standard discretization.
    pub fn set_tau(&mut self, tau: f32, sample_rate: f32) {
        let samples = tau * sample_rate;
        self.coeff = if samples <= 1.0 { 1.0 } else { 1.0 - (-1.0 / samples).exp() };
    }

    /// Jumps the state to `value` with no glide (e.g. on `reset()`).
    pub fn snap_to(&mut self, value: f32) {
        self.value = value;
    }

    /// Advances one sample toward `target`, returning the smoothed value.
    pub fn process(&mut self, target: f32) -> f32 {
        self.value += self.coeff * (target - self.value);
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converges_to_target() {
        let mut smooth = OnePole::new(0.0);
        smooth.set_tau(0.002, 48_000.0);
        let mut last = 0.0;
        for _ in 0..(48_000 / 100) {
            last = smooth.process(1.0);
        }
        // 10 ms = 5τ: effectively settled.
        assert!(last > 0.99, "not settled: {last}");
    }

    #[test]
    fn tau_reaches_63_percent() {
        let mut smooth = OnePole::new(0.0);
        let fs = 48_000.0;
        smooth.set_tau(0.002, fs);
        let mut value = 0.0;
        for _ in 0..96 {
            // τ · fs = 96 samples
            value = smooth.process(1.0);
        }
        assert!((value - 0.632).abs() < 0.01, "expected ~63% after τ, got {value}");
    }

    #[test]
    fn monotonic_toward_target() {
        let mut smooth = OnePole::new(1.0);
        smooth.set_tau(0.002, 48_000.0);
        let mut previous = 1.0;
        for _ in 0..500 {
            let value = smooth.process(0.0);
            assert!(value <= previous);
            previous = value;
        }
    }
}
