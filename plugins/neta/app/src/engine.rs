//! Standalone Neta analysis path.
//!
//! The demo source intentionally enters through the same SPSC bridge used by
//! an eventual system-audio capture thread. Replacing `fill_demo_block` with
//! a capture callback does not change loudness, scope, Dalia, or rendering.

use dalia_core::DaliaEngine;
use neta_bridge::{
    AudioBlockConsumer, AudioBlockMeta, AudioBlockProducer, AudioBlockRing, PushError, RingConfig,
};
use neta_meter::LoudnessMeter;
use neta_meter::scope::{GoniometerPoint, SPECTRUM_FLOOR_DBFS, ScopeAnalyzer, WaveformBucket};
use neta_visual::{
    MeterReadings, MinMax, Palette, StereoPoint, VisualComposer, VisualFrame, VisualScene,
};

pub const SAMPLE_RATE: u32 = 48_000;
pub const BLOCK_FRAMES: usize = 800;
const WAVEFORM_POINTS: usize = 256;
const GONIOMETER_POINTS: usize = 1_024;
const SPECTRUM_POINTS: usize = 96;
const SPECTROGRAM_ROWS: usize = 64;
const SPECTROGRAM_BANDS: usize = 48;
/// Dalia's browser implementation consumes `AnalyserNode`'s 2048-point FFT.
/// It receives the 1024 non-Nyquist bins as `getByteFrequencyData` output.
const DALIA_FFT_SIZE: usize = 2_048;
const DALIA_BIN_COUNT: usize = DALIA_FFT_SIZE / 2;
// A meter window is not a fullscreen particle show. Keep Dalia's full
// 12,000-point source domain, but emit an even 1,024-point sample: enough
// density at Neta's native window size without burning a core every frame.
const DALIA_RENDER_VERTICES: usize = 1_024;
const WEB_AUDIO_MIN_DBFS: f32 = -100.0;
const WEB_AUDIO_MAX_DBFS: f32 = -30.0;
const MAX_DEMO_BLOCKS_PER_TICK: usize = 8;

/// UI-owned source/analysis pipeline. It never claims demo data is system
/// audio; callers choose it explicitly while capture backends are unavailable.
pub struct StandaloneEngine {
    meter: LoudnessMeter,
    scope: ScopeAnalyzer,
    composer: VisualComposer,
    dalia: DaliaEngine,
    producer: Option<AudioBlockProducer>,
    consumer: AudioBlockConsumer,
    source_left: [f32; BLOCK_FRAMES],
    source_right: [f32; BLOCK_FRAMES],
    received_left: [f32; BLOCK_FRAMES],
    received_right: [f32; BLOCK_FRAMES],
    waveform_buckets: Vec<WaveformBucket>,
    goniometer_points: Vec<GoniometerPoint>,
    waveform: Vec<MinMax>,
    goniometer: Vec<StereoPoint>,
    spectrum: Vec<f32>,
    spectrogram: Vec<f32>,
    spectrogram_rows: usize,
    last_spectrogram_frame: u64,
    dalia_bins: [u8; DALIA_BIN_COUNT],
    reactivity: f32,
    integrated_lufs: Option<f64>,
    loudness_range_lu: Option<f64>,
    next_slow_meter_refresh_frame: u64,
    sample_rate: u32,
    stream_frame: u64,
    demo_pending_seconds: f32,
}

