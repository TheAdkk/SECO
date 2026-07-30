//! Neta — a loudness meter.
//!
//! *La neta* is the truth, and that is the whole product: what a mix
//! actually measures, when three hours of listening have stopped being
//! evidence.
//!
//! This crate is the CLAP shell. The measurement lives in `neta-meter`,
//! which knows nothing about plugins or hosts, so a standalone application
//! can be a second shell around the same numbers rather than a second
//! implementation of them.
//!
//! The full plan — what it contains, where the audio comes from, why the
//! renderer is not a webview, and how `dalia` links in — is in
//! `docs/neta/PLAN.md`.
//!
//! # Where it stands
//!
//! It loads, passes audio through untouched, and feeds every block to the
//! standards-based `neta-meter` core. It also publishes a bounded picture for
//! the compatibility editor; native WGPU rendering lives in `neta-wgpu`.
//!
//! # What the framework still owes this plugin
//!
//! - `seco-clap` hard-codes one stereo input and one stereo output. A meter
//!   wants an input and no output at all, and a mastering meter wants more
//!   than two channels. Passing through is correct for an insert meter, so
//!   this is a ceiling rather than a blocker today.
//! - Native GPU embedding into an arbitrary DAW child view still needs the
//!   platform adapter described in `ADR-001`; the compatibility editor is
//!   macOS-only today.

#![forbid(unsafe_code)]

mod editor;
mod model;
mod settings;

use neta_meter::scope::{
    OscilloscopeCycles as MeterOscilloscopeCycles, OscilloscopeMode as MeterOscilloscopeMode,
    OscilloscopeSample, ScopeAnalyzer, ScopeConfig, ScopeWork, SpectrumScale as MeterSpectrumScale,
};
use neta_meter::{
    LoudnessHistogramBin, LoudnessHistogramKind, LoudnessMeter, RealtimeMeterSnapshot,
    VuChannelSnapshot, VuMeter, VuMode as MeterVuMode, VuSnapshot,
};
use seco_clap::seco_export;
use seco_core::{AudioBuffer, EditorPage, ParamDesc, ParamRange, Plugin, RtContext};

/// Sentinel for a gated value the programme has not produced yet.
///
/// Far below any real loudness, and finite so `set_scope` accepts it — a
/// NaN would be dropped and leave the slot reading as a measurement.
pub(crate) const UNMEASURED: f32 = -200.0;

/// Fixed live-picture vocabulary. The editor reads these positions directly;
/// keep the map append-only and review it with `editor.rs` together.
const NETA_SCOPE_SLOTS: usize = 896;
const SCOPE_METRICS: usize = 0;
const SCOPE_METRICS_END: usize = 32;
const SCOPE_TIMELINE: usize = 32;
const SCOPE_HISTORY_POINTS: usize = 64;
const SCOPE_TIMELINE_MOMENTARY: usize = SCOPE_TIMELINE;
const SCOPE_TIMELINE_SHORT_TERM: usize = SCOPE_TIMELINE_MOMENTARY + SCOPE_HISTORY_POINTS;
const SCOPE_TIMELINE_INTEGRATED: usize = SCOPE_TIMELINE_SHORT_TERM + SCOPE_HISTORY_POINTS;
const SCOPE_HISTOGRAM: usize = SCOPE_TIMELINE_INTEGRATED + SCOPE_HISTORY_POINTS;
const SCOPE_HISTOGRAM_POINTS: usize = 64;
const SCOPE_WAVE_MIN: usize = SCOPE_HISTOGRAM + SCOPE_HISTOGRAM_POINTS;
const SCOPE_WAVE_MAX: usize = SCOPE_WAVE_MIN + SCOPE_HISTORY_POINTS;
const SCOPE_WAVE_POINTS: usize = SCOPE_HISTORY_POINTS;
const SCOPE_OSCILLOSCOPE: usize = SCOPE_WAVE_MAX + SCOPE_WAVE_POINTS;
/// 128 original stereo samples, packed as L/R pairs in 256 slots.
const SCOPE_OSCILLOSCOPE_POINTS: usize = 128;
const SCOPE_SPECTRUM: usize = SCOPE_OSCILLOSCOPE + SCOPE_OSCILLOSCOPE_POINTS * 2;
const SCOPE_SPECTRUM_POINTS: usize = 96;
const SCOPE_GONIOMETER: usize = SCOPE_SPECTRUM + SCOPE_SPECTRUM_POINTS;
const SCOPE_GONIOMETER_POINTS: usize = 64;

const FFT_SIZES: [usize; 5] = [1_024, 2_048, 4_096, 8_192, 16_384];
const MODULE_LOUDNESS: usize = 0;
const MODULE_SPECTRUM: usize = 1;
const MODULE_SPECTROGRAM: usize = 2;
const MODULE_WAVEFORM: usize = 3;
const MODULE_STEREO: usize = 4;
const MODULE_OBJECT: usize = 5;
const MODULE_VISUALS: usize = 6;
const MODULE_VU: usize = 7;
const MODULE_OSCILLOSCOPE: usize = 8;
const MODULE_COUNT: usize = 9;

/// Counter parameters deliberately do not store a command. Hosts persist a
/// number, and Neta acts only on a change, so Reset/Capture cannot fire again
/// merely because a project is reopened.
const PARAM_RESET: usize = 0;
const PARAM_CAPTURE_A: usize = 1;
const PARAM_CAPTURE_B: usize = 2;
const COUNTER_LIMIT: f64 = 65_535.0;

const _: () = {
    assert!(SCOPE_METRICS == 0);
    assert!(SCOPE_METRICS_END == SCOPE_TIMELINE);
    assert!(SCOPE_TIMELINE_INTEGRATED + SCOPE_HISTORY_POINTS == SCOPE_HISTOGRAM);
    assert!(SCOPE_HISTOGRAM + SCOPE_HISTOGRAM_POINTS == SCOPE_WAVE_MIN);
    assert!(SCOPE_WAVE_MIN + SCOPE_WAVE_POINTS == SCOPE_WAVE_MAX);
    assert!(SCOPE_WAVE_MAX + SCOPE_WAVE_POINTS == SCOPE_OSCILLOSCOPE);
    assert!(SCOPE_OSCILLOSCOPE + SCOPE_OSCILLOSCOPE_POINTS * 2 == SCOPE_SPECTRUM);
    assert!(SCOPE_SPECTRUM + SCOPE_SPECTRUM_POINTS == SCOPE_GONIOMETER);
    assert!(SCOPE_GONIOMETER + SCOPE_GONIOMETER_POINTS * 2 == NETA_SCOPE_SLOTS);
    assert!(NETA_SCOPE_SLOTS <= seco_clap::MAX_SCOPE_BUCKETS);
};

/// Five preallocated FFT-only banks. Non-FFT views live in one separate
/// display analyzer, so selecting an FFT size cannot rewind waveform,
/// vectorscope, or oscilloscope history.
struct ScopeBanks {
    banks: [Option<ScopeAnalyzer>; FFT_SIZES.len()],
    active: usize,
    /// A source/trim/reset invalidates every bank logically. Clear inactive
    /// storage lazily when it is selected rather than memset-ing five large
    /// analyzers inside one audio callback.
    generation: u32,
    bank_generation: [u32; FFT_SIZES.len()],
}

