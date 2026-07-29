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

use neta_meter::scope::ScopeAnalyzer;
use neta_meter::{LoudnessMeter, RealtimeMeterSnapshot};
use seco_clap::seco_export;
use seco_core::{AudioBuffer, EditorPage, Plugin, RtContext};

/// Sentinel for a gated value the programme has not produced yet.
///
/// Far below any real loudness, and finite so `set_scope` accepts it — a
/// NaN would be dropped and leave the slot reading as a measurement.
pub(crate) const UNMEASURED: f32 = -200.0;

const SCOPE_METRICS: usize = 13;
const SCOPE_WAVE_MIN: usize = 16;
const SCOPE_WAVE_MAX: usize = 80;
const SCOPE_WAVE_POINTS: usize = 64;
const SCOPE_SPECTRUM: usize = 144;
const SCOPE_SPECTRUM_POINTS: usize = 80;
const SCOPE_GONIOMETER: usize = 224;
const SCOPE_GONIOMETER_POINTS: usize = 16;

const _: () = {
    assert!(SCOPE_METRICS <= SCOPE_WAVE_MIN);
    assert!(SCOPE_WAVE_MIN + SCOPE_WAVE_POINTS == SCOPE_WAVE_MAX);
    assert!(SCOPE_WAVE_MAX <= SCOPE_SPECTRUM);
    assert!(SCOPE_SPECTRUM + SCOPE_SPECTRUM_POINTS == SCOPE_GONIOMETER);
    assert!(SCOPE_GONIOMETER + SCOPE_GONIOMETER_POINTS * 2 == seco_clap::SCOPE_BUCKETS);
};

struct Neta {
    /// Constructed in `activate`, where allocation is allowed.
    meter: Option<LoudnessMeter>,
    /// Fixed-storage scope; it only sees audio and publishes atoms through
    /// `RtContext`, never the editor itself.
    scope: Option<ScopeAnalyzer>,
}

impl Plugin for Neta {
    const ID: &'static str = "dev.seco.neta";
    const NAME: &'static str = "Neta";
    const VENDOR: &'static str = "SECO";
    const VERSION: &'static str = "0.1.0";
    const DESCRIPTION: &'static str = "Loudness meter";
    const EDITOR: Option<EditorPage> = Some(editor::PAGE);

    // A meter has nothing to automate yet. `PARAMS` defaults to empty, and
    // the adapter reports zero parameters rather than inventing one.

    fn new() -> Self {
        Neta {
            meter: None,
            scope: None,
        }
    }

    fn activate(&mut self, sample_rate: f64, max_frames: u32) {
        self.meter = LoudnessMeter::stereo(sample_rate, max_frames as usize).ok();
        self.scope = ScopeAnalyzer::stereo(sample_rate, max_frames as usize).ok();
    }

    fn deactivate(&mut self) {
        self.meter = None;
        self.scope = None;
    }

    fn reset(&mut self) {
        if let Some(meter) = &mut self.meter {
            meter.reset();
        }
        if let Some(scope) = &mut self.scope {
            scope.reset();
        }
    }

    fn process(&mut self, audio: &mut AudioBuffer, rt: &RtContext) {
        // A meter is an observer: the audio leaves exactly as it arrived.
        // `process` receives the buffer mutably because the framework has
        // one shape for every plugin, not because this one writes to it.
        let meter_snapshot = if let Some(meter) = &mut self.meter {
            let result = meter.process_block(audio.channels());
            debug_assert!(
                result.is_ok(),
                "framework broke activate/audio-buffer contract: {result:?}"
            );
            Some(meter.realtime_snapshot())
        } else {
            None
        };

        if let Some(scope) = &mut self.scope {
            let mut channels = audio.channels();
            let result = match (channels.next(), channels.next()) {
                (Some(left), Some(right)) => scope.process_stereo(left, right),
                (Some(mono), None) => scope.process_mono(mono),
                (None, _) => Ok(()),
            };
            debug_assert!(
                result.is_ok(),
                "framework broke activate/audio-buffer contract: {result:?}"
            );
            publish_picture(rt, meter_snapshot, scope);
        }
    }

    fn editor_frame(scope: &[f32]) -> Option<String> {
        editor::frame(scope)
    }

    /// Hands the page back the layout the session was saved with. The block
    /// is the editor's own; the audio thread never reads it, which is why
    /// `apply_state` stays the default no-op — hiding the waveform does not
    /// change what the meter measures.
    fn editor_script(_params: &[f64], state: &[u8]) -> Option<String> {
        let block = core::str::from_utf8(state).unwrap_or_default();
        Some(settings::Settings::parse(block).to_script())
    }

    /// The page's one request: load a model from disk. Main thread, so the
    /// I/O is allowed here and nowhere else in this plugin.
    fn editor_message(text: &str) -> Option<String> {
        text.strip_prefix("model ").map(model::load)
    }
}

/// Copies a deliberately bounded picture from preallocated analysis state to
/// the adapter's lock-free visualization slots. One dropped/mixed frame is
/// preferable to one delayed audio callback.
fn publish_picture(rt: &RtContext, meter: Option<RealtimeMeterSnapshot>, scope: &ScopeAnalyzer) {
    let scope_snapshot = scope.snapshot();
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
    let value =
        |option: Option<f64>, fallback: f32| option.map(|number| number as f32).unwrap_or(fallback);
    rt.set_scope(0, value(meter.momentary_lufs, -60.0));
    rt.set_scope(1, value(meter.short_term_lufs, -60.0));
    rt.set_scope(2, value(meter.max_true_peak_dbfs, -60.0));
    rt.set_scope(3, scope_snapshot.correlation.unwrap_or(0.0));
    rt.set_scope(4, scope_snapshot.stereo_width.unwrap_or(0.0));
    rt.set_scope(5, scope_snapshot.left_rms_dbfs.unwrap_or(-60.0));
    rt.set_scope(6, scope_snapshot.right_rms_dbfs.unwrap_or(-60.0));
    rt.set_scope(7, scope_snapshot.mid_rms_dbfs.unwrap_or(-60.0));
    rt.set_scope(8, scope_snapshot.side_rms_dbfs.unwrap_or(-60.0));
    rt.set_scope(9, value(meter.psr_db, 0.0));
    // Integrated, LRA and PLR now come from the meter's binned distribution,
    // so the plugin shows them live instead of pointing at the native app.
    //
    // The sentinel is a number rather than NaN on purpose: `set_scope`
    // drops non-finite values, so a NaN would leave the slot holding its
    // previous contents — 0.0 on a fresh plugin, which the page would draw
    // as a real reading of 0 LUFS.
    rt.set_scope(10, value(meter.integrated_lufs, UNMEASURED));
    rt.set_scope(11, value(meter.loudness_range_lu, UNMEASURED));
    rt.set_scope(12, value(meter.plr_db, UNMEASURED));

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
    for index in 0..SCOPE_GONIOMETER_POINTS {
        let source = index * scope.goniometer_point_count() / SCOPE_GONIOMETER_POINTS;
        let point = scope.goniometer_point(source).unwrap_or_default();
        rt.set_scope(SCOPE_GONIOMETER + index * 2, point.side);
        rt.set_scope(SCOPE_GONIOMETER + index * 2 + 1, point.mid);
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
}
