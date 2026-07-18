/// Resolution of the lookup table. 256 intervals is far below any audible
/// stair-stepping once linearly interpolated and smoothed.
const RESOLUTION: usize = 256;

/// A sampled gain curve over one cycle, indexed by phase `[0, 1)`.
///
/// Stored inline (`[f32; 257]`, ~1 KiB): building one fills a fixed array —
/// no heap — so plugins can own several and switch without allocating. The
/// extra guard point at index 256 holds `f(1.0)`, letting
/// [`lookup`](CurveTable::lookup) interpolate the last interval without a
/// wrap branch.
#[derive(Clone, Copy, Debug)]
pub struct CurveTable {
    table: [f32; RESOLUTION + 1],
}

impl CurveTable {
    /// Samples `f` (phase in `[0, 1]` → gain) into a table.
    pub fn from_fn(f: impl Fn(f32) -> f32) -> Self {
        let mut table = [0.0; RESOLUTION + 1];
        for (i, slot) in table.iter_mut().enumerate() {
            *slot = f(i as f32 / RESOLUTION as f32);
        }
        Self { table }
    }

    /// Linearly interpolated gain at `phase`. Any real input is accepted:
    /// the phase is wrapped into `[0, 1)` (negative values wrap upward, as
    /// a cyclic position should).
    pub fn lookup(&self, phase: f32) -> f32 {
        let wrapped = phase.rem_euclid(1.0);
        let scaled = wrapped * RESOLUTION as f32;
        let index = (scaled as usize).min(RESOLUTION - 1);
        let frac = scaled - index as f32;
        let a = self.table[index];
        let b = self.table[index + 1];
        a + (b - a) * frac
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reproduces_endpoints() {
        let curve = CurveTable::from_fn(|phase| phase);
        assert_eq!(curve.lookup(0.0), 0.0);
        assert!((curve.lookup(0.999_999) - 1.0).abs() < 1e-3);
    }

    #[test]
    fn interpolates_between_grid_points() {
        let curve = CurveTable::from_fn(|phase| phase);
        // Halfway inside a grid interval must land halfway between samples.
        let mid = 0.5 / RESOLUTION as f32 + 0.25;
        assert!((curve.lookup(mid) - mid).abs() < 1e-6);
    }

    #[test]
    fn wraps_out_of_range_phases() {
        let curve = CurveTable::from_fn(|phase| phase);
        assert!((curve.lookup(1.25) - 0.25).abs() < 1e-6);
        assert!((curve.lookup(-0.25) - 0.75).abs() < 1e-6);
    }
}