impl ScopeBanks {
    fn new(sample_rate: f64, max_frames: usize) -> Option<Self> {
        let mut banks: [Option<ScopeAnalyzer>; FFT_SIZES.len()] = std::array::from_fn(|_| None);
        for (index, fft_size) in FFT_SIZES.into_iter().enumerate() {
            let mut config = fft_scope_config(sample_rate, max_frames)?;
            config.fft_size = fft_size;
            config.fft_hop_frames = fft_size / 2;
            config.spectrum_bands = SCOPE_SPECTRUM_POINTS;
            banks[index] = Some(ScopeAnalyzer::new(config).ok()?);
        }
        Some(Self {
            banks,
            active: 1,
            generation: 1,
            bank_generation: [0; FFT_SIZES.len()],
        })
    }

    fn active(&self) -> Option<&ScopeAnalyzer> {
        self.banks.get(self.active)?.as_ref()
    }

    fn active_mut(&mut self) -> Option<&mut ScopeAnalyzer> {
        self.banks.get_mut(self.active)?.as_mut()
    }

    fn select(&mut self, index: usize) -> Option<&mut ScopeAnalyzer> {
        let index = index.min(FFT_SIZES.len() - 1);
        if self.active != index {
            self.active = index;
            let stale = self.bank_generation[index] != self.generation;
            if stale {
                self.active_mut()?.reset();
                self.bank_generation[index] = self.generation;
            } else {
                // An FFT-size choice only invalidates FFT-derived views.
                self.active_mut()?.reset_work(ScopeWork::SPECTRUM);
            }
        }
        self.active_mut()
    }

    fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            // A wrap is fantastically unlikely, but zero is the stale epoch
            // used at construction. Make this reset unambiguous.
            self.generation = 1;
            self.bank_generation = [0; FFT_SIZES.len()];
        }
        let active = self.active;
        if let Some(scope) = self.active_mut() {
            scope.reset();
            self.bank_generation[active] = self.generation;
        }
    }
}

/// The visible waveform/stereo/scope analyzer never receives FFT work. Keep
/// its unused spectrum storage tiny while retaining full raw-scope history
/// for safe pitch triggering.
fn display_scope(sample_rate: f64, max_frames: usize) -> Option<ScopeAnalyzer> {
    let mut config = ScopeConfig::recommended(sample_rate, max_frames).ok()?;
    config.fft_size = 16;
    config.fft_hop_frames = 16;
    config.spectrum_bands = 1;
    config.spectrogram_rows = 1;
    ScopeAnalyzer::new(config).ok()
}

/// Spectrum banks never process correlation, waveform, goniometer, or raw
/// scope data. Construct their dormant representations at the smallest valid
/// capacity; all five preallocations stay focused on FFT work and history.
fn fft_scope_config(sample_rate: f64, max_frames: usize) -> Option<ScopeConfig> {
    let mut config = ScopeConfig::recommended(sample_rate, max_frames).ok()?;
    let one_frame = 1.0 / sample_rate;
    config.correlation_window_seconds = one_frame;
    config.waveform_buckets = 1;
    config.waveform_seconds = one_frame;
    config.goniometer_points = 1;
    config.goniometer_decimation_frames = 1;
    config.oscilloscope_frames = 1;
    Some(config)
}

#[derive(Clone, Copy)]
struct LoudnessTimeline {
    momentary: [f32; SCOPE_HISTORY_POINTS],
    short_term: [f32; SCOPE_HISTORY_POINTS],
    integrated: [f32; SCOPE_HISTORY_POINTS],
    write: usize,
    count: usize,
}

impl Default for LoudnessTimeline {
    fn default() -> Self {
        Self {
            momentary: [UNMEASURED; SCOPE_HISTORY_POINTS],
            short_term: [UNMEASURED; SCOPE_HISTORY_POINTS],
            integrated: [UNMEASURED; SCOPE_HISTORY_POINTS],
            write: 0,
            count: 0,
        }
    }
}

impl LoudnessTimeline {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn push(&mut self, meter: RealtimeMeterSnapshot) {
        self.momentary[self.write] = option_f64(meter.momentary_lufs, UNMEASURED);
        self.short_term[self.write] = option_f64(meter.short_term_lufs, UNMEASURED);
        self.integrated[self.write] = option_f64(meter.integrated_lufs, UNMEASURED);
        self.write = (self.write + 1) % SCOPE_HISTORY_POINTS;
        self.count = (self.count + 1).min(SCOPE_HISTORY_POINTS);
    }

    fn value(&self, series: &[f32; SCOPE_HISTORY_POINTS], chronological: usize) -> f32 {
        if chronological >= self.count {
            return UNMEASURED;
        }
        let start = if self.count < SCOPE_HISTORY_POINTS {
            0
        } else {
            self.write
        };
        series[(start + chronological) % SCOPE_HISTORY_POINTS]
    }
}

#[derive(Clone, Copy, Default)]
struct Capture {
    valid: bool,
    short_term: f32,
    integrated: f32,
    true_peak: f32,
}