impl StandaloneEngine {
    pub fn new(sample_rate: u32) -> Result<Self, String> {
        if sample_rate < 8_000 {
            return Err(format!(
                "input sample rate {sample_rate} Hz is too low for Neta"
            ));
        }
        let meter = LoudnessMeter::stereo(f64::from(sample_rate), BLOCK_FRAMES)
            .map_err(|error| error.to_string())?;
        let scope = ScopeAnalyzer::stereo(f64::from(sample_rate), BLOCK_FRAMES)
            .map_err(|error| error.to_string())?;
        if scope.fft_size() != DALIA_FFT_SIZE {
            return Err(format!(
                "Neta scope FFT is {}; Dalia adapter requires {DALIA_FFT_SIZE}",
                scope.fft_size()
            ));
        }
        let (producer, consumer) = AudioBlockRing::new(RingConfig::new(8, 2, BLOCK_FRAMES))
            .map_err(|error| error.to_string())?
            .into_endpoints();
        Ok(Self {
            meter,
            scope,
            composer: VisualComposer::with_spectrogram(
                Palette::NETA,
                WAVEFORM_POINTS,
                SPECTRUM_POINTS,
                GONIOMETER_POINTS,
                SPECTROGRAM_ROWS,
                SPECTROGRAM_BANDS,
            ),
            dalia: DaliaEngine::with_vertex_count(DALIA_RENDER_VERTICES),
            producer: Some(producer),
            consumer,
            source_left: [0.0; BLOCK_FRAMES],
            source_right: [0.0; BLOCK_FRAMES],
            received_left: [0.0; BLOCK_FRAMES],
            received_right: [0.0; BLOCK_FRAMES],
            waveform_buckets: vec![empty_waveform_bucket(); WAVEFORM_POINTS],
            goniometer_points: vec![GoniometerPoint::default(); GONIOMETER_POINTS],
            waveform: vec![MinMax::default(); WAVEFORM_POINTS],
            goniometer: vec![StereoPoint::default(); GONIOMETER_POINTS],
            spectrum: vec![0.0; SPECTRUM_POINTS],
            spectrogram: vec![SPECTRUM_FLOOR_DBFS; SPECTROGRAM_ROWS * SPECTROGRAM_BANDS],
            spectrogram_rows: 0,
            last_spectrogram_frame: 0,
            dalia_bins: [0; DALIA_BIN_COUNT],
            reactivity: 0.0,
            integrated_lufs: None,
            loudness_range_lu: None,
            next_slow_meter_refresh_frame: 0,
            sample_rate,
            stream_frame: 0,
            // Render an immediate first frame, then keep demo audio tied to
            // elapsed audio time rather than window redraw frequency.
            demo_pending_seconds: BLOCK_FRAMES as f32 / sample_rate as f32,
        })
    }

    /// Advances a deterministic stereo programme, transfers it through the
    /// bridge, and updates all analysis. It is a demo source, not a capture.
    pub fn step_demo(&mut self, delta_seconds: f32) {
        let block_seconds = BLOCK_FRAMES as f32 / self.sample_rate as f32;
        let maximum_pending = block_seconds * MAX_DEMO_BLOCKS_PER_TICK as f32;
        let elapsed = if delta_seconds.is_finite() {
            delta_seconds.clamp(0.0, 0.25)
        } else {
            0.0
        };
        self.demo_pending_seconds = (self.demo_pending_seconds + elapsed).min(maximum_pending);
        let mut blocks = 0;
        while self.demo_pending_seconds >= block_seconds && blocks < MAX_DEMO_BLOCKS_PER_TICK {
            self.demo_pending_seconds -= block_seconds;
            self.step_demo_block(block_seconds);
            blocks += 1;
        }
    }

    fn step_demo_block(&mut self, block_seconds: f32) {
        self.fill_demo_block();
        let source: [&[f32]; 2] = [&self.source_left, &self.source_right];
        let meta = AudioBlockMeta::new(self.stream_frame, self.sample_rate);
        let Some(producer) = &mut self.producer else {
            return;
        };
        match producer.try_push(meta, &source) {
            Ok(()) => {}
            Err(PushError::Full) => {
                // Visual monitoring favors newest data over latency. The
                // bridge still makes this policy explicit rather than hiding
                // a blocking operation in the producer.
                let _ = self.consumer.try_discard();
                let _ = producer.try_push(meta, &source);
            }
            Err(error) => {
                debug_assert!(false, "demo source broke bridge contract: {error}");
                return;
            }
        }
        self.stream_frame = self.stream_frame.saturating_add(BLOCK_FRAMES as u64);
        self.consume_available(block_seconds);
    }

