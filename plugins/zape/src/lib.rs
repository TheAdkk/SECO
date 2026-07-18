//! Zape — tempo-synced ducking.
//!
//! Not a compressor: no signal analysis, no sidechain input. The audio is
//! multiplied by a gain curve indexed by the host's beat position — a
//! "ghost kick". Transport facts this leans on (beat = quarter note, frozen
//! ppq when stopped, uninterpolated loop jumps) are documented with evidence
//! in docs/clap-notes.md §1.5 and §11.

use seco_clap::seco_export;
use seco_core::{AudioBuffer, ParamDesc, ParamRange, Plugin, RtContext};
use seco_dsp::{CurveTable, OnePole, duck};

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

struct Zape {
    sample_rate: f64,
    /// Cycle phase in `[0, 1)`. Kept across transport stops: hosts freeze
    /// ppq when stopped (§11.2), and zape keeps ducking free-run.
    phase: f64,
    smoother: OnePole,
    /// True until the first processed sample after new/activate/reset: the
    /// smoother then snaps onto the curve's actual value at the starting
    /// phase instead of gliding down from an arbitrary 1.0 (which was an
    /// audible first-cycle overshoot on slow-attack curves). Only initial
    /// state snaps — transport jumps keep their glide.
    needs_snap: bool,
    curves: [CurveTable; 3],
    /// Per-block gain, computed once and applied to every channel.
    /// Allocated in `activate` (where `max_frames` is known); `process`
    /// never allocates.
    gain: Vec<f32>,
}

/// The shipped curves live in seco-dsp (`duck`), where a unit test enforces
/// the cyclic invariant `f(0) == f(1) == 1.0` — the seam-click bug class.
fn curve_shapes() -> [CurveTable; 3] {
    [duck::pump(), duck::punch(), duck::soft()]
}

