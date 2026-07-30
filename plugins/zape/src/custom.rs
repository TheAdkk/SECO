//! The user-drawn curve: the 16th shape.
//!
//! Stored as control points rather than a sampled table. Sixteen numbers
//! survive a session file, parse on the audio thread without allocating,
//! and are what the editor actually manipulates; a 257-point table would be
//! none of those things.
//!
//! The wire format is the plain text the editor sends and `clap.state`
//! keeps verbatim: comma-separated values in `[0, 1]`, one per control
//! point. The framework never reads it (see `seco-clap`'s `plugin_state`);
//! this module is the only place that knows what those bytes mean.

use seco_dsp::{CurveTable, DuckShape};

/// Control points across one cycle, the first sitting on the beat.
///
/// Sixteen is a compromise the drawing has to live with: enough to shape a
/// groove, few enough that each handle stays grabbable in a 700 px display.
pub(crate) const POINTS: usize = 16;

/// A drawn recovery: `points[i]` is the gain at phase `i / POINTS`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CustomCurve {
    points: [f32; POINTS],
}

impl Default for CustomCurve {
    /// A straight ramp out of silence — recognizably a duck, and obviously
    /// editable, which a copy of Pump would not be.
    fn default() -> Self {
        let mut points = [0.0; POINTS];
        for (index, point) in points.iter_mut().enumerate() {
            *point = index as f32 / (POINTS - 1) as f32;
        }
        Self { points }
    }
}

impl CustomCurve {
    /// Reads a block written by the editor or restored from a session.
    ///
    /// Runs on the audio thread (`Plugin::apply_state`), so it allocates
    /// nothing: `split` and `parse` both work straight off the borrowed
    /// bytes. Anything unreadable — wrong length, junk, invalid UTF-8, an
    /// empty block — is the default curve rather than a failure: a session
    /// that cannot be understood must still play.
    pub(crate) fn parse(bytes: &[u8]) -> Self {
        let Ok(text) = core::str::from_utf8(bytes) else {
            return Self::default();
        };
        let mut points = [0.0_f32; POINTS];
        let mut count = 0;
        for field in text.split(',') {
            if count == POINTS {
                // More points than we store: a newer version of this format
                // rather than something to guess at.
                return Self::default();
            }
            let Ok(value) = field.trim().parse::<f32>() else {
                return Self::default();
            };
            if !value.is_finite() {
                return Self::default();
            }
            points[count] = value.clamp(0.0, 1.0);
            count += 1;
        }
        if count != POINTS {
            return Self::default();
        }
        Self::pinned(points)
    }

    /// The invariant the drawing cannot be trusted with: the first point is
    /// the beat, and the beat is silent.
    ///
    /// The editor pins its first handle too, but a state block is just
    /// bytes — from an older version, a hand-edited project file, another
    /// plugin's preset. A curve that starts at 0.8 would jump the gain at
    /// every cycle boundary; the slew limiter would ramp it rather than
    /// click, but the duck would simply not be a duck.
    fn pinned(mut points: [f32; POINTS]) -> Self {
        points[0] = 0.0;
        Self { points }
    }

    /// The control points. The editor works from [`to_wire`](Self::to_wire),
    /// so this is only how the tests look inside.
    #[cfg(test)]
    fn points(&self) -> &[f32; POINTS] {
        &self.points
    }

    /// The wire form: what the editor sends and `clap.state` stores.
    pub(crate) fn to_wire(self) -> String {
        let mut text = String::with_capacity(POINTS * 6);
        for (index, point) in self.points.iter().enumerate() {
            if index > 0 {
                text.push(',');
            }
            text.push_str(&format!("{point:.3}"));
        }
        text
    }

    /// Samples the drawn points into a playable shape.
    ///
    /// Cosine interpolation between control points, not linear: a corner in
    /// the gain is a corner in the audio, and with sixteen points the
    /// corners would land on every sixteenth of the cycle. Called from the
    /// audio thread — `CurveTable` is a fixed array, so nothing allocates.
    pub(crate) fn shape(&self) -> DuckShape {
        let points = self.points;
        DuckShape::from_recovery(CurveTable::from_fn(move |phase| {
            let scaled = phase.clamp(0.0, 1.0) * (POINTS - 1) as f32;
            let index = (scaled as usize).min(POINTS - 2);
            let fraction = (scaled - index as f32).clamp(0.0, 1.0);
            // Raised cosine: flat where the handles are, steep between them.
            let blend = 0.5 - 0.5 * (core::f32::consts::PI * fraction).cos();
            points[index] + (points[index + 1] - points[index]) * blend
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 5 ms of a one-beat cycle at 120 BPM, as in the plugin's tests.
    const ATTACK: f32 = 0.01;

    #[test]
    fn the_default_is_a_duck() {
        let shape = CustomCurve::default().shape();
        assert!(
            shape.gain(0.0, ATTACK) < 1e-4,
            "the default must start silent"
        );
        assert!(shape.gain(0.9, ATTACK) > 0.8, "and recover");
    }

    #[test]
    fn a_drawn_curve_round_trips_through_the_wire() {
        let mut points = [0.0_f32; POINTS];
        for (index, point) in points.iter_mut().enumerate() {
            *point = (index as f32 * 0.37).fract();
        }
        let drawn = CustomCurve::pinned(points);
        let parsed = CustomCurve::parse(drawn.to_wire().as_bytes());
        for (a, b) in drawn.points().iter().zip(parsed.points()) {
            assert!((a - b).abs() < 1e-3, "{a} came back as {b}");
        }
    }

    /// The block is bytes from anywhere — a session file, an older build,
    /// something hand-edited. None of it may panic or play something wild.
    #[test]
    fn junk_reads_as_the_default_curve() {
        let default = CustomCurve::default();
        for block in [
            &b""[..],
            b"nope",
            b"0.1,0.2",                                             // too few
            b"0,0.1,0.2,0.3,0.4,0.5,0.6,0.7,0.8,0.9,1,1,1,1,1,1,1", // too many
            b"0,nan,0.2,0.3,0.4,0.5,0.6,0.7,0.8,0.9,1,1,1,1,1,1",
            &[0xff, 0xfe], // not UTF-8
        ] {
            assert_eq!(CustomCurve::parse(block), default, "block {block:?}");
        }
    }

    /// Values outside the range are clamped rather than rejected: a curve
    /// above unity would boost on the beat.
    #[test]
    fn out_of_range_points_are_clamped() {
        let block = b"0,-3,2,0.3,0.4,0.5,0.6,0.7,0.8,0.9,1,1,1,1,1,1";
        let curve = CustomCurve::parse(block);
        assert_eq!(curve.points()[1], 0.0);
        assert_eq!(curve.points()[2], 1.0);
    }

    /// The seam is enforced here, not requested politely of whoever wrote
    /// the block: a curve that starts high jumps the gain at every cycle
    /// boundary.
    #[test]
    fn the_first_point_is_pinned_to_the_floor() {
        let block = b"0.8,0.8,0.9,1,1,1,1,1,1,1,1,1,1,1,1,1";
        let curve = CustomCurve::parse(block);
        assert_eq!(curve.points()[0], 0.0);
        assert!(
            curve.shape().gain(0.0, ATTACK) < 1e-4,
            "the beat must still be silent"
        );
    }
}
