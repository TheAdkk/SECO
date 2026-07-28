//! The shipped ducking shapes.
//!
//! # Why a shape is not just a curve
//!
//! The first model sampled the whole cycle into a table: gain 1.0 at the
//! beat, a fast dip, then a recovery. It closed the wrap seam, but it was
//! wrong twice over.
//!
//! - **The duck arrived after the beat.** The table sat at unity *at* phase
//!   0 and the dip took a slice of the cycle to land, so the loudest moment
//!   of the source — the transient the duck exists to make room for —
//!   passed at full level and was then cut. At 95 BPM, rate 1/2, that was
//!   ~5 ms above 0.9 gain followed by a 25 ms slam: heard as a tick on
//!   every beat, and visible as a spike in the host's waveform.
//! - **The entry time was a fraction of the cycle**, so it scaled with
//!   tempo and rate: the same shape faded over 25 ms at 1/2 and ~3 ms at
//!   1/16, and the fast end really did click.
//!
//! A shape is therefore stored as its *recovery* — 0.0 at a dip, rising to
//! 1.0 by the end of its segment — plus the phases where dips happen. The
//! entry into each dip is applied at play time as a fade of a fixed
//! duration in **seconds** ([`DuckShape::gain`]), so:
//!
//! - the floor lands exactly on the beat, never after it;
//! - the seam closes at 0.0 by construction, on both sides of every dip;
//! - the gain never moves faster than that fade, at any tempo or rate.

use core::f32::consts::PI;

use crate::CurveTable;

/// A cyclic ducking shape: what happens after each dip, and where the dips
/// are.
///
/// The cycle is cut at [`dips`](DuckShape::dips) (ascending, always starting
/// at 0.0 — the beat) and the recovery is stretched over each segment.
#[derive(Clone, Copy, Debug)]
pub struct DuckShape {
    /// Gain over one segment: `0.0` at the dip, `1.0` at the segment's end.
    recovery: CurveTable,
    /// Dip positions in the cycle, ascending, first is `0.0`.
    dips: &'static [f32],
}

impl DuckShape {
    /// Gain at `phase`, with the dip entry faded over `attack` — expressed
    /// in *cycles*, i.e. `attack_seconds / cycle_seconds`.
    ///
    /// The fade is what keeps this click-free: the shape's own recovery can
    /// be as abrupt as it likes, but the approach to every dip is a
    /// raised-cosine of a duration the caller controls in real time. It is
    /// clamped to half a segment so that a very fast rate cannot make the
    /// fade swallow the shape.
    ///
    /// The gain is 0.0 exactly at every dip, so a cycle boundary joins 0.0
    /// to 0.0: there is no wrap step to declick.
    pub fn gain(&self, phase: f32, attack: f32) -> f32 {
        let phase = phase.rem_euclid(1.0);
        let index = self.dips.iter().rposition(|dip| phase >= *dip).unwrap_or(0);
        let start = self.dips[index];
        let end = self.dips.get(index + 1).copied().unwrap_or(1.0);
        let span = (end - start).max(f32::EPSILON);

        let recovered = self.recovery.lookup((phase - start) / span);

        // Fade into the *next* dip. Multiplying keeps it continuous: the
        // factor is 1.0 until the window opens and 0.0 exactly at the dip.
        let window = attack.max(0.0).min(span * 0.5);
        let remaining = end - phase;
        if window <= 0.0 || remaining >= window {
            recovered
        } else {
            recovered * (0.5 - 0.5 * (PI * (remaining / window)).cos())
        }
    }

    /// The dip positions, for callers that need to draw or analyze them.
    pub fn dips(&self) -> &'static [f32] {
        self.dips
    }
}

/// One dip per cycle, on the beat.
fn single(recover: impl Fn(f32) -> f32) -> DuckShape {
    DuckShape { recovery: CurveTable::from_fn(|t| recover(t).clamp(0.0, 1.0)), dips: &[0.0] }
}

/// Several dips per cycle; the recovery is stretched over each segment.
fn repeated(dips: &'static [f32], recover: impl Fn(f32) -> f32) -> DuckShape {
    DuckShape { recovery: CurveTable::from_fn(|t| recover(t).clamp(0.0, 1.0)), dips }
}

/// Recovery helper: stay down until `hold`, then a raised-cosine climb
/// finishing at `end`, flat at 1.0 afterwards. Times are normalized to the
/// segment.
fn hold_then_rise(t: f32, hold: f32, end: f32) -> f32 {
    if t <= hold {
        0.0
    } else if t >= end {
        1.0
    } else {
        0.5 - 0.5 * (PI * (t - hold) / (end - hold)).cos()
    }
}

/// Pump: power-curve recovery across the whole cycle — the classic
/// sidechain feel.
pub fn pump() -> DuckShape {
    single(|t| t.powf(0.6))
}

