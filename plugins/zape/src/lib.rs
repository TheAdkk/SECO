//! Zape — tempo-synced ducking.
//!
//! Not a compressor: no signal analysis, no sidechain input. The audio is
//! multiplied by a gain curve indexed by the host's beat position — a
//! "ghost kick". Transport facts this leans on (beat = quarter note, frozen
//! ppq when stopped, uninterpolated loop jumps) are documented with evidence
//! in docs/clap-notes.md §1.5 and §11.

mod custom;
mod editor;
mod library;

use custom::CustomCurve;
use seco_clap::seco_export;
use seco_core::{AudioBuffer, EditorPage, ParamDesc, ParamRange, Plugin, RtContext};
use seco_dsp::{DuckShape, Slew, duck};

const PARAM_RATE: usize = 0;
const PARAM_MIX: usize = 1;
const PARAM_CURVE: usize = 2;
const PARAM_BYPASS: usize = 3;

/// The `Curve` parameter's labels: everything seco-dsp ships, plus the
/// drawn one. Built from `duck::NAMES` at compile time so the two lists
/// cannot drift; the drawn curve is last, keeping the shipped indices — and
/// therefore saved sessions — untouched.
const CURVE_LABELS: [&str; duck::COUNT + 1] = {
    let mut labels = [""; duck::COUNT + 1];
    let mut index = 0;
    while index < duck::COUNT {
        labels[index] = duck::NAMES[index];
        index += 1;
    }
    labels[duck::COUNT] = "Custom";
    labels
};

/// Index of the drawn curve in [`CURVE_LABELS`].
const CUSTOM_CURVE: usize = duck::COUNT;

/// Duck cycle length in beats per `Rate` step. Beats are quarter notes —
/// verified empirically (docs/clap-notes.md §1.5) — so "1/1" is one whole
/// note = 4 beats.
const RATE_BEATS: [f64; 5] = [4.0, 2.0, 1.0, 0.5, 0.25];

/// Ceiling on how fast the gain may move, in gain units per second.
///
/// Set to the peak slope of the shapes' own dip-entry fade, so following a
/// curve is transparent — the limiter never touches it — while everything
/// that is *not* a curve is ramped instead of stepped: loop wraps and seeks
/// (uninterpolated backward ppq jumps, §11.6), parameter flips (mix, curve,
/// rate, bypass) and preset changes. A full-scale jump takes about 3.2 ms.
const MAX_GAIN_RATE: f32 = (std::f64::consts::PI / 2.0 / ATTACK_SECONDS) as f32;

/// How fast a scope bucket forgets a loud sample, per cycle. The envelope
/// is drawn against the beat, so each bucket is revisited once per cycle;
/// this is what stops a single transient from parking there forever.
const SCOPE_RELEASE: f32 = 0.6;

/// Free-run tempo when the host provides none.
const FALLBACK_BPM: f64 = 120.0;

/// How long the gain takes to reach the floor going *into* a dip, in
/// seconds — the same wall-clock duration at every tempo and rate.
///
/// This is the plugin's whole click story on the entry side. The first
/// model faded over a fraction of the cycle, which meant 25 ms at 1/2 and
/// 3 ms at 1/16: audibly a tick at the fast end, and a late duck at the
/// slow one. 5 ms is below the ear's click threshold and still tight
/// enough that the beat is not smeared.
const ATTACK_SECONDS: f64 = 0.005;

struct Zape {
    sample_rate: f64,
    /// Cycle phase in `[0, 1)`. Kept across transport stops: hosts freeze
    /// ppq when stopped (§11.2), and zape keeps ducking free-run.
    phase: f64,
    slew: Slew,
    /// True until the first processed sample after new/activate/reset: the
    /// limiter then snaps onto the shape's actual value at the starting
    /// phase instead of gliding down from an arbitrary 1.0 (which was an
    /// audible first-cycle overshoot on slow-attack curves). Only initial
    /// state snaps — transport jumps keep their glide.
    needs_snap: bool,
    curves: [DuckShape; duck::COUNT],
    /// The drawn curve, and the shape sampled from it. Rebuilt only when a
    /// state block arrives, never per block.
    custom: CustomCurve,
    custom_shape: DuckShape,
    /// Peak level per scope bucket, indexed by phase — a picture of the
    /// audio drawn against the beat, like a scope triggered on it. The first
    /// half is the signal arriving, the second the signal leaving: the duck
    /// is the difference between them, and showing only the input made the
    /// one thing the plugin does invisible.
    ///
    /// The plugin owns this; `RtContext::set_scope` publishes a copy the
    /// editor can read.
    envelope: [f32; seco_clap::SCOPE_BUCKETS],
    /// The bucket the phase was in last, so the release above fires once per
    /// pass rather than once per sample.
    last_bucket: usize,
    /// Per-block gain, computed once and applied to every channel.
    /// Allocated in `activate` (where `max_frames` is known); `process`
    /// never allocates.
    gain: Vec<f32>,
}

