//! Loudness measurement: the part that has nothing to do with plugins.
//!
//! Neta's whole job is telling you the truth about a mix when your ears have
//! stopped being able to — fatigue, the loud-is-better bias, three hours in.
//! That job is arithmetic on samples, and arithmetic does not need a host, a
//! window, or a plugin format. So it lives here, and the CLAP plugin is one
//! shell that wraps it. A standalone application would be another.
//!
//! Deciding that on day one is what makes the application a shell later
//! rather than a rewrite.
//!
//! # What goes in here
//!
//! The measurements are specified, not invented, which is the good news:
//!
//! - **Momentary, short-term and integrated loudness** — ITU-R BS.1770-4:
//!   K-weighting (a high-shelf stage and a high-pass stage), mean square per
//!   channel, channel weights, and the gating that EBU R128 layers on top
//!   (an absolute gate at -70 LUFS, then a relative gate 10 LU below the
//!   ungated level).
//! - **Loudness range (LRA)** — EBU Tech 3342, over the short-term history.
//! - **True peak** — BS.1770-4 annex 2: at least 4x oversampling before the
//!   peak is taken, because a sample peak under 0 dBFS says nothing about
//!   what the converter will do between samples.
//!
//! # How it gets checked
//!
//! The EBU publishes compliance material for exactly this (Tech 3341): test
//! signals with the reading each one must produce, to a stated tolerance.
//! That makes this crate testable the way `seco-dsp` is — against published
//! numbers, with no host and no ears involved. Any meter that cannot quote
//! its result on those signals is asking to be believed rather than checked.
//!
//! Nothing here allocates or locks: the plugin shell calls it from the audio
//! thread.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// A two-pole, two-zero filter section — the shape both K-weighting stages
/// take (BS.1770-4 tables 1 and 2).
///
/// Direct form I, which keeps the coefficients exactly as the specification
/// writes them. The state is four numbers, so a channel is cheap to own.
#[derive(Clone, Copy, Debug, Default)]
pub struct Biquad {
    /// Feed-forward coefficients, `[b0, b1, b2]`.
    b: [f64; 3],
    /// Feedback coefficients, `[a1, a2]`. `a0` is 1 by normalization.
    a: [f64; 2],
    /// Past inputs, most recent first.
    x: [f64; 2],
    /// Past outputs, most recent first.
    y: [f64; 2],
}

impl Biquad {
    /// A section from normalized coefficients (`a0 == 1`).
    pub fn new(b: [f64; 3], a: [f64; 2]) -> Self {
        Self { b, a, x: [0.0; 2], y: [0.0; 2] }
    }

    /// A section that passes everything through unchanged.
    pub fn passthrough() -> Self {
        Self::new([1.0, 0.0, 0.0], [0.0, 0.0])
    }

    /// Forgets the past. Call when the stream is discontinuous — a transport
    /// jump measures the new position, not a blend of two places.
    pub fn reset(&mut self) {
        self.x = [0.0; 2];
        self.y = [0.0; 2];
    }

    /// Filters one sample.
    pub fn process(&mut self, input: f64) -> f64 {
        let output = self.b[0] * input + self.b[1] * self.x[0] + self.b[2] * self.x[1]
            - self.a[0] * self.y[0]
            - self.a[1] * self.y[1];
        self.x = [input, self.x[0]];
        self.y = [output, self.y[0]];
        output
    }

    /// Gain at DC, from the coefficients alone — the cheapest way to catch a
    /// transcription error in a filter you have not run yet.
    pub fn dc_gain(&self) -> f64 {
        (self.b[0] + self.b[1] + self.b[2]) / (1.0 + self.a[0] + self.a[1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_returns_its_input() {
        let mut filter = Biquad::passthrough();
        for sample in [0.0, 1.0, -0.5, 0.25, -1.0] {
            assert_eq!(filter.process(sample), sample);
        }
        assert_eq!(filter.dc_gain(), 1.0);
    }

    /// Direct form I is a difference equation and nothing more; running an
    /// impulse through it must reproduce the coefficients in order.
    #[test]
    fn an_impulse_walks_out_the_numerator() {
        let mut filter = Biquad::new([0.5, -0.25, 0.125], [0.0, 0.0]);
        assert_eq!(filter.process(1.0), 0.5);
        assert_eq!(filter.process(0.0), -0.25);
        assert_eq!(filter.process(0.0), 0.125);
        assert_eq!(filter.process(0.0), 0.0);
    }

    /// Feedback has to actually feed back, or a filter with poles behaves
    /// like one without them and every reading downstream is wrong.
    #[test]
    fn feedback_reaches_the_output() {
        let mut filter = Biquad::new([1.0, 0.0, 0.0], [-0.5, 0.0]);
        assert_eq!(filter.process(1.0), 1.0);
        assert_eq!(filter.process(0.0), 0.5);
        assert_eq!(filter.process(0.0), 0.25);
    }

    /// A discontinuity in the stream must not be measured as a signal.
    #[test]
    fn reset_forgets_the_past() {
        let mut filter = Biquad::new([1.0, 0.0, 0.0], [-0.5, 0.0]);
        filter.process(1.0);
        filter.reset();
        assert_eq!(filter.process(0.0), 0.0);
    }
}