impl Capture {
    fn take(&mut self, meter: RealtimeMeterSnapshot) {
        self.valid = true;
        self.short_term = option_f64(meter.short_term_lufs, UNMEASURED);
        self.integrated = option_f64(meter.integrated_lufs, UNMEASURED);
        self.true_peak = option_f64(meter.max_true_peak_dbfs, UNMEASURED);
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

struct Neta {
    /// One measurement run, with A/B as immutable captures rather than two
    /// simultaneous meters.
    meter: Option<LoudnessMeter>,
    vu: Option<VuMeter>,
    /// Waveform/stereo/oscilloscope state. Its input is masked so it never
    /// runs an FFT and survives an independent FFT-bank selection.
    display_scope: Option<ScopeAnalyzer>,
    scopes: Option<ScopeBanks>,
    /// Analysis-only scratch. Output slices are never written.
    scratch_left: Vec<f32>,
    scratch_right: Vec<f32>,
    scratch_silence: Vec<f32>,
    runtime: settings::RuntimeSettings,
    runtime_dirty: bool,
    runtime_configured: bool,
    applied_modules: [bool; MODULE_COUNT],
    applied_source: u8,
    applied_trim_db: f32,
    applied_fft: usize,
    applied_scale: u8,
    applied_smoothing: u8,
    applied_vu_mode: u8,
    applied_vu_calibration: f32,
    applied_timeline: bool,
    applied_histogram: bool,
    analysis_gain: f32,
    timeline: LoudnessTimeline,
    frames_until_timeline: usize,
    timeline_interval: usize,
    frames_until_publish: usize,
    publish_interval: usize,
    capture_a: Capture,
    capture_b: Capture,
    reset_counter: u16,
    capture_a_counter: u16,
    capture_b_counter: u16,
    actions_primed: bool,
}

impl Plugin for Neta {
    const ID: &'static str = "dev.seco.neta";
    const NAME: &'static str = "Neta";
    const VENDOR: &'static str = "SECO";
    const VERSION: &'static str = "0.1.0";
    const DESCRIPTION: &'static str = "Loudness meter";
    const EDITOR: Option<EditorPage> = Some(editor::PAGE);

    const PARAMS: &'static [ParamDesc] = &[
        ParamDesc {
            name: "Reset",
            range: ParamRange::Continuous {
                min: 0.0,
                max: COUNTER_LIMIT,
                default: 0.0,
                unit: "",
                decimals: 0,
            },
        },
        ParamDesc {
            name: "Capture A",
            range: ParamRange::Continuous {
                min: 0.0,
                max: COUNTER_LIMIT,
                default: 0.0,
                unit: "",
                decimals: 0,
            },
        },
        ParamDesc {
            name: "Capture B",
            range: ParamRange::Continuous {
                min: 0.0,
                max: COUNTER_LIMIT,
                default: 0.0,
                unit: "",
                decimals: 0,
            },
        },
    ];

    const SCOPE_SLOTS: usize = NETA_SCOPE_SLOTS;
    const EDITOR_REFRESH_HZ: u32 = 60;

    fn new() -> Self {
        Neta {
            meter: None,
            vu: None,
            display_scope: None,
            scopes: None,
            scratch_left: Vec::new(),
            scratch_right: Vec::new(),
            scratch_silence: Vec::new(),
            runtime: settings::RuntimeSettings::default(),
            runtime_dirty: true,
            runtime_configured: false,
            applied_modules: [false; MODULE_COUNT],
            applied_source: 0,
            applied_trim_db: 0.0,
            applied_fft: 1,
            applied_scale: 0,
            applied_smoothing: 90,
            applied_vu_mode: 0,
            applied_vu_calibration: -18.0,
            applied_timeline: true,
            applied_histogram: true,
            analysis_gain: 1.0,
            timeline: LoudnessTimeline::default(),
            frames_until_timeline: 1,
            timeline_interval: 1,
            frames_until_publish: 1,
            publish_interval: 1,
            capture_a: Capture::default(),
            capture_b: Capture::default(),
            reset_counter: 0,
            capture_a_counter: 0,
            capture_b_counter: 0,
            actions_primed: false,
        }
    }

    fn activate(&mut self, sample_rate: f64, max_frames: u32) {
        let max_frames = max_frames as usize;
        self.meter = LoudnessMeter::stereo(sample_rate, max_frames).ok();
        self.vu = VuMeter::stereo(sample_rate, max_frames).ok();
        self.display_scope = display_scope(sample_rate, max_frames);
        self.scopes = ScopeBanks::new(sample_rate, max_frames);
        self.scratch_left.clear();
        self.scratch_left.resize(max_frames, 0.0);
        self.scratch_right.clear();
        self.scratch_right.resize(max_frames, 0.0);
        self.scratch_silence.clear();
        self.scratch_silence.resize(max_frames, 0.0);
        self.timeline_interval = (sample_rate / 10.0).round().max(1.0) as usize;
        self.publish_interval = (sample_rate / Self::EDITOR_REFRESH_HZ as f64)
            .round()
            .max(1.0) as usize;
        self.frames_until_timeline = 1;
        self.frames_until_publish = 1;
        self.runtime_dirty = true;
        self.runtime_configured = false;
        self.applied_modules = [false; MODULE_COUNT];
        self.actions_primed = false;
        self.reset_analysis();
    }

    fn deactivate(&mut self) {
        self.meter = None;
        self.vu = None;
        self.display_scope = None;
        self.scopes = None;
        self.scratch_left.clear();
        self.scratch_right.clear();
        self.scratch_silence.clear();
    }

    fn reset(&mut self) {
        self.reset_analysis();
    }

    fn apply_state(&mut self, state: &[u8], _rt: &RtContext) {
        // RuntimeSettings borrows/parses fixed fields only. This is called
        // before a callback, but follows audio-thread rules by construction.
        self.runtime = settings::RuntimeSettings::parse(state);
        self.runtime_dirty = true;
    }

    fn process(&mut self, audio: &mut AudioBuffer, rt: &RtContext) {
        if self.runtime_dirty {
            self.apply_runtime();
        }
        self.handle_actions(rt);

        let loudness_enabled = self.module_enabled(MODULE_LOUDNESS);
        let vu_enabled = self.module_enabled(MODULE_VU);
        let waveform_enabled = self.module_enabled(MODULE_WAVEFORM);
        let stereo_enabled = self.module_enabled(MODULE_STEREO);
        let oscilloscope_enabled = self.module_enabled(MODULE_OSCILLOSCOPE);
        let spectrum_visible =
            self.module_enabled(MODULE_SPECTRUM) || self.module_enabled(MODULE_SPECTROGRAM);
        let scope_work = self.scope_work();
        let display_work = scope_work.without(ScopeWork::SPECTRUM);
        let spectrum_enabled = scope_work.contains(ScopeWork::SPECTRUM);
        if !loudness_enabled && !vu_enabled && scope_work == ScopeWork::NONE {
            return;
        }

        let frames = audio
            .frames()
            .min(self.scratch_left.len())
            .min(self.scratch_right.len());
        if frames == 0 {
            return;
        }

        let mut channels = audio.channels();
        let left = channels.next();
        let right = channels.next();
        self.copy_analysis_input(left, right, frames);

        let meter_snapshot = if loudness_enabled {
            self.process_loudness(frames)
        } else {
            None
        };

        if vu_enabled {
            if let Some(vu) = &mut self.vu {
                let result =
                    vu.process_stereo(&self.scratch_left[..frames], &self.scratch_right[..frames]);
                debug_assert!(
                    result.is_ok(),
                    "VU input violated activate contract: {result:?}"
                );
            }
        }

        if display_work != ScopeWork::NONE {
            if let Some(scope) = &mut self.display_scope {
                let result = scope.process_stereo_masked(
                    &self.scratch_left[..frames],
                    &self.scratch_right[..frames],
                    display_work,
                );
                debug_assert!(
                    result.is_ok(),
                    "display scope input violated activate contract: {result:?}"
                );
            }
        }

        if spectrum_enabled {
            if let Some(scope) = self.scopes.as_mut().and_then(ScopeBanks::active_mut) {
                let result = scope.process_stereo_masked(
                    &self.scratch_left[..frames],
                    &self.scratch_right[..frames],
                    ScopeWork::SPECTRUM,
                );
                debug_assert!(
                    result.is_ok(),
                    "spectrum scope input violated activate contract: {result:?}"
                );
            }
        }

        if self.runtime.loudness_timeline {
            if let Some(meter) = meter_snapshot {
                if interval_due(
                    &mut self.frames_until_timeline,
                    self.timeline_interval,
                    frames,
                ) {
                    self.timeline.push(meter);
                }
            }
        }

        if interval_due(
            &mut self.frames_until_publish,
            self.publish_interval,
            frames,
        ) {
            let display_scope = self.display_scope.as_ref();
            let spectrum_scope = self.scopes.as_ref().and_then(ScopeBanks::active);
            let vu = if vu_enabled {
                self.vu.as_ref().map(VuMeter::snapshot)
            } else {
                None
            };
            // Loudness owns retained histogram/peak data. When its module is
            // hidden, do not even resample that history for a frame requested
            // by another module.
            let meter_core = if loudness_enabled {
                self.meter.as_ref()
            } else {
                None
            };
            publish_picture(
                rt,
                meter_snapshot,
                meter_core,
                vu,
                display_scope,
                spectrum_scope,
                self.timeline,
                self.capture_a,
                self.capture_b,
                self.target_lufs(),
                self.source_id(),
                self.applied_fft,
                self.module_mask(),
                loudness_enabled,
                waveform_enabled,
                stereo_enabled,
                oscilloscope_enabled,
                spectrum_visible,
                self.oscilloscope_mode(),
                self.oscilloscope_cycles(),
                loudness_enabled && self.runtime.loudness_timeline,
                loudness_enabled && self.runtime.loudness_histogram,
                loudness_enabled && self.runtime.loudness_overs,
            );
        }
    }

    fn editor_frame(scope: &[f32]) -> Option<String> {
        editor::frame(scope)
    }

    fn editor_script(_params: &[f64], state: &[u8]) -> Option<String> {
        let block = core::str::from_utf8(state).unwrap_or_default();
        Some(settings::Settings::parse(block).to_script())
    }

    fn editor_message(text: &str) -> Option<String> {
        text.strip_prefix("model ").map(model::load)
    }
}

impl Neta {
    fn reset_analysis(&mut self) {
        if let Some(meter) = &mut self.meter {
            meter.reset();
        }
        if let Some(vu) = &mut self.vu {
            vu.reset();
        }
        if let Some(scope) = &mut self.display_scope {
            scope.reset();
        }
        if let Some(scopes) = &mut self.scopes {
            scopes.reset();
        }
        self.timeline.reset();
        self.capture_a.reset();
        self.capture_b.reset();
        self.frames_until_timeline = 1;
        self.frames_until_publish = 1;
    }

