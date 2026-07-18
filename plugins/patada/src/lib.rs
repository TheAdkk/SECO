//! patada — tempo-synced ducking.
//!
//! Not a compressor: no signal analysis, no sidechain input. The audio is
//! multiplied by a gain curve indexed by the host's beat position — a
//! "ghost kick". Transport facts this leans on (beat = quarter note, frozen
//! ppq when stopped, uninterpolated loop jumps) are documented with evidence
//! in docs/clap-notes.md §1.5 and §11.

use seco_clap::seco_export;
use seco_core::{AudioBuffer, ParamDesc, ParamRange, Plugin, RtContext};
use seco_dsp::{CurveTable, OnePole};

const PARAM_RATE: usize = 0;
const PARAM_MIX: usize = 1;
const PARAM_CURVE: usize = 2;
const PARAM_BYPASS: usize = 3;

/// Duck cycle length in beats per `Rate` step. Beats are quarter notes —
/// verified empirically (docs/clap-notes.md §1.5) — so "1/1" is one whole
/// note = 4 beats.
const RATE_BEATS: [f64; 5] = [4.0, 2.0, 1.0, 0.5, 0.25];

/// Gain smoother time constant. This is also the transport-jump declick:
/// loop wraps arrive as an uninterpolated backward ppq jump (§11.6), the
/// phase resyncs hard, and the smoother glides the gain step over ~2 ms
/// instead of clicking. It equally declicks parameter flips (mix, curve,
/// bypass).
const SMOOTH_TAU_SECONDS: f32 = 0.002;

/// Free-run tempo when the host provides none.
const FALLBACK_BPM: f64 = 120.0;

struct Patada {
    sample_rate: f64,
    /// Cycle phase in `[0, 1)`. Kept across transport stops: hosts freeze
    /// ppq when stopped (§11.2), and patada keeps ducking free-run.
    phase: f64,
    smoother: OnePole,
    curves: [CurveTable; 3],
    /// Per-block gain, computed once and applied to every channel.
    /// Allocated in `activate` (where `max_frames` is known); `process`
    /// never allocates.
    gain: Vec<f32>,
}

fn curve_shapes() -> [CurveTable; 3] {
    [
        // Pump: brief hold at silence, then a power-curve recovery across
        // the whole cycle — the classic sidechain feel.
        CurveTable::from_fn(|phase| ((phase - 0.02) / 0.98).clamp(0.0, 1.0).powf(0.6)),
        // Punch: full dip, fully recovered by 35% of the cycle.
        CurveTable::from_fn(|phase| (phase / 0.35).clamp(0.0, 1.0).powf(0.8)),
        // Soft: raised cosine, gentle the whole way.
        CurveTable::from_fn(|phase| {
            0.5 - 0.5 * (core::f32::consts::PI * phase.clamp(0.0, 1.0)).cos()
        }),
    ]
}

impl Plugin for Patada {
    const ID: &'static str = "dev.seco.patada";
    const NAME: &'static str = "patada";
    const VENDOR: &'static str = "SECO";
    const VERSION: &'static str = "0.3.0";
    const DESCRIPTION: &'static str = "Tempo-synced ducking";

    const PARAMS: &'static [ParamDesc] = &[
        ParamDesc {
            name: "Rate",
            range: ParamRange::Stepped {
                labels: &["1/1", "1/2", "1/4", "1/8", "1/16"],
                default: 2,
            },
        },
        ParamDesc {
            name: "Mix",
            range: ParamRange::Continuous { min: 0.0, max: 1.0, default: 1.0 },
        },
        ParamDesc {
            name: "Curve",
            range: ParamRange::Stepped { labels: &["Pump", "Punch", "Soft"], default: 0 },
        },
        ParamDesc { name: "Bypass", range: ParamRange::Toggle { default: false, bypass: true } },
    ];

    fn new() -> Self {
        Patada {
            sample_rate: 48_000.0,
            phase: 0.0,
            smoother: OnePole::new(1.0),
            curves: curve_shapes(),
            gain: Vec::new(),
        }
    }

    fn activate(&mut self, sample_rate: f64, max_frames: u32) {
        self.sample_rate = sample_rate;
        self.smoother.set_tau(SMOOTH_TAU_SECONDS, sample_rate as f32);
        self.gain.clear();
        self.gain.resize(max_frames as usize, 1.0);
    }

    fn reset(&mut self) {
        self.phase = 0.0;
        self.smoother.snap_to(1.0);
    }

    fn process(&mut self, audio: &mut AudioBuffer, rt: &RtContext) {
        let transport = rt.transport();
        let rate = (rt.param(PARAM_RATE).round().max(0.0) as usize).min(RATE_BEATS.len() - 1);
        let cycle_beats = RATE_BEATS[rate];
        let mix = rt.param(PARAM_MIX).clamp(0.0, 1.0) as f32;
        let curve =
            (rt.param(PARAM_CURVE).round().max(0.0) as usize).min(self.curves.len() - 1);
        let curve = &self.curves[curve];
        let bypass = rt.param(PARAM_BYPASS) >= 0.5;

        // Transport is read once per block: a mid-block tempo change lands
        // one block late. Known v1 limitation.
        let tempo = transport.tempo_bpm.unwrap_or(FALLBACK_BPM);
        if transport.playing {
            if let Some(ppq) = transport.song_pos_beats {
                // Hard resync every block while rolling. Loop wraps and
                // seeks land here as a phase jump — no interpolation across
                // the discontinuity — and the smoother absorbs the gain
                // step. rem_euclid also handles negative ppq (count-in).
                self.phase = (ppq / cycle_beats).rem_euclid(1.0);
            }
        }
        // Stopped, or no beats timeline: free-run from the current phase.

        let inc = tempo / 60.0 / cycle_beats / self.sample_rate; // cycles/sample
        let frames = audio.frames();
        let n = frames.min(self.gain.len());
        debug_assert!(n == frames, "host sent more frames than activate() promised");

        for slot in &mut self.gain[..n] {
            let target = if bypass {
                // Bypass stays inside process() (CLAP requires the host to
                // keep calling it); gliding to unity makes it click-free.
                1.0
            } else {
                1.0 + (curve.lookup(self.phase as f32) - 1.0) * mix
            };
            *slot = self.smoother.process(target);
            self.phase = (self.phase + inc).rem_euclid(1.0);
        }
        // If the host overran its activate() promise, the tail passes
        // through un-ducked; keep the phase advancing consistently anyway.
        self.phase = (self.phase + inc * (frames - n) as f64).rem_euclid(1.0);

        for channel in audio.channels_mut() {
            for (sample, gain) in channel.iter_mut().zip(&self.gain[..n]) {
                *sample *= gain;
            }
        }
    }
}

seco_export!(Patada);