    /// Moves every ready bridge block through analysis. This is called by the
    /// UI thread; a capture callback owns the producer on a separate thread.
    pub fn consume_available(&mut self, delta_seconds: f32) {
        let mut consumed = false;
        loop {
            let result = {
                let mut destination: [&mut [f32]; 2] =
                    [&mut self.received_left, &mut self.received_right];
                self.consumer.try_pop_into(&mut destination)
            };
            let Ok(Some(info)) = result else {
                break;
            };
            let left = &self.received_left[..info.frames];
            let right = &self.received_right[..info.frames];
            let _ = self.meter.process_block([left, right].into_iter());
            let _ = self.scope.process_stereo(left, right);
            consumed = true;
        }
        if consumed {
            self.update_visual_data(delta_seconds);
        }
    }

    /// Transfers producer ownership to a realtime capture callback. Once this
    /// is called, `step_demo` becomes a no-op and only `consume_available`
    /// advances visuals.
    pub fn take_capture_producer(&mut self) -> Option<AudioBlockProducer> {
        self.producer.take()
    }

    /// Returns one coherent frame for the native renderer. The scene and
    /// Dalia slices borrow separate engine fields, so no copy is needed.
    pub fn render_parts(&mut self) -> (&VisualScene, &[f32], &[f32], f32) {
        let meter = self.meter.realtime_snapshot();
        let scope = self.scope.snapshot();
        let scene = self.composer.compose(VisualFrame {
            meter: MeterReadings {
                momentary_lufs: meter.momentary_lufs.map(as_f32),
                short_term_lufs: meter.short_term_lufs.map(as_f32),
                integrated_lufs: self.integrated_lufs.map(as_f32),
                lra_lu: self.loudness_range_lu.map(as_f32),
                true_peak_dbfs: meter.max_true_peak_dbfs.map(as_f32).unwrap_or(-60.0),
                correlation: scope.correlation.unwrap_or(0.0),
                stereo_width: scope.stereo_width.unwrap_or(0.0),
            },
            waveform: &self.waveform,
            spectrum: &self.spectrum,
            spectrogram: &self.spectrogram[..self.spectrogram_rows * SPECTROGRAM_BANDS],
            spectrogram_rows: self.spectrogram_rows,
            spectrogram_bands: SPECTROGRAM_BANDS,
            goniometer: &self.goniometer,
        });
        (
            scene,
            self.dalia.geometry(),
            self.dalia.colors(),
            self.reactivity,
        )
    }

    pub fn next_dalia_preset(&mut self) {
        self.dalia.next_preset();
    }

    fn fill_demo_block(&mut self) {
        // Three partials with slowly changing side energy: enough movement to
        // exercise loudness, spectrum, vectorscope, and Dalia without hiding
        // what each visual is responding to.
        let rate = self.sample_rate as f32;
        for index in 0..BLOCK_FRAMES {
            let time = (self.stream_frame as usize + index) as f32 / rate;
            let carrier = (std::f32::consts::TAU * 110.0 * time).sin() * 0.18;
            let shimmer = (std::f32::consts::TAU * 1_920.0 * time).sin() * 0.055;
            let side = (std::f32::consts::TAU * 0.071 * time).sin() * 0.11;
            self.source_left[index] = carrier + shimmer + side;
            self.source_right[index] = carrier + shimmer - side * 0.72;
        }
    }