    fn apply_runtime(&mut self) {
        let source = self.source_id();
        let trim_db = self.runtime.trim_db as f32;
        let fft = self.runtime.fft_index().min(FFT_SIZES.len() - 1);
        let scale = self.spectrum_scale();
        let scale_id = spectrum_scale_id(scale);
        let smoothing = self.runtime.spectrum_smoothing;
        let vu_mode = self.vu_mode();
        let vu_mode_id = vu_mode_id(vu_mode);
        let vu_calibration = self.runtime.vu_calibration as f32;
        let timeline_enabled = self.runtime.loudness_timeline;
        let histogram_enabled = self.runtime.loudness_histogram;
        let analysis_changed = !self.runtime_configured
            || self.applied_source != source
            || (self.applied_trim_db - trim_db).abs() > f32::EPSILON;
        let fft_changed = !self.runtime_configured || self.applied_fft != fft;

        self.analysis_gain = 10.0_f32.powf(trim_db.clamp(-24.0, 24.0) / 20.0);
        if analysis_changed {
            self.reset_analysis();
        }
        if let Some(scopes) = &mut self.scopes {
            if fft_changed {
                let _ = scopes.select(fft);
            }
            if let Some(scope) = scopes.active_mut() {
                if fft_changed || !self.runtime_configured || self.applied_scale != scale_id {
                    scope.set_spectrum_scale(scale);
                }
                if fft_changed || !self.runtime_configured || self.applied_smoothing != smoothing {
                    // The setting is a display percentage. Keep attack quick
                    // and extend release as smoothing rises; both setters
                    // change coefficients only, never capacity.
                    let release_seconds = 0.01 + f64::from(smoothing) * 0.012;
                    let _ = scope.set_spectrum_ballistics(0.015, release_seconds);
                }
            }
        }
        if let Some(vu) = &mut self.vu {
            if !self.runtime_configured || self.applied_vu_mode != vu_mode_id {
                vu.set_mode(vu_mode);
            }
            if !self.runtime_configured
                || (self.applied_vu_calibration - vu_calibration).abs() > f32::EPSILON
            {
                let _ = vu.set_calibration_dbfs(f64::from(vu_calibration));
            }
        }

        for module in 0..MODULE_COUNT {
            let enabled = self.module_enabled(module);
            if enabled && !self.applied_modules[module] {
                self.reset_reenabled_module(module);
            }
            self.applied_modules[module] = enabled;
        }
        if !self.runtime_configured || self.applied_timeline != timeline_enabled {
            self.timeline.reset();
            self.frames_until_timeline = 1;
        }
        if self.runtime_configured && !self.applied_histogram && histogram_enabled {
            // A histogram is a retained view. Returning from Off starts a
            // clean programme distribution instead of reviving old bars.
            if let Some(meter) = &mut self.meter {
                meter.reset();
            }
            self.timeline.reset();
            self.capture_a.reset();
            self.capture_b.reset();
        }
        self.applied_source = source;
        self.applied_trim_db = trim_db;
        self.applied_fft = fft;
        self.applied_scale = scale_id;
        self.applied_smoothing = smoothing;
        self.applied_vu_mode = vu_mode_id;
        self.applied_vu_calibration = vu_calibration;
        self.applied_timeline = timeline_enabled;
        self.applied_histogram = histogram_enabled;
        self.runtime_configured = true;
        self.runtime_dirty = false;
    }

    fn reset_reenabled_module(&mut self, module: usize) {
        match module {
            MODULE_LOUDNESS => {
                if let Some(meter) = &mut self.meter {
                    meter.reset();
                }
                self.timeline.reset();
                self.capture_a.reset();
                self.capture_b.reset();
            }
            MODULE_VU => {
                if let Some(vu) = &mut self.vu {
                    vu.reset();
                }
            }
            _ => {
                if let Some(work) = module_scope_work(module) {
                    let display_work = work.without(ScopeWork::SPECTRUM);
                    if display_work != ScopeWork::NONE {
                        if let Some(scope) = &mut self.display_scope {
                            scope.reset_work(display_work);
                        }
                    }
                    if work.contains(ScopeWork::SPECTRUM) {
                        if let Some(scope) = self.scopes.as_mut().and_then(ScopeBanks::active_mut) {
                            scope.reset_work(ScopeWork::SPECTRUM);
                        }
                    }
                }
            }
        }
    }

    fn handle_actions(&mut self, rt: &RtContext) {
        let reset = action_counter(rt.param(PARAM_RESET));
        let capture_a = action_counter(rt.param(PARAM_CAPTURE_A));
        let capture_b = action_counter(rt.param(PARAM_CAPTURE_B));
        // Counter values are persisted by hosts. Seed them once rather than
        // treating a reopened project's old non-zero value as a new click.
        if !self.actions_primed {
            self.reset_counter = reset;
            self.capture_a_counter = capture_a;
            self.capture_b_counter = capture_b;
            self.actions_primed = true;
            return;
        }
        if reset != self.reset_counter {
            self.reset_counter = reset;
            self.reset_analysis();
        }
        if capture_a != self.capture_a_counter {
            self.capture_a_counter = capture_a;
            if self.module_enabled(MODULE_LOUDNESS) {
                if let Some(meter) = &self.meter {
                    self.capture_a.take(meter.realtime_snapshot());
                }
            }
        }
        if capture_b != self.capture_b_counter {
            self.capture_b_counter = capture_b;
            if self.module_enabled(MODULE_LOUDNESS) {
                if let Some(meter) = &self.meter {
                    self.capture_b.take(meter.realtime_snapshot());
                }
            }
        }
    }