/// Punch: back to full by about a third of the cycle.
pub fn punch() -> DuckShape {
    single(|t| (t / 0.34).min(1.0).powf(0.8))
}

/// Soft: raised-cosine recovery across the whole cycle — no corner at
/// either end.
pub fn soft() -> DuckShape {
    single(|t| 0.5 - 0.5 * (PI * t).cos())
}

/// Snap: recovered within a fifth of the cycle. The shortest duck here — a
/// tick of space rather than a groove.
pub fn snap() -> DuckShape {
    single(|t| (t / 0.18).min(1.0).powf(0.8))
}

/// Slope: dead-straight ramp recovering by half the cycle.
pub fn slope() -> DuckShape {
    single(|t| (t / 0.5).min(1.0))
}

/// Long: straight ramp across three quarters of the cycle.
pub fn long() -> DuckShape {
    single(|t| (t / 0.75).min(1.0))
}

/// Deep: convex recovery — stays down well past the midpoint and rushes the
/// last stretch. The most "sucked out" of the single-dip shapes.
pub fn deep() -> DuckShape {
    single(|t| t.powf(1.6))
}

/// Glide: the mirror of Deep — most of the level returns immediately, then
/// a long tail creeps back to unity.
pub fn glide() -> DuckShape {
    single(|t| t.powf(0.35))
}

/// Gate: silence held for a fifth of the cycle, then a quick rise. Chops
/// rather than pumps.
pub fn gate() -> DuckShape {
    single(|t| hold_then_rise(t, 0.19, 0.31))
}

/// Hold: Gate stretched — silent for nearly half the cycle before it
/// returns. Half-time chop.
pub fn hold() -> DuckShape {
    single(|t| hold_then_rise(t, 0.44, 0.64))
}

/// Step: recovers to a shelf a little above half, sits there, then finishes
/// the climb late in the cycle. Two levels per cycle.
pub fn step() -> DuckShape {
    single(|t| {
        if t < 0.3 {
            0.55 * (t / 0.3).powf(0.7)
        } else if t < 0.62 {
            0.55
        } else {
            0.55 + 0.45 * (0.5 - 0.5 * (PI * (t - 0.62) / 0.38).cos())
        }
    })
}

/// Double: two Pump-shaped ducks per cycle — eighth-note feel from a
/// quarter-note rate.
pub fn double() -> DuckShape {
    repeated(&[0.0, 0.5], |t| t.powf(0.6))
}

/// Triple: three ducks per cycle — triplet feel.
pub fn triple() -> DuckShape {
    repeated(&[0.0, 1.0 / 3.0, 2.0 / 3.0], |t| t.powf(0.6))
}

/// Quad: four ducks per cycle — sixteenth feel from a quarter-note rate.
pub fn quad() -> DuckShape {
    repeated(&[0.0, 0.25, 0.5, 0.75], |t| t.powf(0.6))
}

/// Swing: two ducks, the second late (at two thirds) — shuffle rather than
/// straight eighths.
pub fn swing() -> DuckShape {
    repeated(&[0.0, 2.0 / 3.0], |t| t.powf(0.6))
}

/// How many shapes ship.
pub const COUNT: usize = 15;

/// Display names, in [`tables`] order. This order is a compatibility
/// contract: the plugin exposes the shape as a stepped parameter whose value
/// is the index, so entries may be appended but never reordered or removed —
/// saved sessions store the number, not the name.
pub const NAMES: &[&str] = &[
    "Pump", "Punch", "Soft", "Snap", "Slope", "Long", "Deep", "Glide", "Gate", "Hold", "Step",
    "Double", "Triple", "Quad", "Swing",
];

