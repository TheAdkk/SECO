//! The shipped ducking curves.
//!
//! Invariant, enforced by test: every curve here is cyclic —
//! `f(0.0) == f(1.0)`, both edges fully recovered (gain 1.0) — with the dip
//! living *inside* the cycle. A curve that ends anywhere other than where it
//! starts steps the gain at every phase wrap and clicks once per cycle.
//!
//! Curves are drawn in phase, so attack width scales with cycle length
//! (rate × tempo), like factory curves in phase-drawn duckers.

use core::f32::consts::PI;

use crate::CurveTable;

/// The common cyclic-duck skeleton: a fast cosine attack from 1.0 down to
/// the dip floor over the first `attack` fraction of the cycle, then a
/// shape-specific recovery back to exactly 1.0. Both edges sit at 1.0, so
/// the seam closes by construction.
///
/// `recover` receives normalized time `t ∈ [0, 1]` over the post-attack
/// span and must return 0.0 at t=0 and 1.0 at t=1.
fn ducked(phase: f32, attack: f32, recover: impl Fn(f32) -> f32) -> f32 {
    let phase = phase.clamp(0.0, 1.0);
    if phase < attack {
        // Cosine edge: full punch without a hard corner's spray.
        0.5 + 0.5 * (PI * phase / attack).cos()
    } else {
        recover((phase - attack) / (1.0 - attack)).clamp(0.0, 1.0)
    }
}

/// Pump: 2% attack, then a power-curve recovery across the whole cycle —
/// the classic sidechain feel.
pub fn pump() -> CurveTable {
    CurveTable::from_fn(|phase| ducked(phase, 0.02, |t| t.powf(0.6)))
}

/// Punch: fastest attack (1.5%), fully recovered by ~35% of the cycle.
pub fn punch() -> CurveTable {
    CurveTable::from_fn(|phase| ducked(phase, 0.015, |t| (t / 0.34).min(1.0).powf(0.8)))
}

/// Soft: same 2% attack as Pump — the duck must land on the beat — with the
/// softness where it is actually heard: a raised-cosine recovery across the
/// whole cycle. (A 6% attack put ~36 ms of full volume after every beat at
/// 99 BPM, audibly "volume first, duck later".)
pub fn soft() -> CurveTable {
    CurveTable::from_fn(|phase| ducked(phase, 0.02, |t| 0.5 - 0.5 * (PI * t).cos()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEAM_EPSILON: f32 = 1e-3;

    fn shipped() -> [(&'static str, CurveTable); 3] {
        [("pump", pump()), ("punch", punch()), ("soft", soft())]
    }

    /// The invariant the Phase 0 brief called out and the first shapes broke
    /// anyway: a cyclic gain curve must close its seam or it clicks on every
    /// wrap from the second cycle on.
    #[test]
    fn duck_curves_close_the_seam() {
        for (name, curve) in shipped() {
            let seam = curve.seam_step();
            assert!(
                seam < SEAM_EPSILON,
                "{name}: |f(0) - f(1)| = {seam} — gain steps at every phase wrap"
            );
            assert!(
                (curve.lookup(0.0) - 1.0).abs() < SEAM_EPSILON,
                "{name}: cycle must start recovered (gain 1.0), got {}",
                curve.lookup(0.0)
            );
        }
    }

    /// The duck must land essentially ON the beat: a wide attack turns into
    /// an audible full-volume window after every beat before the dip
    /// arrives (36 ms at 99 BPM with the old 6% Soft attack — reported by
    /// ear as "full volume, then an abrupt drop"). Every shipped curve must
    /// be deep within the first 4% of the cycle.
    #[test]
    fn duck_lands_near_the_beat() {
        for (name, curve) in shipped() {
            let min = (0..=40)
                .map(|i| curve.lookup(i as f32 * 0.001))
                .fold(f32::MAX, f32::min);
            assert!(
                min < 0.15,
                "{name}: still at gain {min} within 4% of the cycle — duck lands late"
            );
        }
    }

    /// Closing the seam must not close the *duck*: the dip has to survive
    /// inside the cycle.
    #[test]
    fn duck_curves_still_duck() {
        for (name, curve) in shipped() {
            let min = (0..=1000)
                .map(|i| curve.lookup(i as f32 / 1000.0))
                .fold(f32::MAX, f32::min);
            assert!(min < 0.05, "{name}: never dips (min gain {min})");
        }
    }
}