    fn copy_analysis_input(&mut self, left: Option<&[f32]>, right: Option<&[f32]>, frames: usize) {
        let gain = self.analysis_gain;
        for index in 0..frames {
            let left = finite_sample(left.and_then(|channel| channel.get(index)).copied());
            // Neta declares a stereo bus. If a broken host gives us mono,
            // missing R means silence — not a duplicate that inflates
            // loudness by roughly 3 LU.
            let right = finite_sample(right.and_then(|channel| channel.get(index)).copied());
            let (analysis_left, analysis_right) = match self.runtime.analysis_source {
                settings::AnalysisSource::Stereo => (left, right),
                settings::AnalysisSource::Left => (left, left),
                settings::AnalysisSource::Right => (right, right),
                settings::AnalysisSource::Mid => {
                    let mid = (left + right) * 0.5;
                    (mid, mid)
                }
                settings::AnalysisSource::Side => {
                    let side = (left - right) * 0.5;
                    (side, side)
                }
            };
            self.scratch_left[index] = analysis_left * gain;
            self.scratch_right[index] = analysis_right * gain;
        }
    }

    fn process_loudness(&mut self, frames: usize) -> Option<RealtimeMeterSnapshot> {
        let stereo = matches!(
            self.runtime.analysis_source,
            settings::AnalysisSource::Stereo
        );
        let meter = self.meter.as_mut()?;
        let right = if stereo {
            &self.scratch_right[..frames]
        } else {
            &self.scratch_silence[..frames]
        };
        let result = meter.process_block([&self.scratch_left[..frames], right].into_iter());
        debug_assert!(
            result.is_ok(),
            "loudness input violated activate contract: {result:?}"
        );
        Some(meter.realtime_snapshot())
    }

    fn module_enabled(&self, module: usize) -> bool {
        self.runtime.module_enabled(module)
    }

    fn scope_work(&self) -> ScopeWork {
        let mut work = ScopeWork::NONE;
        if self.module_enabled(MODULE_WAVEFORM) {
            work = work.with(ScopeWork::WAVEFORM);
        }
        if self.module_enabled(MODULE_STEREO) {
            work = work
                .with(ScopeWork::CORRELATION)
                .with(ScopeWork::GONIOMETER);
        }
        if self.module_enabled(MODULE_OSCILLOSCOPE) {
            work = work.with(ScopeWork::OSCILLOSCOPE);
        }
        if !self.runtime.spectrum_hold
            && (self.module_enabled(MODULE_SPECTRUM) || self.module_enabled(MODULE_SPECTROGRAM))
        {
            work = work.with(ScopeWork::SPECTRUM);
        }
        work
    }

    fn source_id(&self) -> u8 {
        match self.runtime.analysis_source {
            settings::AnalysisSource::Stereo => 0,
            settings::AnalysisSource::Left => 1,
            settings::AnalysisSource::Right => 2,
            settings::AnalysisSource::Mid => 3,
            settings::AnalysisSource::Side => 4,
        }
    }

    fn spectrum_scale(&self) -> MeterSpectrumScale {
        match self.runtime.spectrum_scale {
            settings::SpectrumScale::Log => MeterSpectrumScale::Log,
            settings::SpectrumScale::Mel => MeterSpectrumScale::Mel,
            settings::SpectrumScale::Linear => MeterSpectrumScale::Linear,
        }
    }

    fn vu_mode(&self) -> MeterVuMode {
        match self.runtime.vu_mode {
            settings::VuMode::Vu => MeterVuMode::Vu,
            settings::VuMode::Rms => MeterVuMode::Rms,
            settings::VuMode::Peak => MeterVuMode::Peak,
        }
    }

    fn oscilloscope_mode(&self) -> MeterOscilloscopeMode {
        match self.runtime.oscilloscope_mode {
            settings::OscilloscopeMode::Pitch => MeterOscilloscopeMode::Pitch,
            settings::OscilloscopeMode::Free => MeterOscilloscopeMode::Free,
        }
    }

    fn oscilloscope_cycles(&self) -> MeterOscilloscopeCycles {
        match self.runtime.oscilloscope_cycles {
            settings::OscilloscopeCycles::Single => MeterOscilloscopeCycles::Single,
            settings::OscilloscopeCycles::Multi => MeterOscilloscopeCycles::Multi,
        }
    }

    fn target_lufs(&self) -> f32 {
        self.runtime.target_lufs as f32
    }