/// The shipped shapes, in [`NAMES`] order. Built fresh (no shared state, no
/// allocation); a plugin holds its own copy.
pub fn tables() -> [DuckShape; COUNT] {
    [
        pump(),
        punch(),
        soft(),
        snap(),
        slope(),
        long(),
        deep(),
        glide(),
        gate(),
        hold(),
        step(),
        double(),
        triple(),
        quad(),
        swing(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A representative play-time fade: 5 ms of a 1-beat cycle at 120 BPM.
    const ATTACK: f32 = 0.005 / 0.5;

    fn shipped() -> impl Iterator<Item = (&'static str, DuckShape)> {
        NAMES.iter().copied().zip(tables())
    }

    /// The parameter that selects a shape is a stepped enum over `NAMES`,
    /// and its value indexes `tables()`: a mismatch would play a different
    /// shape than the one named.
    #[test]
    fn names_and_tables_line_up() {
        assert_eq!(NAMES.len(), tables().len());
    }

    /// The bug this model exists to kill: the floor must be *on* the beat,
    /// not a few milliseconds after it. A shape that is still near unity at
    /// phase 0 lets the transient through and then cuts it — heard as a
    /// tick on every beat.
    #[test]
    fn every_shape_is_down_on_the_beat() {
        for (name, shape) in shipped() {
            for dip in shape.dips() {
                let at_dip = shape.gain(*dip, ATTACK);
                assert!(at_dip < 1e-4, "{name}: gain {at_dip} at dip {dip}, not silent");
            }
        }
    }

    /// The cycle joins 0.0 to 0.0 at the wrap. Measured across the seam, not
    /// assumed: the previous model closed at 1.0 and needed the fast dip
    /// that caused the tick.
    #[test]
    fn shapes_close_the_seam_at_the_floor() {
        for (name, shape) in shipped() {
            let before = shape.gain(0.999_99, ATTACK);
            let after = shape.gain(0.0, ATTACK);
            assert!(
                (before - after).abs() < 1e-3,
                "{name}: |f(1-) - f(0)| = {}, the gain steps at every wrap",
                (before - after).abs()
            );
        }
    }

    /// The guarantee the phase-domain model could not make: whatever the
    /// tempo and rate, the gain moves no faster than the play-time fade.
    ///
    /// Worst case here is the fastest rate at a fast tempo — 1/16 at 200 BPM
    /// is a 75 ms cycle — sampled at 44.1 kHz. The bound is the peak slope
    /// of a raised cosine over the fade window (`pi/2 / samples`), with room
    /// for the shapes' own recoveries.
    #[test]
    fn gain_never_moves_faster_than_the_fade() {
        const SAMPLE_RATE: f32 = 44_100.0;
        const ATTACK_SECONDS: f32 = 0.005;
        let cycle_seconds = 60.0 / 200.0 * 0.25; // 1/16 at 200 BPM
        let attack = ATTACK_SECONDS / cycle_seconds;
        let samples = (cycle_seconds * SAMPLE_RATE) as usize;
        let bound = 2.0 * PI / 2.0 / (ATTACK_SECONDS * SAMPLE_RATE);

        for (name, shape) in shipped() {
            let mut previous = shape.gain(0.0, attack);
            let mut worst = 0.0_f32;
            for i in 1..=samples {
                let gain = shape.gain(i as f32 / samples as f32, attack);
                worst = worst.max((gain - previous).abs());
                previous = gain;
            }
            assert!(worst <= bound, "{name}: gain stepped {worst} per sample (bound {bound})");
        }
    }

    /// Closing the seam must not close the *duck*: the dip has to survive
    /// inside the cycle.
    #[test]
    fn shapes_still_duck() {
        for (name, shape) in shipped() {
            let min = (0..=1000)
                .map(|i| shape.gain(i as f32 / 1000.0, ATTACK))
                .fold(f32::MAX, f32::min);
            assert!(min < 0.05, "{name}: never dips (min gain {min})");
        }
    }

    /// And it must come back: a shape that never returns to unity is a
    /// volume knob, not a ducker.
    ///
    /// Not exactly 1.0: the fade into the next dip starts before the segment
    /// ends, so shapes with several dips per cycle top out a hair below —
    /// Quad reaches 0.976 (0.2 dB) at this fade width. That is the honest
    /// cost of a fixed-duration entry, not a bug to paper over.
    #[test]
    fn shapes_recover_to_unity() {
        for (name, shape) in shipped() {
            let max = (0..=1000)
                .map(|i| shape.gain(i as f32 / 1000.0, ATTACK))
                .fold(f32::MIN, f32::max);
            assert!(max > 0.95, "{name}: peaks at {max}, never fully recovers");
            assert!(max <= 1.0 + 1e-3, "{name}: peaks at {max}, above unity");
        }
    }

    /// Two shapes that measure the same are one shape with two names.
    #[test]
    fn every_shape_is_distinct() {
        let shapes = tables();
        for (i, a) in shapes.iter().enumerate() {
            for (j, b) in shapes.iter().enumerate().skip(i + 1) {
                let apart = (0..=256)
                    .map(|k| {
                        let phase = k as f32 / 256.0;
                        (a.gain(phase, ATTACK) - b.gain(phase, ATTACK)).abs()
                    })
                    .fold(0.0_f32, f32::max);
                assert!(apart > 0.05, "{} and {} are the same shape", NAMES[i], NAMES[j]);
            }
        }
    }

    /// A fade wider than the segment would erase the shape instead of
    /// declicking it; it is clamped, and the dip still lands at zero.
    #[test]
    fn an_absurd_fade_is_clamped_not_obeyed() {
        let shape = quad(); // shortest segments: a quarter cycle each
        let recovered = (0..=1000)
            .map(|i| shape.gain(i as f32 / 1000.0, 10.0))
            .fold(f32::MIN, f32::max);
        assert!(recovered > 0.5, "a huge fade flattened the shape to {recovered}");
        assert!(shape.gain(0.0, 10.0) < 1e-4);
    }
}