    fn update_visual_data(&mut self, delta_seconds: f32) {
        self.refresh_slow_meter();
        let bucket_count = self.scope.copy_waveform_buckets(&mut self.waveform_buckets);
        self.waveform_buckets[bucket_count..].fill(empty_waveform_bucket());
        for (destination, source) in self.waveform.iter_mut().zip(&self.waveform_buckets) {
            *destination = MinMax {
                min: ((source.left_min + source.right_min) * 0.5).clamp(-1.0, 1.0),
                max: ((source.left_max + source.right_max) * 0.5).clamp(-1.0, 1.0),
            };
        }
        self.waveform[bucket_count..].fill(MinMax::default());

        let point_count = self
            .scope
            .copy_goniometer_points(&mut self.goniometer_points);
        self.goniometer_points[point_count..].fill(GoniometerPoint::default());
        for (destination, source) in self.goniometer.iter_mut().zip(&self.goniometer_points) {
            *destination = StereoPoint {
                left: source.mid + source.side,
                right: source.mid - source.side,
            };
        }
        self.goniometer[point_count..].fill(StereoPoint::default());

        let mut energy = 0.0_f32;
        for (linear, band) in self.spectrum.iter_mut().zip(self.scope.spectrum()) {
            *linear = 10.0_f32
                .powf(band.level_dbfs.max(-120.0) / 20.0)
                .clamp(0.0, 1.0);
            energy += *linear;
        }
        self.reactivity = (energy / self.spectrum.len().max(1) as f32).clamp(0.0, 1.0);
        self.update_spectrogram();
        let raw_fft = self.scope.linear_spectrum();
        debug_assert_eq!(raw_fft.len(), DALIA_BIN_COUNT + 1);
        for (byte, amplitude) in self.dalia_bins.iter_mut().zip(raw_fft.iter()) {
            *byte = web_audio_frequency_byte(*amplitude);
        }
        self.dalia.process_audio(
            &self.dalia_bins,
            self.sample_rate as f32 / DALIA_FFT_SIZE as f32,
            delta_seconds.clamp(0.001, 0.1),
        );
    }

    /// Copies the scope's chronological waterfall only when FFT analysis
    /// advanced. Both source and destination are fixed buffers; the display
    /// takes evenly spread samples rather than allocating a resized image.
    fn update_spectrogram(&mut self) {
        let frame = self.scope.spectrum_frame_count();
        if frame == self.last_spectrogram_frame {
            return;
        }
        self.last_spectrogram_frame = frame;
        self.spectrogram.fill(SPECTRUM_FLOOR_DBFS);
        let source_rows = self.scope.spectrogram_row_count();
        let source_bands = self.scope.spectrogram_band_count();
        if source_rows == 0 || source_bands == 0 {
            self.spectrogram_rows = 0;
            return;
        }
        let displayed_rows = source_rows.min(SPECTROGRAM_ROWS);
        for destination_row in 0..displayed_rows {
            let source_row = evenly_spaced_index(destination_row, displayed_rows, source_rows);
            let Some(row) = self.scope.spectrogram_row(source_row) else {
                continue;
            };
            let destination = &mut self.spectrogram
                [destination_row * SPECTROGRAM_BANDS..(destination_row + 1) * SPECTROGRAM_BANDS];
            for (destination_band, value) in destination.iter_mut().enumerate() {
                let source_band =
                    evenly_spaced_index(destination_band, SPECTROGRAM_BANDS, source_bands);
                *value = row[source_band];
            }
        }
        self.spectrogram_rows = displayed_rows;
    }

    /// Integrated loudness and LRA scan/sort programme history. Cache those
    /// values at one-second audio intervals; use `realtime_snapshot` for each
    /// visual frame so redraw cadence cannot turn an O(n log n) UI query into
    /// a CPU spinner.
    fn refresh_slow_meter(&mut self) {
        let realtime = self.meter.realtime_snapshot();
        if realtime.processed_frames < self.next_slow_meter_refresh_frame {
            return;
        }
        let snapshot = self.meter.snapshot();
        self.integrated_lufs = snapshot.integrated_lufs;
        self.loudness_range_lu = snapshot.loudness_range_lu;
        self.next_slow_meter_refresh_frame = realtime
            .processed_frames
            .saturating_add(u64::from(self.sample_rate));
    }
}