    fn module_mask(&self) -> f32 {
        let mut mask = 0_u16;
        for module in 0..MODULE_COUNT {
            if self.module_enabled(module) {
                mask |= 1 << module;
            }
        }
        f32::from(mask)
    }
}

/// Copies a deliberately bounded picture from preallocated analysis state to
/// the adapter's lock-free visualization slots. One dropped/mixed frame is
/// preferable to one delayed audio callback.
#[allow(clippy::too_many_arguments)]
fn publish_picture(
    rt: &RtContext,
    meter: Option<RealtimeMeterSnapshot>,
    meter_core: Option<&LoudnessMeter>,
    vu: Option<VuSnapshot>,
    display_scope: Option<&ScopeAnalyzer>,
    spectrum_scope: Option<&ScopeAnalyzer>,
    timeline: LoudnessTimeline,
    capture_a: Capture,
    capture_b: Capture,
    target_lufs: f32,
    source_id: u8,
    fft_index: usize,
    module_mask: f32,
    loudness_enabled: bool,
    waveform_enabled: bool,
    stereo_enabled: bool,
    oscilloscope_enabled: bool,
    spectrum_visible: bool,
    oscilloscope_mode: MeterOscilloscopeMode,
    oscilloscope_cycles: MeterOscilloscopeCycles,
    timeline_enabled: bool,
    histogram_enabled: bool,
    overs_enabled: bool,
) {
    let meter = meter.unwrap_or(RealtimeMeterSnapshot {
        momentary_lufs: None,
        short_term_lufs: None,
        max_true_peak_dbfs: None,
        psr_db: None,
        integrated_lufs: None,
        loudness_range_lu: None,
        plr_db: None,
        processed_frames: 0,
    });
    let snapshot = if stereo_enabled {
        display_scope.map(ScopeAnalyzer::snapshot)
    } else {
        None
    };
    let vu = vu.unwrap_or_else(empty_vu_snapshot);
    rt.set_scope(0, option_f64(meter.momentary_lufs, UNMEASURED));
    rt.set_scope(1, option_f64(meter.short_term_lufs, UNMEASURED));
    rt.set_scope(
        2,
        if loudness_enabled && overs_enabled {
            option_f64(meter.max_true_peak_dbfs, UNMEASURED)
        } else {
            UNMEASURED
        },
    );
    rt.set_scope(
        3,
        snapshot.and_then(|value| value.correlation).unwrap_or(0.0),
    );
    rt.set_scope(
        4,
        snapshot.and_then(|value| value.stereo_width).unwrap_or(0.0),
    );
    rt.set_scope(
        5,
        snapshot
            .and_then(|value| value.left_rms_dbfs)
            .unwrap_or(UNMEASURED),
    );
    rt.set_scope(
        6,
        snapshot
            .and_then(|value| value.right_rms_dbfs)
            .unwrap_or(UNMEASURED),
    );
    rt.set_scope(
        7,
        snapshot
            .and_then(|value| value.mid_rms_dbfs)
            .unwrap_or(UNMEASURED),
    );
    rt.set_scope(
        8,
        snapshot
            .and_then(|value| value.side_rms_dbfs)
            .unwrap_or(UNMEASURED),
    );
    rt.set_scope(9, option_f64(meter.psr_db, UNMEASURED));
    rt.set_scope(10, option_f64(meter.integrated_lufs, UNMEASURED));
    rt.set_scope(11, option_f64(meter.loudness_range_lu, UNMEASURED));
    rt.set_scope(12, option_f64(meter.plr_db, UNMEASURED));
    rt.set_scope(13, option_f64(vu.left.calibrated_db, UNMEASURED));
    rt.set_scope(14, option_f64(vu.right.calibrated_db, UNMEASURED));
    rt.set_scope(15, option_f64(vu.left.peak_hold_calibrated_db, UNMEASURED));
    rt.set_scope(16, option_f64(vu.right.peak_hold_calibrated_db, UNMEASURED));
    rt.set_scope(
        17,
        meter_core
            .and_then(|core| core.channel_metrics(0))
            .and_then(|channel| channel.sample_peak_dbfs)
            .map(|value| value as f32)
            .unwrap_or(UNMEASURED),
    );
    rt.set_scope(
        18,
        meter_core
            .and_then(|core| core.channel_metrics(1))
            .and_then(|channel| channel.sample_peak_dbfs)
            .map(|value| value as f32)
            .unwrap_or(UNMEASURED),
    );
    rt.set_scope(19, f32::from(source_id));
    rt.set_scope(
        20,
        if spectrum_visible {
            spectrum_scope.map_or(0.0, |value| value.spectrum_frame_count() as f32)
        } else {
            0.0
        },
    );
    rt.set_scope(21, FFT_SIZES[fft_index.min(FFT_SIZES.len() - 1)] as f32);
    rt.set_scope(22, module_mask);
    rt.set_scope(
        23,
        if loudness_enabled {
            capture_value(capture_a.valid, capture_a.integrated)
        } else {
            UNMEASURED
        },
    );
    rt.set_scope(
        24,
        if loudness_enabled {
            capture_value(capture_b.valid, capture_b.integrated)
        } else {
            UNMEASURED
        },
    );
    rt.set_scope(
        25,
        if loudness_enabled {
            capture_value(capture_a.valid, capture_a.short_term)
        } else {
            UNMEASURED
        },
    );
    rt.set_scope(
        26,
        if loudness_enabled {
            capture_value(capture_b.valid, capture_b.short_term)
        } else {
            UNMEASURED
        },
    );
    rt.set_scope(
        27,
        if loudness_enabled && overs_enabled {
            capture_value(capture_a.valid, capture_a.true_peak)
        } else {
            UNMEASURED
        },
    );
    rt.set_scope(
        28,
        if loudness_enabled && overs_enabled {
            capture_value(capture_b.valid, capture_b.true_peak)
        } else {
            UNMEASURED
        },
    );
    rt.set_scope(29, target_lufs);
    rt.set_scope(30, option_f64(vu.left.level_dbfs, UNMEASURED));
    rt.set_scope(31, option_f64(vu.right.level_dbfs, UNMEASURED));

    for index in 0..SCOPE_HISTORY_POINTS {
        rt.set_scope(
            SCOPE_TIMELINE_MOMENTARY + index,
            if timeline_enabled {
                timeline.value(&timeline.momentary, index)
            } else {
                UNMEASURED
            },
        );
        rt.set_scope(
            SCOPE_TIMELINE_SHORT_TERM + index,
            if timeline_enabled {
                timeline.value(&timeline.short_term, index)
            } else {
                UNMEASURED
            },
        );
        rt.set_scope(
            SCOPE_TIMELINE_INTEGRATED + index,
            if timeline_enabled {
                timeline.value(&timeline.integrated, index)
            } else {
                UNMEASURED
            },
        );
    }

    let mut histogram = [LoudnessHistogramBin::default(); SCOPE_HISTOGRAM_POINTS];
    let histogram_count = if histogram_enabled {
        meter_core.map_or(0, |core| {
            core.copy_loudness_histogram(LoudnessHistogramKind::Integrated, &mut histogram)
        })
    } else {
        0
    };
    let maximum = histogram[..histogram_count]
        .iter()
        .map(|bin| bin.count)
        .max()
        .unwrap_or(0)
        .max(1) as f32;
    for (index, bin) in histogram.iter().enumerate() {
        rt.set_scope(
            SCOPE_HISTOGRAM + index,
            (bin.count as f32 / maximum).sqrt().clamp(0.0, 1.0),
        );
    }

    if waveform_enabled {
        if let Some(scope) = display_scope {
            for index in 0..SCOPE_WAVE_POINTS {
                let source = index * scope.waveform_bucket_count() / SCOPE_WAVE_POINTS;
                let (minimum, maximum) = scope
                    .waveform_bucket(source)
                    .filter(|bucket| bucket.frames > 0)
                    .map(|bucket| {
                        (
                            (bucket.left_min + bucket.right_min) * 0.5,
                            (bucket.left_max + bucket.right_max) * 0.5,
                        )
                    })
                    .unwrap_or((0.0, 0.0));
                rt.set_scope(SCOPE_WAVE_MIN + index, minimum);
                rt.set_scope(SCOPE_WAVE_MAX + index, maximum);
            }
        } else {
            clear_scope_slots(rt, SCOPE_WAVE_MIN, SCOPE_WAVE_MAX + SCOPE_WAVE_POINTS);
        }
    } else {
        clear_scope_slots(rt, SCOPE_WAVE_MIN, SCOPE_WAVE_MAX + SCOPE_WAVE_POINTS);
    }

    if oscilloscope_enabled {
        if let Some(scope) = display_scope {
            let mut oscilloscope = [OscilloscopeSample::default(); SCOPE_OSCILLOSCOPE_POINTS];
            let oscilloscope_count =
                scope.copy_oscilloscope(&mut oscilloscope, oscilloscope_mode, oscilloscope_cycles);
            for (index, sample) in oscilloscope.iter().enumerate() {
                let (left, right) = if index < oscilloscope_count {
                    (sample.left, sample.right)
                } else {
                    (0.0, 0.0)
                };
                rt.set_scope(SCOPE_OSCILLOSCOPE + index * 2, left);
                rt.set_scope(SCOPE_OSCILLOSCOPE + index * 2 + 1, right);
            }
        } else {
            clear_scope_slots(rt, SCOPE_OSCILLOSCOPE, SCOPE_SPECTRUM);
        }
    } else {
        clear_scope_slots(rt, SCOPE_OSCILLOSCOPE, SCOPE_SPECTRUM);
    }

    if stereo_enabled {
        if let Some(scope) = display_scope {
            for index in 0..SCOPE_GONIOMETER_POINTS {
                let source = index * scope.goniometer_point_count() / SCOPE_GONIOMETER_POINTS;
                let point = scope.goniometer_point(source).unwrap_or_default();
                rt.set_scope(SCOPE_GONIOMETER + index * 2, point.side);
                rt.set_scope(SCOPE_GONIOMETER + index * 2 + 1, point.mid);
            }
        } else {
            clear_scope_slots(rt, SCOPE_GONIOMETER, NETA_SCOPE_SLOTS);
        }
    } else {
        clear_scope_slots(rt, SCOPE_GONIOMETER, NETA_SCOPE_SLOTS);
    }

    if spectrum_visible {
        if let Some(scope) = spectrum_scope {
            let spectrum = scope.spectrum();
            for index in 0..SCOPE_SPECTRUM_POINTS {
                let source = index * spectrum.len() / SCOPE_SPECTRUM_POINTS;
                let level = spectrum
                    .get(source)
                    .map(|band| band.level_dbfs)
                    .unwrap_or(-120.0);
                rt.set_scope(
                    SCOPE_SPECTRUM + index,
                    ((level + 120.0) / 120.0).clamp(0.0, 1.0),
                );
            }
        } else {
            clear_scope_slots(rt, SCOPE_SPECTRUM, SCOPE_GONIOMETER);
        }
    } else {
        clear_scope_slots(rt, SCOPE_SPECTRUM, SCOPE_GONIOMETER);
    }
}

fn clear_scope_slots(rt: &RtContext, start: usize, end: usize) {
    for index in start..end {
        rt.set_scope(index, 0.0);
    }
}

fn option_f64(value: Option<f64>, fallback: f32) -> f32 {
    value.map(|value| value as f32).unwrap_or(fallback)
}

fn finite_sample(value: Option<f32>) -> f32 {
    value.filter(|value| value.is_finite()).unwrap_or(0.0)
}

fn action_counter(value: f64) -> u16 {
    value.round().clamp(0.0, COUNTER_LIMIT) as u16
}

/// Advances a fixed-rate visual cadence without accumulating block-size
/// drift. At 48 kHz/512, for example, it alternates 512/1024-frame gaps
/// around 60 Hz instead of silently degrading to 46.9 Hz.
fn interval_due(remaining: &mut usize, interval: usize, frames: usize) -> bool {
    let interval = interval.max(1);
    if frames < *remaining {
        *remaining -= frames;
        return false;
    }
    let overshoot = frames - *remaining;
    *remaining = interval - overshoot % interval;
    true
}

fn capture_value(valid: bool, value: f32) -> f32 {
    if valid { value } else { UNMEASURED }
}

fn empty_vu_snapshot() -> VuSnapshot {
    let channel = VuChannelSnapshot {
        level_dbfs: None,
        calibrated_db: None,
        peak_hold_dbfs: None,
        peak_hold_calibrated_db: None,
    };
    VuSnapshot {
        mode: MeterVuMode::Vu,
        left: channel,
        right: channel,
        processed_frames: 0,
    }
}

fn spectrum_scale_id(scale: MeterSpectrumScale) -> u8 {
    match scale {
        MeterSpectrumScale::Log => 0,
        MeterSpectrumScale::Mel => 1,
        MeterSpectrumScale::Linear => 2,
    }
}

fn vu_mode_id(mode: MeterVuMode) -> u8 {
    match mode {
        MeterVuMode::Vu => 0,
        MeterVuMode::Rms => 1,
        MeterVuMode::Peak => 2,
    }
}

fn module_scope_work(module: usize) -> Option<ScopeWork> {
    match module {
        MODULE_SPECTRUM | MODULE_SPECTROGRAM => Some(ScopeWork::SPECTRUM),
        MODULE_WAVEFORM => Some(ScopeWork::WAVEFORM),
        MODULE_STEREO => Some(ScopeWork::CORRELATION.with(ScopeWork::GONIOMETER)),
        MODULE_OSCILLOSCOPE => Some(ScopeWork::OSCILLOSCOPE),
        MODULE_LOUDNESS | MODULE_VU | MODULE_OBJECT | MODULE_VISUALS => None,
        _ => None,
    }
}

seco_export!(Neta);

#[cfg(feature = "vst3")]
clap_wrapper::export_vst3!();

#[cfg(test)]
mod tests {
    use std::f32::consts::PI;