impl Plugin for Zape {
    const ID: &'static str = "dev.seco.zape";
    const NAME: &'static str = "Zape";
    const VENDOR: &'static str = "SECO";
    const VERSION: &'static str = "0.5.0";
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
            // Percent is the plain value, not a display trick: the host's
            // automation lane reads the same 0..100 the editor shows.
            range: ParamRange::Continuous {
                min: 0.0,
                max: 100.0,
                default: 100.0,
                unit: "%",
                decimals: 0,
            },
        },
        ParamDesc {
            name: "Curve",
            // Names and tables both come from seco-dsp, in one order: the
            // stored value is the index, so the list is append-only.
            range: ParamRange::Stepped {
                labels: &CURVE_LABELS,
                default: 0,
            },
        },
        ParamDesc {
            name: "Bypass",
            range: ParamRange::Toggle {
                default: false,
                bypass: true,
            },
        },
    ];

    /// Zape's beat envelope uses the framework's legacy 256-slot picture.
    const SCOPE_SLOTS: usize = seco_clap::SCOPE_BUCKETS;
    const EDITOR: Option<EditorPage> = Some(editor::PAGE);

    fn editor_script(params: &[f64], state: &[u8]) -> Option<String> {
        editor::script(params, state)
    }

    fn editor_frame(scope: &[f32]) -> Option<String> {
        editor::frame(scope)
    }

    fn editor_message(text: &str) -> Option<String> {
        editor::message(text)
    }

    fn new() -> Self {
        Zape {
            sample_rate: 48_000.0,
            phase: 0.0,
            slew: Slew::new(1.0),
            needs_snap: true,
            curves: duck::tables(),
            custom: CustomCurve::default(),
            custom_shape: CustomCurve::default().shape(),
            envelope: [0.0; seco_clap::SCOPE_BUCKETS],
            last_bucket: usize::MAX,
            gain: Vec::new(),
        }
    }

    fn activate(&mut self, sample_rate: f64, max_frames: u32) {
        self.sample_rate = sample_rate;
        self.slew.set_max_rate(MAX_GAIN_RATE, sample_rate as f32);
        self.gain.clear();
        self.gain.resize(max_frames as usize, 1.0);
        self.needs_snap = true;
    }

    fn apply_state(&mut self, state: &[u8], _rt: &RtContext) {
        // Audio thread: parsing borrows the block and the table is a fixed
        // array, so nothing here allocates. An unreadable or empty block
        // reads as the default curve — a session that cannot be understood
        // must still play.
        self.custom = CustomCurve::parse(state);
        self.custom_shape = self.custom.shape();
    }

    fn reset(&mut self) {
        self.phase = 0.0;
        // Deliberately NOT arming the snap: reset() can happen mid-stream
        // (hosts that flush FX around transport changes), where snapping to
        // the new phase's curve value is a one-sample gain step — a click
        // (measured 0.424). The limiter keeps its value and ramps; the
        // snap belongs only to fresh streams (new/activate), where there is
        // no prior output to click against.
    }

    fn process(&mut self, audio: &mut AudioBuffer, rt: &RtContext) {
        let transport = rt.transport();
        let rate = (rt.param(PARAM_RATE).round().max(0.0) as usize).min(RATE_BEATS.len() - 1);
        let cycle_beats = RATE_BEATS[rate];
        // Mix is a percentage in plain units (what the host and the editor
        // show); the curve blend wants a 0..1 factor.
        let mix = (rt.param(PARAM_MIX).clamp(0.0, 100.0) / 100.0) as f32;
        let curve = (rt.param(PARAM_CURVE).round().max(0.0) as usize).min(CUSTOM_CURVE);
        let curve = if curve == CUSTOM_CURVE {
            &self.custom_shape
        } else {
            &self.curves[curve]
        };
        let bypass = rt.param(PARAM_BYPASS) >= 0.5;

        // Transport is read once per block: a mid-block tempo change lands
        // one block late. Known v1 limitation.
        let tempo = transport.tempo_bpm.unwrap_or(FALLBACK_BPM);
        if transport.playing {
            if let Some(ppq) = transport.song_pos_beats {
                // Hard resync every block while rolling. Loop wraps and
                // seeks land here as a phase jump — no interpolation across
                // the discontinuity — and the limiter ramps the gain
                // step. rem_euclid also handles negative ppq (count-in).
                self.phase = (ppq / cycle_beats).rem_euclid(1.0);
            }
        }
        // Stopped, or no beats timeline: free-run from the current phase.

        let inc = tempo / 60.0 / cycle_beats / self.sample_rate; // cycles/sample
        // The dip entry is a fixed wall-clock fade, so its width in phase
        // depends on how long this cycle actually lasts.
        let cycle_seconds = cycle_beats * 60.0 / tempo;
        let attack = (ATTACK_SECONDS / cycle_seconds) as f32;
        let frames = audio.frames();
        let n = frames.min(self.gain.len());
        debug_assert!(
            n == frames,
            "host sent more frames than activate() promised"
        );

        if self.needs_snap {
            // Order matters: the phase above is already resynced for this
            // block, so this is the exact target of the first sample. Start
            // *on* the curve, not above it.
            let first_target = if bypass {
                1.0
            } else {
                1.0 + (curve.gain(self.phase as f32, attack) - 1.0) * mix
            };
            self.slew.snap_to(first_target);
            self.needs_snap = false;
        }

        // Remembered before the gain loop advances it. Reconstructing this
        // afterwards by subtraction leaves a negative epsilon that
        // rem_euclid wraps to ~0.99999, lighting the last scope bucket on
        // every block.
        let block_start_phase = self.phase;

        for slot in &mut self.gain[..n] {
            let target = if bypass {
                // Bypass stays inside process() (CLAP requires the host to
                // keep calling it); gliding to unity makes it click-free.
                1.0
            } else {
                1.0 + (curve.gain(self.phase as f32, attack) - 1.0) * mix
            };
            *slot = self.slew.process(target);
            self.phase = (self.phase + inc).rem_euclid(1.0);
        }
        // If the host overran its activate() promise, the tail passes
        // through un-ducked; keep the phase advancing consistently anyway.
        self.phase = (self.phase + inc * (frames - n) as f64).rem_euclid(1.0);

        // The picture is of what arrives, before the duck: the curve drawn
        // over it is exactly what is about to be done to it. Peaks are
        // bucketed by phase, so the display is beat-aligned rather than
        // scrolling — a scope triggered on the beat.
        let half = rt.scope_len().min(self.envelope.len()) / 2;
        if half > 0 {
            for index in 0..n {
                let phase = (block_start_phase + inc * index as f64).rem_euclid(1.0);
                let bucket = ((phase * half as f64) as usize).min(half - 1);
                // Release is applied once per *pass*, when the phase first
                // reaches a bucket — not per sample. Decaying per sample
                // would leave each bucket holding the last few samples
                // instead of the loudest of the pass, and the picture would
                // be a thin wobble rather than an envelope.
                if bucket != self.last_bucket {
                    self.envelope[bucket] *= SCOPE_RELEASE;
                    self.envelope[half + bucket] *= SCOPE_RELEASE;
                    self.last_bucket = bucket;
                }
                let mut peak = 0.0_f32;
                for channel in audio.channels() {
                    peak = peak.max(channel[index].abs());
                }
                // The gain for this sample is already computed; the output
                // envelope is the input times it, sampled before the audio
                // is actually multiplied below.
                self.envelope[bucket] = self.envelope[bucket].max(peak);
                self.envelope[half + bucket] =
                    self.envelope[half + bucket].max(peak * self.gain[index]);
            }
            for bucket in 0..half * 2 {
                rt.set_scope(bucket, self.envelope[bucket].min(1.0));
            }
        }

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
    use seco_core::__private::with_rt_context;
    use seco_core::Transport;
    use seco_dsp::duck;

    use super::*;

    /// rate = 1/4 (cycle 1 beat), mix = 100%, curve = Soft, bypass off. Soft's
    /// slow attack is what made the start-up overshoot audible.
    const PARAMS_SOFT: [f64; 4] = [2.0, 100.0, 2.0, 0.0];

    /// The dip entry width the tests run at: 5 ms of a one-beat cycle at
    /// 120 BPM, matching `transport_at`.
    const TEST_ATTACK: f32 = (ATTACK_SECONDS / 0.5) as f32;

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
        with_rt_context(transport_at(ppq), params, &[], |rt| {
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

    /// Every parameter flip a user can perform from the editor, measured as
    /// a gain step across the block boundary where it lands. Parameters are
    /// read once per block, so a flip is always a block-boundary event: the
    /// limiter is the only thing standing between it and a click.
    ///
    /// Returns (name, worst step) per case.
    fn param_flip_steps() -> Vec<(&'static str, f32)> {
        // [rate, mix, curve, bypass]
        let flips: [(&str, [f64; 4], [f64; 4]); 8] = [
            ("mix 100 -> 0", [2.0, 100.0, 0.0, 0.0], [2.0, 0.0, 0.0, 0.0]),
            ("mix 0 -> 100", [2.0, 0.0, 0.0, 0.0], [2.0, 100.0, 0.0, 0.0]),
            (
                "curve pump -> punch",
                [2.0, 100.0, 0.0, 0.0],
                [2.0, 100.0, 1.0, 0.0],
            ),
            (
                "curve punch -> soft",
                [2.0, 100.0, 1.0, 0.0],
                [2.0, 100.0, 2.0, 0.0],
            ),
            (
                "rate 1/4 -> 1/16",
                [2.0, 100.0, 0.0, 0.0],
                [4.0, 100.0, 0.0, 0.0],
            ),
            (
                "rate 1/16 -> 1/1",
                [4.0, 100.0, 0.0, 0.0],
                [0.0, 100.0, 0.0, 0.0],
            ),
            (
                "bypass off -> on",
                [2.0, 100.0, 1.0, 0.0],
                [2.0, 100.0, 1.0, 1.0],
            ),
            (
                "bypass on -> off",
                [2.0, 100.0, 1.0, 1.0],
                [2.0, 100.0, 1.0, 0.0],
            ),
        ];
        const BLOCK: usize = 512;
        let beats_per_block = BLOCK as f64 * 120.0 / 60.0 / 48_000.0;
        flips
            .iter()
            .map(|(name, before, after)| {
                let mut plugin = Zape::new();
                plugin.activate(48_000.0, 512);
                let mut prev = None;
                // Land the flip mid-duck (phase ~0.1 at 1/4), where the two
                // curves disagree most — a flip at unity gain hides the step.
                max_gain_step(&mut plugin, before, 10.1, 3, &mut prev);
                let after_ppq = 10.1 + 3.0 * beats_per_block;
                let (step, _) = max_gain_step(&mut plugin, after, after_ppq, 3, &mut prev);
                (*name, step)
            })
            .collect()
    }

    /// A parameter flip must glide like everything else. Same 0.02 bound as
    /// steady playback: a one-sample click is orders above it.
    #[test]
    fn param_flips_never_step_the_gain() {
        for (name, step) in param_flip_steps() {
            assert!(step < 0.02, "{name} stepped the gain by {step}");
        }
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
                let params = [r, 100.0, c, 0.0];
                let blocks = (8.0 * cycle_beats * 24_000.0 / 512.0).ceil() as usize + 1;
                let mut prev = None;
                let (step, at) = max_gain_step(&mut plugin, &params, 0.0, blocks, &mut prev);
                let phase = (at / cycle_beats).rem_euclid(1.0);
                println!("{cname:5} {rname:4}: max |dG| = {step:.5} @ phase {phase:.4}");
            }
        }
        println!("--- parameter flips from the editor ---");
        for (name, step) in param_flip_steps() {
            println!("{name:22}: max |dG| = {step:.5}");
        }
        println!("--- transport jump while playing (glide expected) ---");
        let mut plugin = Zape::new();
        plugin.activate(48_000.0, 512);
        let params = [2.0, 100.0, 2.0, 0.0];
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

    /// Steady playback must never step the gain. Superseded in strength by
    /// `the_fade_holds_at_any_tempo_and_rate` — which proves the bound at
    /// the extremes rather than at one tempo — but kept: it is the cheap
    /// check that fails first when a shape grows a discontinuity.
    #[test]
    fn steady_playback_never_steps_the_gain() {
        for curve in [0.0_f64, 1.0, 2.0] {
            for (rate, cycle_beats) in [(2.0_f64, 1.0_f64), (4.0, 0.25)] {
                let mut plugin = Zape::new();
                plugin.activate(48_000.0, 512);
                let params = [rate, 100.0, curve, 0.0];
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

    /// Runs the plugin at an arbitrary tempo and sample rate and returns
    /// the worst per-sample gain step. The shared helpers above are pinned
    /// to 120 BPM / 48 kHz; the fade's whole point is that neither matters.
    fn worst_step_at(curve: f64, cycle_beats: f64, rate: f64, tempo: f64, sample_rate: f64) -> f32 {
        const BLOCK: usize = 256;
        let mut plugin = Zape::new();
        plugin.activate(sample_rate, BLOCK as u32);
        let beats_per_sample = tempo / 60.0 / sample_rate;
        let params = [rate, 100.0, curve, 0.0];
        let blocks = (4.0 * cycle_beats / (BLOCK as f64 * beats_per_sample)).ceil() as usize + 1;
        let (mut ppq, mut worst) = (0.0, 0.0_f32);
        let mut previous: Option<f32> = None;
        for _ in 0..blocks {
            let mut left = vec![1.0_f32; BLOCK];
            let mut right = vec![1.0_f32; BLOCK];
            let transport = Transport {
                tempo_bpm: Some(tempo),
                song_pos_beats: Some(ppq),
                song_pos_seconds: None,
                time_signature: None,
                playing: true,
            };
            with_rt_context(transport, &params, &[], |rt| {
                let mut channels: [&mut [f32]; 2] = [&mut left[..], &mut right[..]];
                let mut audio = AudioBuffer::new(&mut channels);
                plugin.process(&mut audio, rt);
            });
            for gain in &left {
                if let Some(p) = previous {
                    worst = worst.max((gain - p).abs());
                }
                previous = Some(*gain);
            }
            ppq += BLOCK as f64 * beats_per_sample;
        }
        worst
    }

    /// The failure this model exists to prevent: the dip entry used to be a
    /// fraction of the cycle, so it got shorter as rate and tempo went up
    /// until it clicked (3 ms at 1/16, heard as a tick). The fade is now
    /// wall-clock, so the worst case — fastest rate, fast tempo, lowest
    /// sample rate — obeys the same bound as the slowest.
    ///
    /// Bound: the peak slope of a raised cosine over `ATTACK_SECONDS`,
    /// which is what the fade is.
    #[test]
    fn the_fade_holds_at_any_tempo_and_rate() {
        let cases = [
            // (rate index, cycle beats, tempo, sample rate)
            (4.0, 0.25, 200.0, 44_100.0),
            (4.0, 0.25, 174.0, 48_000.0),
            (0.0, 4.0, 60.0, 96_000.0),
            (2.0, 1.0, 95.0, 48_000.0), // the session that reported the tick
        ];
        for (rate, cycle_beats, tempo, sample_rate) in cases {
            // The limiter computes its own step from the same constants in
            // f32, so allow a hair for rounding rather than chase ULPs.
            let bound =
                (std::f64::consts::PI / 2.0 / (ATTACK_SECONDS * sample_rate)) as f32 * 1.001;
            // The drawn curve is included: it goes through the same gain()
            // and must obey the same bound, whatever the user drew.
            for curve in 0..=CUSTOM_CURVE {
                let worst = worst_step_at(curve as f64, cycle_beats, rate, tempo, sample_rate);
                assert!(
                    worst <= bound,
                    "{} at rate {rate}, {tempo} BPM, {sample_rate} Hz: stepped {worst} \
                     (bound {bound})",
                    duck::NAMES[curve],
                );
            }
        }
    }

    /// The drawn curve is a curve like any other: selected by the same
    /// parameter, played through the same shape, on the beat.
    #[test]
    fn a_drawn_curve_plays_where_the_shipped_ones_do() {
        let mut plugin = Zape::new();
        plugin.activate(48_000.0, 512);
        // Flat at full gain after the beat: the only ducking left is the
        // dip entry itself, which is what makes the assertions sharp.
        let block = b"0,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1";
        with_rt_context(transport_at(0.0), &PARAMS_SOFT, &[], |rt| {
            plugin.apply_state(block, rt);
        });

        // Rate 1/4, full mix, Custom, bypass off.
        let params = [2.0, 100.0, CUSTOM_CURVE as f64, 0.0];
        let on_beat = run_block_with(&mut plugin, &params, 8.0, 64);
        assert!(on_beat[0] < 0.1, "gain {} at the beat", on_beat[0]);

        // Long enough for the slew limiter to cross from the beat's floor:
        // the jump to mid-cycle is a discontinuity like any other, ramped at
        // MAX_GAIN_RATE, so a 64-sample block would still be climbing.
        let mid = run_block_with(&mut plugin, &params, 8.5, 512);
        let arrived = *mid.last().unwrap();
        assert!(
            arrived > 0.9,
            "gain {arrived} mid-cycle, the drawn curve is not playing"
        );
    }

    /// `apply_state` runs on the audio thread. The allocation detector armed
    /// by `seco_export!` aborts the test binary if parsing the block or
    /// rebuilding the table touches the heap.
    #[test]
    fn applying_a_drawn_curve_never_allocates() {
        let mut plugin = Zape::new();
        plugin.activate(48_000.0, 512);
        // Built out here; inside the runner the detector is armed.
        let block = custom::CustomCurve::default().to_wire();
        with_rt_context(transport_at(0.0), &PARAMS_SOFT, &[], |rt| {
            plugin.apply_state(block.as_bytes(), rt);
            plugin.apply_state(b"junk", rt);
            plugin.apply_state(b"", rt);
        });
    }

    /// The scope is the picture of the audio, aligned to the beat: bucket i
    /// is phase i / count, the same axis the curve is drawn on. It carries
    /// both signals — in and out — because the gap between them *is* the
    /// duck, and a display of the input alone shows none of it. It also has
    /// to be free: it runs inside the real-time scope, where the allocation
    /// detector is armed.
    #[test]
    fn the_scope_pictures_both_signals_against_the_beat() {
        use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

        const BUCKETS: usize = seco_clap::SCOPE_BUCKETS;
        const HALF: usize = BUCKETS / 2;
        let scope: [AtomicU32; BUCKETS] = std::array::from_fn(|_| AtomicU32::new(0));
        let mut plugin = Zape::new();
        plugin.activate(48_000.0, 4096);

        // A block covering the first eighth of the cycle: rate 1/4 at 120
        // BPM is 24 000 samples, so 3 000 of them.
        let frames = 3_000;
        let mut left = vec![0.5_f32; frames];
        let mut right = vec![0.5_f32; frames];
        with_rt_context(transport_at(8.0), &[2.0, 100.0, 0.0, 0.0], &scope, |rt| {
            let mut channels: [&mut [f32]; 2] = [&mut left[..], &mut right[..]];
            let mut audio = AudioBuffer::new(&mut channels);
            plugin.process(&mut audio, rt);
        });

        let read = |bucket: usize| f32::from_bits(scope[bucket].load(Relaxed));
        // The block covered the first eighth of the cycle in both halves,
        // and nothing else.
        let lit: Vec<usize> = (0..BUCKETS).filter(|bucket| read(*bucket) > 0.0).collect();
        let expected: Vec<usize> = (0..HALF / 8).chain(HALF..HALF + HALF / 8).collect();
        assert_eq!(lit, expected, "wrong buckets lit");

        // What arrived, unducked.
        assert!(
            (read(0) - 0.5).abs() < 1e-3,
            "input bucket 0 shows {}",
            read(0)
        );
        // What left: the beat is where the duck is deepest, so this is the
        // assertion that would have caught the display showing nothing.
        assert!(
            read(HALF) < read(0) * 0.2,
            "output bucket 0 shows {} against an input of {} — the duck is invisible",
            read(HALF),
            read(0),
        );
    }

    /// A bucket holds the loudest sample of the pass, not the last few. The
    /// release fires once per pass; per sample it would leave a thin wobble
    /// where the envelope should be.
    #[test]
    fn a_scope_bucket_holds_the_peak_of_the_pass() {
        use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

        let scope: [AtomicU32; seco_clap::SCOPE_BUCKETS] =
            std::array::from_fn(|_| AtomicU32::new(0));
        let mut plugin = Zape::new();
        plugin.activate(48_000.0, 4096);

        // One bucket's worth of samples: loud at the start, silent after.
        let frames = 187;
        let mut left = vec![0.0_f32; frames];
        left[0] = 0.9;
        let mut right = vec![0.0_f32; frames];
        with_rt_context(transport_at(8.0), &[2.0, 100.0, 0.0, 0.0], &scope, |rt| {
            let mut channels: [&mut [f32]; 2] = [&mut left[..], &mut right[..]];
            let mut audio = AudioBuffer::new(&mut channels);
            plugin.process(&mut audio, rt);
        });

        let peak = f32::from_bits(scope[0].load(Relaxed));
        assert!(
            (peak - 0.9).abs() < 1e-3,
            "bucket 0 kept {peak}, not the peak of the pass"
        );
    }

    /// The complaint that started this: the duck has to be *on* the beat.
    /// The old shapes sat at unity at phase 0 and fell afterwards, so the
    /// transient passed at full level and was then cut — ~5 ms above 0.9
    /// gain at 95 BPM, rate 1/2. Nothing near the beat may be loud.
    #[test]
    fn the_floor_lands_on_the_beat_not_after_it() {
        let mut plugin = Zape::new();
        plugin.activate(48_000.0, 512);
        // Rate 1/4 (one beat per cycle), Pump, full mix: ppq 8.0 is a beat.
        let params = [2.0, 100.0, 0.0, 0.0];
        // A block that ends exactly on the beat, so the entry fade runs
        // inside it, then a block that starts on the beat.
        const BLOCK: usize = 512;
        let beats_per_sample = 120.0 / 60.0 / 48_000.0;
        let approach = run_block_with(
            &mut plugin,
            &params,
            8.0 - BLOCK as f64 * beats_per_sample,
            BLOCK,
        );
        let on_beat = run_block_with(&mut plugin, &params, 8.0, 64);

        assert!(
            on_beat[0] < 0.1,
            "gain {} at the beat — the duck is late",
            on_beat[0]
        );
        // And the fade must have done the work before the beat, not after.
        let last = *approach.last().unwrap();
        assert!(
            last < 0.15,
            "gain {last} one sample before the beat — the entry never ran"
        );
    }

    /// Hosts with flush-on-transport-change call reset() around jumps. That
    /// must glide like any other jump — snapping there is a one-sample
    /// click (measured 0.424 before the fix).
    #[test]
    fn reset_plus_jump_glides_instead_of_clicking() {
        let mut plugin = Zape::new();
        plugin.activate(48_000.0, 512);
        let params = [2.0, 100.0, 2.0, 0.0];
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

        let expected = duck::soft().gain(0.25, TEST_ATTACK);
        assert!(
            expected < 0.2,
            "test premise: Soft at phase 0.25 must duck deep"
        );
        assert!(
            (out[0] - expected).abs() < 1e-4,
            "first sample {} must sit on the curve ({expected}), not near 1.0",
            out[0]
        );
    }

    /// The snap is initial-state only. A transport jump (loop wrap, seek)
    /// must keep the limiter's ramp — snapping there would reintroduce
    /// the seam click as a resync click.
    #[test]
    fn transport_jump_glides_snap_only_on_start() {
        let mut plugin = Zape::new();
        plugin.activate(48_000.0, 512);
        plugin.reset();

        let first = run_block(&mut plugin, 10.25, 64);
        // Jump far away on the curve (phase 0.25 → 0.6, gain ~0.1 → ~0.6).
        let second = run_block(&mut plugin, 20.6, 64);

        let jump_target = duck::soft().gain(0.6, TEST_ATTACK);
        assert!(
            jump_target > 0.5,
            "test premise: jump lands on a high-gain phase"
        );
        let boundary_step = (second[0] - first[63]).abs();
        assert!(
            boundary_step < 0.02,
            "jump must glide from {} (stepped {boundary_step} toward {jump_target})",
            first[63]
        );
    }
}