impl Plugin for Zape {
    const ID: &'static str = "dev.seco.zape";
    const NAME: &'static str = "Zape";
    const VENDOR: &'static str = "SECO";
    const VERSION: &'static str = "0.4.0";
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
        Zape {
            sample_rate: 48_000.0,
            phase: 0.0,
            smoother: OnePole::new(1.0),
            needs_snap: true,
            curves: curve_shapes(),
            gain: Vec::new(),
        }
    }

    fn activate(&mut self, sample_rate: f64, max_frames: u32) {
        self.sample_rate = sample_rate;
        self.smoother.set_tau(SMOOTH_TAU_SECONDS, sample_rate as f32);
        self.gain.clear();
        self.gain.resize(max_frames as usize, 1.0);
        self.needs_snap = true;
    }

    fn reset(&mut self) {
        self.phase = 0.0;
        // Deliberately NOT arming the snap: reset() can happen mid-stream
        // (hosts that flush FX around transport changes), where snapping to
        // the new phase's curve value is a one-sample gain step — a click
        // (measured 0.424). The smoother keeps its value and glides; the
        // snap belongs only to fresh streams (new/activate), where there is
        // no prior output to click against.
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

        if self.needs_snap {
            // Order matters: the phase above is already resynced for this
            // block, so this is the exact target of the first sample. Start
            // *on* the curve, not above it.
            let first_target = if bypass {
                1.0
            } else {
                1.0 + (curve.lookup(self.phase as f32) - 1.0) * mix
            };
            self.smoother.snap_to(first_target);
            self.needs_snap = false;
        }

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

seco_export!(Zape);

// VST3 export: free-audio's clap-wrapper (C++ inside) re-hosts the
// `clap_entry` this same library exports and presents it as a VST3. SECO's
// core stays pure Rust; this format adapter is borrowed, and only exists
// when the `vst3` feature is on.
#[cfg(feature = "vst3")]
clap_wrapper::export_vst3!();

#[cfg(test)]
mod tests {
    use seco_core::Transport;
    use seco_core::__private::with_rt_context;
    use seco_dsp::duck;

    use super::*;

    /// rate = 1/4 (cycle 1 beat), mix = 1, curve = Soft, bypass off. Soft's
    /// slow attack is what made the start-up overshoot audible.
    const PARAMS_SOFT: [f64; 4] = [2.0, 1.0, 2.0, 0.0];

    fn transport_at(ppq: f64) -> Transport {
        Transport {
            tempo_bpm: Some(120.0),
            song_pos_beats: Some(ppq),
            song_pos_seconds: None,
            time_signature: None,
            playing: true,
        }
    }

    /// Feeds a block of all-ones, so the output IS the gain signal. Buffers
    /// are allocated out here: inside the runner the armed allocation
    /// detector (registered by `seco_export!` above) aborts the test binary,
    /// so these tests also pin `process()` as allocation-free.
    fn run_block_with(plugin: &mut Zape, params: &[f64; 4], ppq: f64, frames: usize) -> Vec<f32> {
        let mut left = vec![1.0_f32; frames];
        let mut right = vec![1.0_f32; frames];
        with_rt_context(transport_at(ppq), params, |rt| {
            let mut channels: [&mut [f32]; 2] = [&mut left[..], &mut right[..]];
            let mut audio = AudioBuffer::new(&mut channels);
            plugin.process(&mut audio, rt);
        });
        left
    }

    fn run_block(plugin: &mut Zape, ppq: f64, frames: usize) -> Vec<f32> {
        run_block_with(plugin, &PARAMS_SOFT, ppq, frames)
    }

    /// Streams `blocks` host-realistic blocks (advancing ppq like a DAW at
    /// 120 BPM / 48 kHz) and returns the largest per-sample gain step and
    /// the ppq where it happened. `prev` carries continuity across calls so
    /// scenarios can measure steps across reset()/jump boundaries too.
    fn max_gain_step(
        plugin: &mut Zape,
        params: &[f64; 4],
        start_ppq: f64,
        blocks: usize,
        prev: &mut Option<f32>,
    ) -> (f32, f64) {
        const BLOCK: usize = 512;
        let beats_per_sample = 120.0 / 60.0 / 48_000.0;
        let mut ppq = start_ppq;
        let (mut max_step, mut max_at) = (0.0_f32, 0.0_f64);
        for _ in 0..blocks {
            let gains = run_block_with(plugin, params, ppq, BLOCK);
            for (i, gain) in gains.iter().enumerate() {
                if let Some(previous) = *prev {
                    let step = (gain - previous).abs();
                    if step > max_step {
                        max_step = step;
                        max_at = ppq + i as f64 * beats_per_sample;
                    }
                }
                *prev = Some(*gain);
            }
            ppq += BLOCK as f64 * beats_per_sample;
        }
        (max_step, max_at)
    }

    /// Diagnostic, not a pass/fail gate: prints the worst per-sample gain
    /// step per scenario. Run with:
    /// `cargo test -p zape -- --ignored --nocapture click_scan`
    #[test]
    #[ignore = "diagnostic: run explicitly with --ignored --nocapture"]
    fn click_scan_diagnostic() {
        let curves: [(&str, f64); 3] = [("pump", 0.0), ("punch", 1.0), ("soft", 2.0)];
        let rates: [(&str, f64, f64); 2] = [("1/4", 2.0, 1.0), ("1/16", 4.0, 0.25)];
        println!("--- steady playback, 8 cycles ---");
        for (cname, c) in curves {
            for (rname, r, cycle_beats) in rates {
                let mut plugin = Zape::new();
                plugin.activate(48_000.0, 512);
                let params = [r, 1.0, c, 0.0];
                let blocks = (8.0 * cycle_beats * 24_000.0 / 512.0).ceil() as usize + 1;
                let mut prev = None;
                let (step, at) = max_gain_step(&mut plugin, &params, 0.0, blocks, &mut prev);
                let phase = (at / cycle_beats).rem_euclid(1.0);
                println!("{cname:5} {rname:4}: max |dG| = {step:.5} @ phase {phase:.4}");
            }
        }
        println!("--- transport jump while playing (glide expected) ---");
        let mut plugin = Zape::new();
        plugin.activate(48_000.0, 512);
        let params = [2.0, 1.0, 2.0, 0.0];
        let mut prev = None;
        max_gain_step(&mut plugin, &params, 10.25, 4, &mut prev);
        let (step, _) = max_gain_step(&mut plugin, &params, 20.6, 2, &mut prev);
        println!("soft jump 0.25->0.6: max |dG| = {step:.5}");
        println!("--- reset() mid-stream + transport jump (flush-on-jump hosts) ---");
        let mut plugin = Zape::new();
        plugin.activate(48_000.0, 512);
        let mut prev = None;
        max_gain_step(&mut plugin, &params, 10.25, 4, &mut prev);
        plugin.reset();
        let (step, _) = max_gain_step(&mut plugin, &params, 20.6, 2, &mut prev);
        println!("soft reset+jump 0.25->0.6: max |dG| = {step:.5}");
        println!("--- reset() mid-stream, continuous transport ---");
        let mut plugin = Zape::new();
        plugin.activate(48_000.0, 512);
        let mut prev = None;
        max_gain_step(&mut plugin, &params, 10.25, 4, &mut prev);
        plugin.reset();
        let after = 10.25 + 4.0 * 512.0 * (120.0 / 60.0) / 48_000.0;
        let (step, _) = max_gain_step(&mut plugin, &params, after, 2, &mut prev);
        println!("soft reset continuous: max |dG| = {step:.5}");
    }

    /// Steady playback must never step the gain: the seams are closed and
    /// the attacks are finite-slope. Bound = 3x the worst measured slope
    /// (0.00686 at Punch 1/16); a one-sample click is orders above it.
    #[test]
    fn steady_playback_never_steps_the_gain() {
        for curve in [0.0_f64, 1.0, 2.0] {
            for (rate, cycle_beats) in [(2.0_f64, 1.0_f64), (4.0, 0.25)] {
                let mut plugin = Zape::new();
                plugin.activate(48_000.0, 512);
                let params = [rate, 1.0, curve, 0.0];
                let blocks = (8.0 * cycle_beats * 24_000.0 / 512.0).ceil() as usize + 1;
                let mut prev = None;
                let (step, at) = max_gain_step(&mut plugin, &params, 0.0, blocks, &mut prev);
                assert!(
                    step < 0.02,
                    "curve {curve} rate {rate}: gain stepped {step} at ppq {at}"
                );
            }
        }
    }

    /// Hosts with flush-on-transport-change call reset() around jumps. That
    /// must glide like any other jump — snapping there is a one-sample
    /// click (measured 0.424 before the fix).
    #[test]
    fn reset_plus_jump_glides_instead_of_clicking() {
        let mut plugin = Zape::new();
        plugin.activate(48_000.0, 512);
        let params = [2.0, 1.0, 2.0, 0.0];
        let mut prev = None;
        max_gain_step(&mut plugin, &params, 10.25, 4, &mut prev);
        plugin.reset();
        let (step, _) = max_gain_step(&mut plugin, &params, 20.6, 2, &mut prev);
        assert!(step < 0.02, "reset+jump stepped the gain by {step}");
    }

    /// The start-up invariant that shipped untested: after new/activate/
    /// reset, the very first sample must sit ON the curve at the starting
    /// phase — not glide down from an arbitrary 1.0.
    #[test]
    fn first_sample_starts_on_the_curve_not_at_unity() {
        let mut plugin = Zape::new();
        plugin.activate(48_000.0, 512);
        plugin.reset();

        // ppq 10.25, cycle 1 beat → phase 0.25, well inside Soft's dip.
        let out = run_block(&mut plugin, 10.25, 64);

        let expected = duck::soft().lookup(0.25);
        assert!(expected < 0.2, "test premise: Soft at phase 0.25 must duck deep");
        assert!(
            (out[0] - expected).abs() < 1e-4,
            "first sample {} must sit on the curve ({expected}), not near 1.0",
            out[0]
        );
    }

    /// The snap is initial-state only. A transport jump (loop wrap, seek)
    /// must keep the smoother's glide — snapping there would reintroduce
    /// the seam click as a resync click.
    #[test]
    fn transport_jump_glides_snap_only_on_start() {
        let mut plugin = Zape::new();
        plugin.activate(48_000.0, 512);
        plugin.reset();

        let first = run_block(&mut plugin, 10.25, 64);
        // Jump far away on the curve (phase 0.25 → 0.6, gain ~0.1 → ~0.6).
        let second = run_block(&mut plugin, 20.6, 64);

        let jump_target = duck::soft().lookup(0.6);
        assert!(jump_target > 0.5, "test premise: jump lands on a high-gain phase");
        let boundary_step = (second[0] - first[63]).abs();
        assert!(
            boundary_step < 0.02,
            "jump must glide from {} (stepped {boundary_step} toward {jump_target})",
            first[63]
        );
    }
}