    use seco_core::__private::with_rt_context;
    use seco_core::Transport;

    use super::*;

    #[test]
    fn plugin_measures_without_touching_audio_or_allocating_in_process() {
        const SAMPLE_RATE: f64 = 48_000.0;
        const BLOCK: usize = 512;
        const AMPLITUDE: f32 = 0.125_892_53; // -18 dBFS peak

        let mut plugin = Neta::new();
        plugin.activate(SAMPLE_RATE, BLOCK as u32);
        let mut left = [0.0_f32; BLOCK];
        let mut right = [0.0_f32; BLOCK];
        let mut sample_index = 0_usize;

        for _ in 0..375 {
            for index in 0..BLOCK {
                let phase = 2.0 * PI * 1_000.0 * (sample_index + index) as f32 / SAMPLE_RATE as f32;
                let sample = AMPLITUDE * phase.sin();
                left[index] = sample;
                right[index] = sample;
            }
            let expected_left = left;
            let expected_right = right;
            with_rt_context(Transport::default(), &[], &[], |rt| {
                let mut channels: [&mut [f32]; 2] = [&mut left, &mut right];
                let mut audio = AudioBuffer::new(&mut channels);
                plugin.process(&mut audio, rt);
            });
            assert_eq!(left, expected_left);
            assert_eq!(right, expected_right);
            sample_index += BLOCK;
        }

        let snapshot = plugin.meter.as_mut().unwrap().snapshot();
        let integrated = snapshot.integrated_lufs.unwrap();
        assert!(
            (integrated + 18.0).abs() <= 0.1,
            "expected -18 LUFS, got {integrated}"
        );
    }

    #[test]
    fn scope_vocabulary_is_exactly_the_declared_neta_picture() {
        assert_eq!(NETA_SCOPE_SLOTS, 896);
        assert_eq!(SCOPE_TIMELINE, 32);
        assert_eq!(SCOPE_HISTOGRAM, 224);
        assert_eq!(SCOPE_WAVE_MIN, 288);
        assert_eq!(SCOPE_WAVE_MAX, 352);
        assert_eq!(SCOPE_OSCILLOSCOPE, 416);
        assert_eq!(SCOPE_SPECTRUM, 672);
        assert_eq!(SCOPE_GONIOMETER, 768);
        assert_eq!(
            SCOPE_GONIOMETER + SCOPE_GONIOMETER_POINTS * 2,
            NETA_SCOPE_SLOTS
        );
        assert_eq!(<Neta as Plugin>::SCOPE_SLOTS, NETA_SCOPE_SLOTS);
        assert_eq!(<Neta as Plugin>::EDITOR_REFRESH_HZ, 60);
    }

    #[test]
    fn action_counters_are_bounded_whole_numbers() {
        assert_eq!(action_counter(-4.0), 0);
        assert_eq!(action_counter(4.49), 4);
        assert_eq!(action_counter(4.5), 5);
        assert_eq!(action_counter(COUNTER_LIMIT + 10.0), COUNTER_LIMIT as u16);
    }

    #[test]
    fn cadence_carries_block_overshoot() {
        let mut remaining = 1;
        let mut published = 0;
        for _ in 0..150 {
            if interval_due(&mut remaining, 800, 512) {
                published += 1;
            }
        }
        // 150 * 512 / 800 = 96: first immediate picture adds one only when
        // the loop begins, so tolerance keeps this a cadence test not a
        // policy test about initial display.
        assert!((95..=97).contains(&published), "published {published}");
    }