fn as_f32(value: f64) -> f32 {
    if value.is_finite() {
        value as f32
    } else {
        -60.0
    }
}

/// Converts a linear FFT magnitude into `AnalyserNode`'s default byte range.
/// Dalia was tuned against `getByteFrequencyData`, whose default analysis
/// range is `-100..-30 dB`; preserving that contract keeps native reactivity
/// aligned with its browser visual language.
fn web_audio_frequency_byte(amplitude: f32) -> u8 {
    if !amplitude.is_finite() {
        return 0;
    }
    let dbfs = 20.0 * amplitude.clamp(0.000_01, 1.0).log10();
    let unit =
        ((dbfs - WEB_AUDIO_MIN_DBFS) / (WEB_AUDIO_MAX_DBFS - WEB_AUDIO_MIN_DBFS)).clamp(0.0, 1.0);
    (unit * 255.0).round() as u8
}

fn evenly_spaced_index(index: usize, output_len: usize, input_len: usize) -> usize {
    if input_len <= 1 || output_len <= 1 {
        0
    } else {
        index * (input_len - 1) / (output_len - 1)
    }
}

const fn empty_waveform_bucket() -> WaveformBucket {
    WaveformBucket {
        left_min: 0.0,
        left_max: 0.0,
        right_min: 0.0,
        right_max: 0.0,
        frames: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_path_builds_real_scene_and_dalia_frame() {
        let mut engine = StandaloneEngine::new(SAMPLE_RATE).unwrap();
        for _ in 0..8 {
            engine.step_demo(1.0 / 60.0);
        }
        let (scene, geometry, colors, _) = engine.render_parts();
        assert!(!scene.quads.is_empty());
        assert!(!scene.lines.is_empty());
        assert_eq!(geometry.len(), colors.len());
        assert_eq!(geometry.len(), DALIA_RENDER_VERTICES * 3);
        assert!(engine.spectrogram_rows > 0);
    }

    #[test]
    fn dalia_receives_linear_web_audio_fft_bins() {
        let mut engine = StandaloneEngine::new(SAMPLE_RATE).unwrap();
        let bin = 213usize;
        let frequency = bin as f32 * SAMPLE_RATE as f32 / DALIA_FFT_SIZE as f32;
        for block in 0..3 {
            for sample in 0..BLOCK_FRAMES {
                let frame = block * BLOCK_FRAMES + sample;
                let phase = std::f32::consts::TAU * frequency * frame as f32 / SAMPLE_RATE as f32;
                let value = 0.5 * phase.sin();
                engine.source_left[sample] = value;
                engine.source_right[sample] = value;
            }
            let source: [&[f32]; 2] = [&engine.source_left, &engine.source_right];
            engine
                .producer
                .as_mut()
                .unwrap()
                .try_push(
                    AudioBlockMeta::new((block * BLOCK_FRAMES) as u64, SAMPLE_RATE),
                    &source,
                )
                .unwrap();
            engine.consume_available(BLOCK_FRAMES as f32 / SAMPLE_RATE as f32);
        }

        assert_eq!(engine.dalia_bins.len(), DALIA_BIN_COUNT);
        assert!(engine.scope.linear_spectrum()[bin] > 0.45);
        assert!(engine.dalia_bins[bin] > 0);
        // 4.99 kHz is Dalia's presence band; this would remain silent when
        // Neta's former 96 log display bars were presented as linear bins.
        assert!(engine.dalia.get_presence() > 0.0);
    }

    #[test]
    fn web_audio_adapter_uses_browser_default_decibel_range() {
        assert_eq!(web_audio_frequency_byte(f32::NAN), 0);
        assert_eq!(web_audio_frequency_byte(0.000_01), 0);
        assert_eq!(web_audio_frequency_byte(1.0), 255);
        assert!(web_audio_frequency_byte(0.01) > web_audio_frequency_byte(0.000_1));
    }
}