    #[test]
    fn timeline_keeps_chronological_newest_values() {
        let mut timeline = LoudnessTimeline::default();
        for value in 0..(SCOPE_HISTORY_POINTS + 2) {
            timeline.push(RealtimeMeterSnapshot {
                momentary_lufs: Some(value as f64),
                short_term_lufs: None,
                max_true_peak_dbfs: None,
                psr_db: None,
                integrated_lufs: None,
                loudness_range_lu: None,
                plr_db: None,
                processed_frames: value as u64,
            });
        }
        assert_eq!(timeline.value(&timeline.momentary, 0), 2.0);
        assert_eq!(
            timeline.value(&timeline.momentary, SCOPE_HISTORY_POINTS - 1),
            (SCOPE_HISTORY_POINTS + 1) as f32
        );
        assert_eq!(
            timeline.value(&timeline.momentary, SCOPE_HISTORY_POINTS),
            UNMEASURED
        );
    }

    #[test]
    fn object_does_not_keep_fft_work_alive() {
        let mut plugin = Neta::new();
        plugin.runtime = settings::RuntimeSettings::parse(b"v=2;m=000001000");
        assert_eq!(plugin.scope_work(), ScopeWork::NONE);

        plugin.runtime = settings::RuntimeSettings::parse(b"v=2;m=010000000;hold=0");
        assert_eq!(plugin.scope_work(), ScopeWork::SPECTRUM);
        plugin.runtime = settings::RuntimeSettings::parse(b"v=2;m=010000000;hold=1");
        assert_eq!(plugin.scope_work(), ScopeWork::NONE);
    }

    #[test]
    fn fft_switch_keeps_non_fft_display_history() {
        let mut plugin = Neta::new();
        plugin.activate(48_000.0, 512);
        let mut left = [0.25_f32; 512];
        let mut right = [-0.25_f32; 512];
        with_rt_context(Transport::default(), &[], &[], |rt| {
            plugin.apply_state(b"v=2;fft=2048", rt);
            let mut channels: [&mut [f32]; 2] = [&mut left, &mut right];
            let mut audio = AudioBuffer::new(&mut channels);
            plugin.process(&mut audio, rt);
        });

        let display = plugin.display_scope.as_ref().expect("display scope");
        let frames = display.snapshot().processed_frames;
        let oscilloscope_frames = display.oscilloscope_frame_count();
        assert!(frames > 0);
        assert!(oscilloscope_frames > 0);

        plugin.runtime = settings::RuntimeSettings::parse(b"v=2;fft=4096");
        plugin.runtime_dirty = true;
        plugin.apply_runtime();

        let display = plugin.display_scope.as_ref().expect("display scope");
        assert_eq!(display.snapshot().processed_frames, frames);
        assert_eq!(display.oscilloscope_frame_count(), oscilloscope_frames);
        assert_eq!(
            plugin
                .meter
                .as_ref()
                .unwrap()
                .realtime_snapshot()
                .processed_frames,
            frames
        );
    }

    #[test]
    fn persisted_action_counters_seed_without_replaying() {
        let mut plugin = Neta::new();
        plugin.activate(48_000.0, 32);
        let mut left = [0.0_f32; 32];
        let mut right = [0.0_f32; 32];
        let params = [41.0, 17.0, 9.0];
        with_rt_context(Transport::default(), &params, &[], |rt| {
            let mut channels: [&mut [f32]; 2] = [&mut left, &mut right];
            let mut audio = AudioBuffer::new(&mut channels);
            plugin.process(&mut audio, rt);
        });
        assert!(plugin.actions_primed);
        assert_eq!(plugin.reset_counter, 41);
        assert_eq!(plugin.capture_a_counter, 17);
        assert_eq!(plugin.capture_b_counter, 9);
        assert!(!plugin.capture_a.valid);
        assert!(!plugin.capture_b.valid);
    }

    #[test]
    fn disabled_loudness_skips_meter_and_capture_work() {
        let mut plugin = Neta::new();
        plugin.activate(48_000.0, 32);
        let mut left = [0.25_f32; 32];
        let mut right = [0.25_f32; 32];
        with_rt_context(Transport::default(), &[], &[], |rt| {
            // VU stays live, so this proves the loudness gate rather than an
            // all-modules-disabled early return.
            plugin.apply_state(b"v=2;m=000000010", rt);
            let mut channels: [&mut [f32]; 2] = [&mut left, &mut right];
            let mut audio = AudioBuffer::new(&mut channels);
            plugin.process(&mut audio, rt);
        });
        assert_eq!(
            plugin
                .meter
                .as_ref()
                .unwrap()
                .realtime_snapshot()
                .processed_frames,
            0
        );
        assert_eq!(plugin.vu.as_ref().unwrap().snapshot().processed_frames, 32);

        with_rt_context(Transport::default(), &[0.0, 1.0, 0.0], &[], |rt| {
            let mut channels: [&mut [f32]; 2] = [&mut left, &mut right];
            let mut audio = AudioBuffer::new(&mut channels);
            plugin.process(&mut audio, rt);
        });
        assert!(!plugin.capture_a.valid);
    }

    #[test]
    fn route_and_trim_are_analysis_only() {
        let mut plugin = Neta::new();
        plugin.activate(48_000.0, 32);
        let mut left = [0.0_f32; 32];
        let mut right = [0.0_f32; 32];
        for (index, sample) in left.iter_mut().enumerate() {
            *sample = index as f32 / 32.0 - 0.5;
            right[index] = -*sample;
        }
        let expected_left = left;
        let expected_right = right;
        with_rt_context(Transport::default(), &[], &[], |rt| {
            plugin.apply_state(b"v=2;route=side;trim=24", rt);
            let mut channels: [&mut [f32]; 2] = [&mut left, &mut right];
            let mut audio = AudioBuffer::new(&mut channels);
            plugin.process(&mut audio, rt);
        });
        assert_eq!(left, expected_left);
        assert_eq!(right, expected_right);
    }

    #[test]
    fn setting_changes_during_audio_remain_byte_identical() {
        let mut plugin = Neta::new();
        plugin.activate(48_000.0, 64);
        let source_left =
            std::array::from_fn::<_, 64, _>(|index| (index as f32 * 0.19).sin() * 0.75);
        let source_right =
            std::array::from_fn::<_, 64, _>(|index| (index as f32 * 0.27).cos() * 0.5);
        let states: [&[u8]; 4] = [
            b"v=2;route=stereo;trim=-24;fft=1024;scale=log;smooth=0;hold=0",
            b"v=2;route=mid;trim=12;fft=16384;scale=mel;smooth=100;hold=0",
            b"v=2;route=side;trim=24;fft=4096;scale=linear;smooth=40;hold=1",
            b"v=2;m=000000000;route=right;trim=-12;fft=8192",
        ];

        for &state in &states {
            let mut left = source_left;
            let mut right = source_right;
            with_rt_context(Transport::default(), &[], &[], |rt| {
                plugin.apply_state(state, rt);
                let mut channels: [&mut [f32]; 2] = [&mut left, &mut right];
                let mut audio = AudioBuffer::new(&mut channels);
                plugin.process(&mut audio, rt);
            });
            assert_eq!(left, source_left, "left changed for {state:?}");
            assert_eq!(right, source_right, "right changed for {state:?}");
        }
    }
}
