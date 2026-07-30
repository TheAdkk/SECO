//! Realtime-safe stereo scope analysis.
//!
//! [`ScopeAnalyzer`] accepts planar stereo blocks and maintains fixed-size
//! representations for a waveform, goniometer, correlation/mid-side metrics,
//! display spectrum, and spectrogram. Construction allocates every buffer;
//! [`ScopeAnalyzer::process_stereo`] only updates existing storage.

use std::error::Error;
use std::f64::consts::PI;
use std::fmt;

/// Lowest level emitted for silent spectrum and spectrogram bands.
pub const SPECTRUM_FLOOR_DBFS: f32 = -120.0;

const SPECTRUM_FLOOR_AMPLITUDE: f32 = 0.000_001;

/// Frequency distribution used for display spectrum bands.
///
/// The FFT itself always remains linear-frequency. This only changes how its
/// fixed bins are grouped for [`ScopeAnalyzer::spectrum`], so switching scale
/// cannot allocate or alter incoming audio.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SpectrumScale {
    /// Equal-width bands in frequency.
    Linear,
    /// Equal-width bands in the perceptual Mel domain.
    Mel,
    /// Equal-width bands in logarithmic frequency.
    #[default]
    Log,
}

impl SpectrumScale {
    const ALL: [Self; 3] = [Self::Log, Self::Mel, Self::Linear];

    const fn storage_index(self) -> usize {
        match self {
            Self::Log => 0,
            Self::Mel => 1,
            Self::Linear => 2,
        }
    }
}

/// Trigger policy used by [`ScopeAnalyzer::copy_oscilloscope`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OscilloscopeMode {
    /// Align a stable rising zero crossing when one is available.
    #[default]
    Pitch,
    /// Return newest chronological audio without trigger alignment.
    Free,
}

/// Number of cycles presented by pitch-triggered oscilloscope output.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OscilloscopeCycles {
    /// Present one estimated cycle.
    Single,
    /// Present up to three estimated cycles.
    #[default]
    Multi,
}

/// One original stereo sample retained for oscilloscope drawing.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct OscilloscopeSample {
    /// Left channel sample.
    pub left: f32,
    /// Right channel sample.
    pub right: f32,
}

/// Bounded scope work selected for one input block.
///
/// Disabling a flag preserves its retained result until callers reset it or
/// process that flag again. This lets a shell avoid FFT work while Spectrum
/// and Spectrogram are hidden without changing audio or allocating.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScopeWork(u8);

impl ScopeWork {
    /// Do rolling correlation and M/S scalar measurements.
    pub const CORRELATION: Self = Self(1 << 0);
    /// Fill min/max waveform envelope buckets.
    pub const WAVEFORM: Self = Self(1 << 1);
    /// Fill vectorscope points.
    pub const GONIOMETER: Self = Self(1 << 2);
    /// Retain original samples for oscilloscope drawing.
    pub const OSCILLOSCOPE: Self = Self(1 << 3);
    /// Run FFT, display spectrum, and spectrogram history.
    pub const SPECTRUM: Self = Self(1 << 4);
    /// Process no optional scope representation.
    pub const NONE: Self = Self(0);
    /// Process every representation.
    pub const ALL: Self = Self(
        Self::CORRELATION.0
            | Self::WAVEFORM.0
            | Self::GONIOMETER.0
            | Self::OSCILLOSCOPE.0
            | Self::SPECTRUM.0,
    );

    /// Returns whether `flag` is enabled by this work set.
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }

    /// Returns this work set with `flag` enabled.
    pub const fn with(self, flag: Self) -> Self {
        Self(self.0 | flag.0)
    }

    /// Returns this work set with `flag` disabled.
    pub const fn without(self, flag: Self) -> Self {
        Self(self.0 & !flag.0)
    }
}

/// Construction settings for [`ScopeAnalyzer`].
///
/// [`ScopeConfig::recommended`] creates values suited to a continuously open
/// stereo scope. Every buffer size is fixed when [`ScopeAnalyzer::new`] runs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScopeConfig {
    /// Audio sample rate in Hz.
    pub sample_rate: f64,
    /// Largest planar input block accepted by the analyzer.
    pub max_block_frames: usize,
    /// Length of rolling correlation and M/S measurement window, in seconds.
    pub correlation_window_seconds: f64,
    /// Number of chronological min/max waveform buckets retained.
    pub waveform_buckets: usize,
    /// Requested duration represented by all waveform buckets, in seconds.
    ///
    /// Actual duration is rounded to whole frames per bucket and can be up to
    /// one bucket longer than this value.
    pub waveform_seconds: f64,
    /// Number of recent goniometer points retained.
    pub goniometer_points: usize,
    /// Number of input frames between retained goniometer points.
    pub goniometer_decimation_frames: usize,
    /// Number of original stereo frames retained for oscilloscope output.
    ///
    /// This is separate from waveform envelope buckets: each slot preserves
    /// both original samples so triggered oscilloscope output can be
    /// resampled without touching audio-thread allocation.
    pub oscilloscope_frames: usize,
    /// Power-of-two FFT size used for spectrum analysis.
    pub fft_size: usize,
    /// Number of fresh frames required before another FFT is run.
    pub fft_hop_frames: usize,
    /// Number of display spectrum bands.
    pub spectrum_bands: usize,
    /// Initial distribution used for display spectrum bands.
    pub spectrum_scale: SpectrumScale,
    /// Number of chronological spectrum rows retained for spectrogram output.
    pub spectrogram_rows: usize,
    /// Low edge of first spectrum band, in Hz.
    pub min_spectrum_hz: f64,
    /// High edge of final spectrum band, in Hz, at or below Nyquist.
    pub max_spectrum_hz: f64,
    /// Spectrum rise time constant, in seconds. Zero disables attack smoothing.
    pub spectrum_attack_seconds: f64,
    /// Spectrum fall time constant, in seconds. Zero disables release smoothing.
    pub spectrum_release_seconds: f64,
}

impl ScopeConfig {
    /// Returns a practical stereo-scope configuration for `sample_rate`.
    pub fn recommended(
        sample_rate: f64,
        max_block_frames: usize,
    ) -> Result<Self, ScopeConfigError> {
        validate_sample_rate(sample_rate)?;
        if max_block_frames == 0 {
            return Err(ScopeConfigError::InvalidBlockSize);
        }

        Ok(Self {
            sample_rate,
            max_block_frames,
            correlation_window_seconds: 0.3,
            waveform_buckets: 256,
            waveform_seconds: 0.25,
            goniometer_points: 1_024,
            goniometer_decimation_frames: 8,
            // 0.34 s at 48 kHz: enough for several low-frequency cycles,
            // while stereo raw storage stays below 128 KiB.
            oscilloscope_frames: 16_384,
            fft_size: 2_048,
            fft_hop_frames: 1_024,
            spectrum_bands: 96,
            spectrum_scale: SpectrumScale::Log,
            spectrogram_rows: 256,
            min_spectrum_hz: 20.0,
            max_spectrum_hz: 20_000.0_f64.min(sample_rate * 0.5),
            spectrum_attack_seconds: 0.03,
            spectrum_release_seconds: 0.3,
        })
    }
}

impl Default for ScopeConfig {
    fn default() -> Self {
        // Constants above are valid by construction.
        Self::recommended(48_000.0, 1_024).expect("valid default scope configuration")
    }
}

/// Invalid [`ScopeConfig`] value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScopeConfigError {
    /// Sample rate is non-finite or below 8 kHz.
    InvalidSampleRate,
    /// Maximum input block size is zero.
    InvalidBlockSize,
    /// Correlation/M/S window duration is invalid or cannot fit memory.
    InvalidCorrelationWindow,
    /// Waveform bucket count is zero.
    InvalidWaveformBuckets,
    /// Waveform duration is invalid or cannot fit memory.
    InvalidWaveformDuration,
    /// Goniometer point count is zero.
    InvalidGoniometerPoints,
    /// Goniometer decimation is zero.
    InvalidGoniometerDecimation,
    /// Oscilloscope sample capacity is zero.
    InvalidOscilloscopeFrames,
    /// FFT size is not a power of two of at least sixteen frames.
    InvalidFftSize,
    /// FFT hop is zero or larger than FFT size.
    InvalidFftHop,
    /// Spectrum band count is zero.
    InvalidSpectrumBands,
    /// Spectrogram row count is zero.
    InvalidSpectrogramRows,
    /// Spectrum range is non-finite, empty, or exceeds Nyquist.
    InvalidSpectrumRange,
    /// Attack or release duration is non-finite or negative.
    InvalidBallistics,
    /// Fixed spectrogram storage cannot be represented.
    StorageTooLarge,
}

impl fmt::Display for ScopeConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidSampleRate => "sample rate must be finite and at least 8 kHz",
            Self::InvalidBlockSize => "maximum block size must be greater than zero",
            Self::InvalidCorrelationWindow => {
                "correlation window must be positive and fit addressable memory"
            }
            Self::InvalidWaveformBuckets => "waveform bucket count must be greater than zero",
            Self::InvalidWaveformDuration => {
                "waveform duration must be positive and fit addressable memory"
            }
            Self::InvalidGoniometerPoints => "goniometer point count must be greater than zero",
            Self::InvalidGoniometerDecimation => "goniometer decimation must be greater than zero",
            Self::InvalidOscilloscopeFrames => {
                "oscilloscope frame capacity must be greater than zero"
            }
            Self::InvalidFftSize => "FFT size must be a power of two of at least sixteen",
            Self::InvalidFftHop => "FFT hop must be between one frame and FFT size",
            Self::InvalidSpectrumBands => "spectrum band count must be greater than zero",
            Self::InvalidSpectrogramRows => "spectrogram row count must be greater than zero",
            Self::InvalidSpectrumRange => {
                "spectrum range must be finite, non-empty, and no higher than Nyquist"
            }
            Self::InvalidBallistics => {
                "spectrum attack and release must be finite and non-negative"
            }
            Self::StorageTooLarge => "fixed scope storage is too large to represent",
        };
        formatter.write_str(message)
    }
}

impl Error for ScopeConfigError {}

/// Input block rejected by [`ScopeAnalyzer`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScopeProcessError {
    /// Left and right slices do not have equal frame counts.
    UnevenChannels,
    /// Input exceeds maximum block size declared at construction.
    BlockTooLarge {
        /// Configured maximum frame count.
        maximum: usize,
        /// Received frame count.
        actual: usize,
    },
}

impl fmt::Display for ScopeProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnevenChannels => {
                formatter.write_str("left and right slices must have equal length")
            }
            Self::BlockTooLarge { maximum, actual } => {
                write!(
                    formatter,
                    "block has {actual} frames; configured maximum is {maximum}"
                )
            }
        }
    }
}

impl Error for ScopeProcessError {}

/// Min/max envelope for left and right samples in one waveform time bucket.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaveformBucket {
    /// Lowest finite left sample observed in this bucket.
    pub left_min: f32,
    /// Highest finite left sample observed in this bucket.
    pub left_max: f32,
    /// Lowest finite right sample observed in this bucket.
    pub right_min: f32,
    /// Highest finite right sample observed in this bucket.
    pub right_max: f32,
    /// Number of frames currently represented, never above configured bucket size.
    pub frames: usize,
}

impl WaveformBucket {
    const fn empty() -> Self {
        Self {
            left_min: f32::INFINITY,
            left_max: f32::NEG_INFINITY,
            right_min: f32::INFINITY,
            right_max: f32::NEG_INFINITY,
            frames: 0,
        }
    }

    fn clear(&mut self) {
        *self = Self::empty();
    }

    fn push(&mut self, left: f32, right: f32) {
        self.left_min = self.left_min.min(left);
        self.left_max = self.left_max.max(left);
        self.right_min = self.right_min.min(right);
        self.right_max = self.right_max.max(right);
        self.frames += 1;
    }
}

/// One stereo vectorscope point in mid/side coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GoniometerPoint {
    /// Horizontal coordinate: `(left - right) / 2`.
    pub side: f32,
    /// Vertical coordinate: `(left + right) / 2`.
    pub mid: f32,
}

/// One display spectrum band.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpectrumBand {
    /// Inclusive low frequency edge, in Hz.
    pub low_hz: f64,
    /// Inclusive high frequency edge, in Hz.
    pub high_hz: f64,
    /// Smoothed peak magnitude inside this band, in dBFS.
    pub level_dbfs: f32,
}

/// Current scalar state of [`ScopeAnalyzer`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScopeSnapshot {
    /// Pearson correlation over current rolling analysis window, from -1 to 1.
    ///
    /// `None` means one or both channels contain no energy in the window.
    pub correlation: Option<f32>,
    /// Left-channel RMS level over current rolling window, in dBFS.
    pub left_rms_dbfs: Option<f32>,
    /// Right-channel RMS level over current rolling window, in dBFS.
    pub right_rms_dbfs: Option<f32>,
    /// Mid RMS level over current rolling window, in dBFS.
    pub mid_rms_dbfs: Option<f32>,
    /// Side RMS level over current rolling window, in dBFS.
    pub side_rms_dbfs: Option<f32>,
    /// Normalized side-energy share from zero (mono) to one (anti-phase).
    ///
    /// Independent left/right material tends toward one half. `None` means
    /// the rolling window is silent.
    pub stereo_width: Option<f32>,
    /// Unbounded side-to-mid RMS ratio, when mid contains energy.
    pub side_to_mid_ratio: Option<f32>,
    /// `20 * log10(side_rms / mid_rms)`, when both signals contain energy.
    pub mid_side_balance_db: Option<f32>,
    /// Frames currently contributing to scalar measurements.
    pub analysis_frames: usize,
    /// Total frames accepted since construction or latest reset.
    pub processed_frames: u64,
    /// Total accepted duration, in seconds.
    pub duration_seconds: f64,
    /// Whether at least one complete FFT/spectrum frame is available.
    pub spectrum_ready: bool,
    /// Number of chronological spectrogram rows currently retained.
    pub spectrogram_rows: usize,
}

#[derive(Clone, Copy, Debug, Default)]
struct StereoEnergy {
    left_square: f64,
    right_square: f64,
    cross: f64,
    mid_square: f64,
    side_square: f64,
}

impl StereoEnergy {
    fn from_samples(left: f32, right: f32, mid: f32, side: f32) -> Self {
        let left = f64::from(left);
        let right = f64::from(right);
        let mid = f64::from(mid);
        let side = f64::from(side);
        Self {
            left_square: left * left,
            right_square: right * right,
            cross: left * right,
            mid_square: mid * mid,
            side_square: side * side,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Complex {
    real: f32,
    imaginary: f32,
}

impl Complex {
    fn multiply(self, other: Self) -> Self {
        Self {
            real: self.real * other.real - self.imaginary * other.imaginary,
            imaginary: self.real * other.imaginary + self.imaginary * other.real,
        }
    }

    fn add(self, other: Self) -> Self {
        Self {
            real: self.real + other.real,
            imaginary: self.imaginary + other.imaginary,
        }
    }

    fn subtract(self, other: Self) -> Self {
        Self {
            real: self.real - other.real,
            imaginary: self.imaginary - other.imaginary,
        }
    }

    fn magnitude(self) -> f32 {
        let square = self
            .real
            .mul_add(self.real, self.imaginary * self.imaginary);
        if square.is_finite() && square >= 0.0 {
            square.sqrt()
        } else {
            f32::MAX
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct SpectrumMapping {
    first_bin: usize,
    last_bin: usize,
    low_hz: f64,
    high_hz: f64,
}

/// Fixed-storage realtime stereo analyzer.
///
/// `process_stereo` accepts only bounded blocks and does not allocate, lock,
/// format text, or perform I/O. Read APIs either return borrowed fixed storage
/// or copy into caller-provided slices.
#[derive(Debug)]
pub struct ScopeAnalyzer {
    config: ScopeConfig,
    correlation_ring: Box<[StereoEnergy]>,
    correlation_cursor: usize,
    correlation_count: usize,
    left_square_sum: f64,
    right_square_sum: f64,
    cross_sum: f64,
    mid_square_sum: f64,
    side_square_sum: f64,
    waveform: Box<[WaveformBucket]>,
    waveform_current: usize,
    waveform_count: usize,
    waveform_frames_in_current: usize,
    waveform_frames_per_bucket: usize,
    goniometer: Box<[GoniometerPoint]>,
    goniometer_write: usize,
    goniometer_count: usize,
    goniometer_frames_until_point: usize,
    oscilloscope: Box<[OscilloscopeSample]>,
    oscilloscope_write: usize,
    oscilloscope_count: usize,
    fft_ring: Box<[f32]>,
    fft_cursor: usize,
    fft_count: usize,
    fft_frames_since_analysis: usize,
    fft_window: Box<[f32]>,
    fft_window_sum: f32,
    fft_bit_reverse: Box<[usize]>,
    fft_twiddles: Box<[Complex]>,
    fft_work: Box<[Complex]>,
    linear_spectrum: Box<[f32]>,
    spectrum_maps: [Box<[SpectrumMapping]>; 3],
    spectrum: Box<[SpectrumBand]>,
    spectrogram: Box<[f32]>,
    spectrogram_write: usize,
    spectrogram_count: usize,
    spectrum_ready: bool,
    spectrum_frames: u64,
    processed_frames: u64,
}

impl ScopeAnalyzer {
    /// Allocates every fixed buffer required by `config`.
    pub fn new(config: ScopeConfig) -> Result<Self, ScopeConfigError> {
        let derived = validate_config(config)?;
        let spectrogram_values = config
            .spectrogram_rows
            .checked_mul(config.spectrum_bands)
            .ok_or(ScopeConfigError::StorageTooLarge)?;

        let mut window = Vec::with_capacity(config.fft_size);
        let mut window_sum = 0.0_f32;
        for index in 0..config.fft_size {
            let phase = 2.0 * PI * index as f64 / (config.fft_size - 1) as f64;
            let value = (0.5 - 0.5 * phase.cos()) as f32;
            window_sum += value;
            window.push(value);
        }

        let fft_bits = config.fft_size.ilog2();
        let bit_reverse = (0..config.fft_size)
            .map(|index| reverse_low_bits(index, fft_bits))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let twiddles = (0..config.fft_size / 2)
            .map(|index| {
                let phase = -2.0 * PI * index as f64 / config.fft_size as f64;
                Complex {
                    real: phase.cos() as f32,
                    imaginary: phase.sin() as f32,
                }
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();

        // Every selectable display mapping is allocated once. Runtime scale
        // changes only swap which precomputed bin ranges are read.
        let spectrum_maps = SpectrumScale::ALL.map(|scale| build_spectrum_mapping(config, scale));
        let active_mapping = &spectrum_maps[config.spectrum_scale.storage_index()];
        let spectrum = active_mapping
            .iter()
            .map(|mapping| SpectrumBand {
                low_hz: mapping.low_hz,
                high_hz: mapping.high_hz,
                level_dbfs: SPECTRUM_FLOOR_DBFS,
            })
            .collect::<Vec<_>>();

        Ok(Self {
            config,
            correlation_ring: vec![StereoEnergy::default(); derived.correlation_frames]
                .into_boxed_slice(),
            correlation_cursor: 0,
            correlation_count: 0,
            left_square_sum: 0.0,
            right_square_sum: 0.0,
            cross_sum: 0.0,
            mid_square_sum: 0.0,
            side_square_sum: 0.0,
            waveform: vec![WaveformBucket::empty(); config.waveform_buckets].into_boxed_slice(),
            waveform_current: 0,
            waveform_count: 0,
            waveform_frames_in_current: 0,
            waveform_frames_per_bucket: derived.waveform_frames_per_bucket,
            goniometer: vec![GoniometerPoint::default(); config.goniometer_points]
                .into_boxed_slice(),
            goniometer_write: 0,
            goniometer_count: 0,
            goniometer_frames_until_point: 0,
            oscilloscope: vec![OscilloscopeSample::default(); config.oscilloscope_frames]
                .into_boxed_slice(),
            oscilloscope_write: 0,
            oscilloscope_count: 0,
            fft_ring: vec![0.0; config.fft_size].into_boxed_slice(),
            fft_cursor: 0,
            fft_count: 0,
            fft_frames_since_analysis: 0,
            fft_window: window.into_boxed_slice(),
            fft_window_sum: window_sum,
            fft_bit_reverse: bit_reverse,
            fft_twiddles: twiddles,
            fft_work: vec![Complex::default(); config.fft_size].into_boxed_slice(),
            // Non-negative-frequency FFT bins, including DC and Nyquist. This stays
            // linear-frequency so consumers such as Dalia can derive their
            // own musical bands without pretending logarithmic display
            // bands are FFT bins.
            linear_spectrum: vec![0.0; config.fft_size / 2 + 1].into_boxed_slice(),
            spectrum_maps,
            spectrum: spectrum.into_boxed_slice(),
            spectrogram: vec![SPECTRUM_FLOOR_DBFS; spectrogram_values].into_boxed_slice(),
            spectrogram_write: 0,
            spectrogram_count: 0,
            spectrum_ready: false,
            spectrum_frames: 0,
            processed_frames: 0,
        })
    }

    /// Creates an analyzer using [`ScopeConfig::recommended`].
    pub fn stereo(sample_rate: f64, max_block_frames: usize) -> Result<Self, ScopeConfigError> {
        Self::new(ScopeConfig::recommended(sample_rate, max_block_frames)?)
    }

    /// Processes one bounded planar stereo block without allocation.
    ///
    /// Non-finite input samples are treated as silence, preventing a bad host
    /// buffer from poisoning a visual frame.
    pub fn process_stereo(&mut self, left: &[f32], right: &[f32]) -> Result<(), ScopeProcessError> {
        self.process_stereo_masked(left, right, ScopeWork::ALL)
    }

    /// Processes one bounded planar stereo block with selected analysis work.
    ///
    /// The block shape checks match [`Self::process_stereo`]. Every accepted
    /// frame still advances total duration; disabled representations retain
    /// their last result and skip their work entirely.
    pub fn process_stereo_masked(
        &mut self,
        left: &[f32],
        right: &[f32],
        work: ScopeWork,
    ) -> Result<(), ScopeProcessError> {
        if left.len() != right.len() {
            return Err(ScopeProcessError::UnevenChannels);
        }
        if left.len() > self.config.max_block_frames {
            return Err(ScopeProcessError::BlockTooLarge {
                maximum: self.config.max_block_frames,
                actual: left.len(),
            });
        }

        for (&left, &right) in left.iter().zip(right) {
            self.push_frame_masked(sanitize_sample(left), sanitize_sample(right), work);
        }
        Ok(())
    }

    /// Processes one mono block as equal left and right channels.
    pub fn process_mono(&mut self, samples: &[f32]) -> Result<(), ScopeProcessError> {
        self.process_stereo(samples, samples)
    }

    /// Processes one mono block with selected analysis work.
    pub fn process_mono_masked(
        &mut self,
        samples: &[f32],
        work: ScopeWork,
    ) -> Result<(), ScopeProcessError> {
        self.process_stereo_masked(samples, samples, work)
    }

    /// Changes spectrum attack and release without reallocating any storage.
    pub fn set_spectrum_ballistics(
        &mut self,
        attack_seconds: f64,
        release_seconds: f64,
    ) -> Result<(), ScopeConfigError> {
        validate_ballistics(attack_seconds, release_seconds)?;
        self.config.spectrum_attack_seconds = attack_seconds;
        self.config.spectrum_release_seconds = release_seconds;
        Ok(())
    }

    /// Changes display spectrum grouping without allocating fixed storage.
    ///
    /// Existing display spectrum and spectrogram rows refer to previous band
    /// edges, so this clears that display history. FFT input history remains
    /// intact and produces a fresh row at its next scheduled hop.
    pub fn set_spectrum_scale(&mut self, scale: SpectrumScale) {
        if self.config.spectrum_scale == scale {
            return;
        }
        self.config.spectrum_scale = scale;
        self.sync_spectrum_band_edges();
        self.clear_spectrum_history();
    }

    /// Returns current display spectrum grouping.
    pub fn spectrum_scale(&self) -> SpectrumScale {
        self.config.spectrum_scale
    }

    /// Clears accumulated state while retaining all fixed allocations.
    pub fn reset(&mut self) {
        self.reset_work(ScopeWork::ALL);
        self.processed_frames = 0;
    }

    /// Clears selected retained representations without allocating.
    ///
    /// Use this when a hidden module becomes visible again: it prevents old
    /// waveform, scope, goniometer, or spectrum history from masquerading as
    /// fresh analysis while leaving other enabled modules uninterrupted.
    /// Global accepted-frame duration is intentionally preserved.
    pub fn reset_work(&mut self, work: ScopeWork) {
        if work.contains(ScopeWork::CORRELATION) {
            self.correlation_ring.fill(StereoEnergy::default());
            self.correlation_cursor = 0;
            self.correlation_count = 0;
            self.left_square_sum = 0.0;
            self.right_square_sum = 0.0;
            self.cross_sum = 0.0;
            self.mid_square_sum = 0.0;
            self.side_square_sum = 0.0;
        }
        if work.contains(ScopeWork::WAVEFORM) {
            self.waveform.fill(WaveformBucket::empty());
            self.waveform_current = 0;
            self.waveform_count = 0;
            self.waveform_frames_in_current = 0;
        }
        if work.contains(ScopeWork::GONIOMETER) {
            self.goniometer.fill(GoniometerPoint::default());
            self.goniometer_write = 0;
            self.goniometer_count = 0;
            self.goniometer_frames_until_point = 0;
        }
        if work.contains(ScopeWork::OSCILLOSCOPE) {
            self.oscilloscope.fill(OscilloscopeSample::default());
            self.oscilloscope_write = 0;
            self.oscilloscope_count = 0;
        }
        if work.contains(ScopeWork::SPECTRUM) {
            self.fft_ring.fill(0.0);
            self.fft_cursor = 0;
            self.fft_count = 0;
            self.fft_frames_since_analysis = 0;
            self.fft_work.fill(Complex::default());
            self.linear_spectrum.fill(0.0);
            self.clear_spectrum_history();
        }
    }

    /// Returns current scalar correlation, M/S, and spectrum state.
    pub fn snapshot(&self) -> ScopeSnapshot {
        let frames = self.correlation_count as f64;
        let left_rms = rms(self.left_square_sum, frames);
        let right_rms = rms(self.right_square_sum, frames);
        let mid_rms = rms(self.mid_square_sum, frames);
        let side_rms = rms(self.side_square_sum, frames);
        let correlation = match (left_rms, right_rms) {
            (Some(left), Some(right)) => {
                let denominator = left * right * frames;
                (denominator > 0.0).then(|| (self.cross_sum / denominator).clamp(-1.0, 1.0) as f32)
            }
            _ => None,
        };
        let stereo_width = match (mid_rms, side_rms) {
            (Some(mid), Some(side)) if mid + side > 0.0 => Some((side / (mid + side)) as f32),
            _ => None,
        };
        let side_to_mid_ratio = match (mid_rms, side_rms) {
            (Some(mid), Some(side)) if mid > 0.0 => Some((side / mid) as f32),
            _ => None,
        };
        let mid_side_balance_db = match (mid_rms, side_rms) {
            (Some(mid), Some(side)) if mid > 0.0 && side > 0.0 => {
                Some((20.0 * (side / mid).log10()) as f32)
            }
            _ => None,
        };

        ScopeSnapshot {
            correlation,
            left_rms_dbfs: left_rms.and_then(amplitude_to_dbfs),
            right_rms_dbfs: right_rms.and_then(amplitude_to_dbfs),
            mid_rms_dbfs: mid_rms.and_then(amplitude_to_dbfs),
            side_rms_dbfs: side_rms.and_then(amplitude_to_dbfs),
            stereo_width,
            side_to_mid_ratio,
            mid_side_balance_db,
            analysis_frames: self.correlation_count,
            processed_frames: self.processed_frames,
            duration_seconds: self.processed_frames as f64 / self.config.sample_rate,
            spectrum_ready: self.spectrum_ready,
            spectrogram_rows: self.spectrogram_count,
        }
    }

    /// Number of waveform buckets currently available, oldest first.
    pub fn waveform_bucket_count(&self) -> usize {
        self.waveform_count
    }

    /// Returns one waveform bucket by chronological index, oldest first.
    pub fn waveform_bucket(&self, chronological_index: usize) -> Option<WaveformBucket> {
        (chronological_index < self.waveform_count)
            .then(|| self.waveform[self.waveform_storage_index(chronological_index)])
    }

    /// Copies most recent waveform buckets into `destination`, oldest first.
    ///
    /// If destination is shorter than retained history, oldest omitted buckets
    /// are discarded and destination receives the newest contiguous range.
    pub fn copy_waveform_buckets(&self, destination: &mut [WaveformBucket]) -> usize {
        let amount = destination.len().min(self.waveform_count);
        let first = self.waveform_count - amount;
        for (index, bucket) in destination[..amount].iter_mut().enumerate() {
            *bucket = self.waveform[self.waveform_storage_index(first + index)];
        }
        amount
    }

    /// Number of goniometer points currently available, oldest first.
    pub fn goniometer_point_count(&self) -> usize {
        self.goniometer_count
    }

    /// Returns one goniometer point by chronological index, oldest first.
    pub fn goniometer_point(&self, chronological_index: usize) -> Option<GoniometerPoint> {
        (chronological_index < self.goniometer_count)
            .then(|| self.goniometer[self.goniometer_storage_index(chronological_index)])
    }

    /// Copies most recent goniometer points into `destination`, oldest first.
    ///
    /// If destination is shorter than retained history, it receives newest
    /// points rather than a sparse sample of full history.
    pub fn copy_goniometer_points(&self, destination: &mut [GoniometerPoint]) -> usize {
        let amount = destination.len().min(self.goniometer_count);
        let first = self.goniometer_count - amount;
        for (index, point) in destination[..amount].iter_mut().enumerate() {
            *point = self.goniometer[self.goniometer_storage_index(first + index)];
        }
        amount
    }

    /// Number of original oscilloscope frames currently retained.
    pub fn oscilloscope_frame_count(&self) -> usize {
        self.oscilloscope_count
    }

    /// Copies a bounded stereo oscilloscope trace into caller-owned storage.
    ///
    /// `Free` copies newest chronological audio. `Pitch` searches the
    /// strongest finite channel for two rising threshold crossings, then
    /// resamples one or up to three complete periods. Silence, DC, short
    /// buffers, and untriggerable material fall back to newest chronological
    /// audio. No path allocates or mutates retained source storage.
    pub fn copy_oscilloscope(
        &self,
        destination: &mut [OscilloscopeSample],
        mode: OscilloscopeMode,
        cycles: OscilloscopeCycles,
    ) -> usize {
        let amount = destination.len().min(self.oscilloscope_count);
        if amount == 0 {
            return 0;
        }

        let (start, frames) = match mode {
            OscilloscopeMode::Free => (self.oscilloscope_count - amount, amount),
            OscilloscopeMode::Pitch => self
                .pitch_oscilloscope_window(cycles)
                .unwrap_or((self.oscilloscope_count - amount, amount)),
        };
        self.copy_oscilloscope_window(destination, amount, start, frames);
        amount
    }

    /// Returns all current display spectrum bands, low frequency first.
    pub fn spectrum(&self) -> &[SpectrumBand] {
        &self.spectrum
    }

    /// Most recent non-negative-frequency FFT magnitudes, DC first.
    ///
    /// Values are finite, linear amplitudes in `[0, 1]`. The fixed slice has
    /// `fft_size() / 2 + 1` elements, including Nyquist, and is updated whenever a new spectrum frame
    /// is available. It is intended for consumers that need true
    /// linear-frequency bins; the regular [`Self::spectrum`] remains the
    /// selected display-scale representation.
    pub fn linear_spectrum(&self) -> &[f32] {
        &self.linear_spectrum
    }

    /// Number of complete FFT frames produced since construction or reset.
    pub fn spectrum_frame_count(&self) -> u64 {
        self.spectrum_frames
    }

    /// Number of spectrogram rows currently retained, oldest first.
    pub fn spectrogram_row_count(&self) -> usize {
        self.spectrogram_count
    }

    /// Number of bands in every returned spectrogram row.
    pub fn spectrogram_band_count(&self) -> usize {
        self.spectrum.len()
    }

    /// Returns one chronological spectrogram row, oldest first.
    ///
    /// Each element corresponds to [`Self::spectrum`] at same band index and
    /// holds its smoothed dBFS level when row was captured.
    pub fn spectrogram_row(&self, chronological_index: usize) -> Option<&[f32]> {
        if chronological_index >= self.spectrogram_count {
            return None;
        }
        let row = self.spectrogram_storage_index(chronological_index);
        let width = self.spectrum.len();
        Some(&self.spectrogram[row * width..(row + 1) * width])
    }

    /// Configured maximum accepted input block length.
    pub fn max_block_frames(&self) -> usize {
        self.config.max_block_frames
    }

    /// FFT size used by current spectrum analyzer.
    pub fn fft_size(&self) -> usize {
        self.config.fft_size
    }

    fn push_frame_masked(&mut self, left: f32, right: f32, work: ScopeWork) {
        let mid = ((f64::from(left) + f64::from(right)) * 0.5) as f32;
        let side = ((f64::from(left) - f64::from(right)) * 0.5) as f32;
        if work.contains(ScopeWork::CORRELATION) {
            self.push_energy(left, right, mid, side);
        }
        if work.contains(ScopeWork::WAVEFORM) {
            self.push_waveform(left, right);
        }
        if work.contains(ScopeWork::GONIOMETER) {
            self.push_goniometer(side, mid);
        }
        if work.contains(ScopeWork::OSCILLOSCOPE) {
            self.push_oscilloscope(left, right);
        }
        if work.contains(ScopeWork::SPECTRUM) {
            self.push_fft(mid);
        }
        self.processed_frames = self.processed_frames.saturating_add(1);
    }

    fn push_energy(&mut self, left: f32, right: f32, mid: f32, side: f32) {
        let incoming = StereoEnergy::from_samples(left, right, mid, side);
        let outgoing = self.correlation_ring[self.correlation_cursor];
        self.correlation_ring[self.correlation_cursor] = incoming;
        self.correlation_cursor = (self.correlation_cursor + 1) % self.correlation_ring.len();
        self.correlation_count = (self.correlation_count + 1).min(self.correlation_ring.len());
        self.left_square_sum += incoming.left_square - outgoing.left_square;
        self.right_square_sum += incoming.right_square - outgoing.right_square;
        self.cross_sum += incoming.cross - outgoing.cross;
        self.mid_square_sum += incoming.mid_square - outgoing.mid_square;
        self.side_square_sum += incoming.side_square - outgoing.side_square;
    }

    fn push_waveform(&mut self, left: f32, right: f32) {
        if self.waveform_frames_in_current == self.waveform_frames_per_bucket {
            self.waveform_current = (self.waveform_current + 1) % self.waveform.len();
            self.waveform[self.waveform_current].clear();
            self.waveform_frames_in_current = 0;
        }
        if self.waveform_frames_in_current == 0 {
            self.waveform_count = (self.waveform_count + 1).min(self.waveform.len());
        }
        self.waveform[self.waveform_current].push(left, right);
        self.waveform_frames_in_current += 1;
    }

    fn push_goniometer(&mut self, side: f32, mid: f32) {
        if self.goniometer_frames_until_point == 0 {
            self.goniometer[self.goniometer_write] = GoniometerPoint { side, mid };
            self.goniometer_write = (self.goniometer_write + 1) % self.goniometer.len();
            self.goniometer_count = (self.goniometer_count + 1).min(self.goniometer.len());
            self.goniometer_frames_until_point = self.config.goniometer_decimation_frames - 1;
        } else {
            self.goniometer_frames_until_point -= 1;
        }
    }

    fn push_oscilloscope(&mut self, left: f32, right: f32) {
        self.oscilloscope[self.oscilloscope_write] = OscilloscopeSample { left, right };
        self.oscilloscope_write = (self.oscilloscope_write + 1) % self.oscilloscope.len();
        self.oscilloscope_count = (self.oscilloscope_count + 1).min(self.oscilloscope.len());
    }

    fn push_fft(&mut self, sample: f32) {
        let already_full = self.fft_count == self.fft_ring.len();
        self.fft_ring[self.fft_cursor] = sample;
        self.fft_cursor = (self.fft_cursor + 1) % self.fft_ring.len();

        if !already_full {
            self.fft_count += 1;
            if self.fft_count == self.fft_ring.len() {
                self.fft_frames_since_analysis = 0;
                self.update_spectrum();
            }
        } else {
            self.fft_frames_since_analysis += 1;
            if self.fft_frames_since_analysis == self.config.fft_hop_frames {
                self.fft_frames_since_analysis = 0;
                self.update_spectrum();
            }
        }
    }

    fn update_spectrum(&mut self) {
        for input_index in 0..self.fft_ring.len() {
            let source_index = (self.fft_cursor + input_index) % self.fft_ring.len();
            let destination_index = self.fft_bit_reverse[input_index];
            self.fft_work[destination_index] = Complex {
                real: self.fft_ring[source_index] * self.fft_window[input_index],
                imaginary: 0.0,
            };
        }
        fft_in_place(&mut self.fft_work, &self.fft_twiddles);

        for (bin, output) in self.linear_spectrum.iter_mut().enumerate() {
            let amplitude = 2.0 * self.fft_work[bin].magnitude() / self.fft_window_sum;
            *output = if amplitude.is_finite() {
                amplitude.clamp(0.0, 1.0)
            } else {
                0.0
            };
        }

        let row_offset = self.spectrogram_write * self.spectrum.len();
        let seconds_per_frame = self.config.fft_hop_frames as f64 / self.config.sample_rate;
        for index in 0..self.spectrum.len() {
            let mapping = self.spectrum_maps[self.config.spectrum_scale.storage_index()][index];
            let mut magnitude = 0.0_f32;
            for bin in mapping.first_bin..=mapping.last_bin {
                magnitude = magnitude.max(self.linear_spectrum[bin]);
            }
            let target = dbfs_with_floor(magnitude);
            let previous = self.spectrum[index].level_dbfs;
            let time_constant = if target > previous {
                self.config.spectrum_attack_seconds
            } else {
                self.config.spectrum_release_seconds
            };
            let level = if self.spectrum_ready {
                let coefficient = ballistics_coefficient(time_constant, seconds_per_frame);
                target + (previous - target) * coefficient
            } else {
                target
            };
            self.spectrum[index].level_dbfs = level;
            self.spectrogram[row_offset + index] = level;
        }
        self.spectrogram_write = (self.spectrogram_write + 1) % self.config.spectrogram_rows;
        self.spectrogram_count = (self.spectrogram_count + 1).min(self.config.spectrogram_rows);
        self.spectrum_ready = true;
        self.spectrum_frames = self.spectrum_frames.saturating_add(1);
    }

    fn waveform_storage_index(&self, chronological_index: usize) -> usize {
        if self.waveform_count < self.waveform.len() {
            chronological_index
        } else {
            (self.waveform_current + 1 + chronological_index) % self.waveform.len()
        }
    }

    fn goniometer_storage_index(&self, chronological_index: usize) -> usize {
        if self.goniometer_count < self.goniometer.len() {
            chronological_index
        } else {
            (self.goniometer_write + chronological_index) % self.goniometer.len()
        }
    }

    fn oscilloscope_storage_index(&self, chronological_index: usize) -> usize {
        if self.oscilloscope_count < self.oscilloscope.len() {
            chronological_index
        } else {
            (self.oscilloscope_write + chronological_index) % self.oscilloscope.len()
        }
    }

    fn oscilloscope_sample(&self, chronological_index: usize) -> OscilloscopeSample {
        self.oscilloscope[self.oscilloscope_storage_index(chronological_index)]
    }

    fn pitch_oscilloscope_window(&self, cycles: OscilloscopeCycles) -> Option<(usize, usize)> {
        if self.oscilloscope_count < 4 {
            return None;
        }

        let mut left_peak = 0.0_f32;
        let mut right_peak = 0.0_f32;
        for index in 0..self.oscilloscope_count {
            let sample = self.oscilloscope_sample(index);
            left_peak = left_peak.max(sample.left.abs());
            right_peak = right_peak.max(sample.right.abs());
        }
        let use_left = left_peak >= right_peak;
        let peak = if use_left { left_peak } else { right_peak };
        // Relative hysteresis rejects low-level noise; floor keeps a normal
        // audio waveform triggerable even after a near-silent section.
        let threshold = (peak * 0.05).max(0.000_01);
        if peak <= threshold {
            return None;
        }

        let sample_value = |index: usize| {
            let sample = self.oscilloscope_sample(index);
            if use_left { sample.left } else { sample.right }
        };
        let mut prior_crossing = None;
        let mut latest_crossing = None;
        let mut previous = sample_value(0);
        for index in 1..self.oscilloscope_count {
            let current = sample_value(index);
            if previous <= -threshold && current >= threshold {
                prior_crossing = latest_crossing;
                latest_crossing = Some(index);
            }
            previous = current;
        }
        let (prior_crossing, latest_crossing) = prior_crossing.zip(latest_crossing)?;
        let period = latest_crossing.checked_sub(prior_crossing)?;
        if period < 2 {
            return None;
        }
        let requested = match cycles {
            OscilloscopeCycles::Single => period,
            OscilloscopeCycles::Multi => period.saturating_mul(3),
        };
        let start = latest_crossing.saturating_sub(requested);
        let frames = latest_crossing.saturating_sub(start);
        (frames >= 2).then_some((start, frames))
    }

    fn copy_oscilloscope_window(
        &self,
        destination: &mut [OscilloscopeSample],
        amount: usize,
        start: usize,
        frames: usize,
    ) {
        debug_assert!(amount <= destination.len());
        debug_assert!(frames > 0);
        debug_assert!(start + frames <= self.oscilloscope_count);
        if amount == 1 || frames == 1 {
            destination[0] = self.oscilloscope_sample(start);
            return;
        }

        let source_width = (frames - 1) as f64;
        let destination_width = (amount - 1) as f64;
        for (index, output) in destination[..amount].iter_mut().enumerate() {
            let position = index as f64 * source_width / destination_width;
            let first = position as usize;
            let second = (first + 1).min(frames - 1);
            let fraction = (position - first as f64) as f32;
            let first = self.oscilloscope_sample(start + first);
            let second = self.oscilloscope_sample(start + second);
            *output = OscilloscopeSample {
                left: first.left + (second.left - first.left) * fraction,
                right: first.right + (second.right - first.right) * fraction,
            };
        }
    }

    fn sync_spectrum_band_edges(&mut self) {
        let mapping = &self.spectrum_maps[self.config.spectrum_scale.storage_index()];
        for (band, mapping) in self.spectrum.iter_mut().zip(mapping) {
            band.low_hz = mapping.low_hz;
            band.high_hz = mapping.high_hz;
        }
    }

    fn clear_spectrum_history(&mut self) {
        for band in &mut self.spectrum {
            band.level_dbfs = SPECTRUM_FLOOR_DBFS;
        }
        self.spectrogram.fill(SPECTRUM_FLOOR_DBFS);
        self.spectrogram_write = 0;
        self.spectrogram_count = 0;
        self.spectrum_ready = false;
        self.spectrum_frames = 0;
    }

    fn spectrogram_storage_index(&self, chronological_index: usize) -> usize {
        if self.spectrogram_count < self.config.spectrogram_rows {
            chronological_index
        } else {
            (self.spectrogram_write + chronological_index) % self.config.spectrogram_rows
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct DerivedConfig {
    correlation_frames: usize,
    waveform_frames_per_bucket: usize,
}

fn validate_config(config: ScopeConfig) -> Result<DerivedConfig, ScopeConfigError> {
    validate_sample_rate(config.sample_rate)?;
    if config.max_block_frames == 0 {
        return Err(ScopeConfigError::InvalidBlockSize);
    }
    let correlation_frames = seconds_to_frames(
        config.sample_rate,
        config.correlation_window_seconds,
        ScopeConfigError::InvalidCorrelationWindow,
    )?;
    if config.waveform_buckets == 0 {
        return Err(ScopeConfigError::InvalidWaveformBuckets);
    }
    let waveform_frames = seconds_to_frames(
        config.sample_rate,
        config.waveform_seconds,
        ScopeConfigError::InvalidWaveformDuration,
    )?;
    let waveform_frames_per_bucket = waveform_frames
        .checked_add(config.waveform_buckets - 1)
        .ok_or(ScopeConfigError::StorageTooLarge)?
        / config.waveform_buckets;
    if config.goniometer_points == 0 {
        return Err(ScopeConfigError::InvalidGoniometerPoints);
    }
    if config.goniometer_decimation_frames == 0 {
        return Err(ScopeConfigError::InvalidGoniometerDecimation);
    }
    if config.oscilloscope_frames == 0 {
        return Err(ScopeConfigError::InvalidOscilloscopeFrames);
    }
    if config.fft_size < 16 || !config.fft_size.is_power_of_two() {
        return Err(ScopeConfigError::InvalidFftSize);
    }
    if config.fft_hop_frames == 0 || config.fft_hop_frames > config.fft_size {
        return Err(ScopeConfigError::InvalidFftHop);
    }
    if config.spectrum_bands == 0 {
        return Err(ScopeConfigError::InvalidSpectrumBands);
    }
    if config.spectrogram_rows == 0 {
        return Err(ScopeConfigError::InvalidSpectrogramRows);
    }
    if !config.min_spectrum_hz.is_finite()
        || !config.max_spectrum_hz.is_finite()
        || config.min_spectrum_hz <= 0.0
        || config.max_spectrum_hz <= config.min_spectrum_hz
        || config.max_spectrum_hz > config.sample_rate * 0.5
    {
        return Err(ScopeConfigError::InvalidSpectrumRange);
    }
    validate_ballistics(
        config.spectrum_attack_seconds,
        config.spectrum_release_seconds,
    )?;
    config
        .spectrogram_rows
        .checked_mul(config.spectrum_bands)
        .ok_or(ScopeConfigError::StorageTooLarge)?;

    Ok(DerivedConfig {
        correlation_frames,
        waveform_frames_per_bucket,
    })
}

fn validate_sample_rate(sample_rate: f64) -> Result<(), ScopeConfigError> {
    if sample_rate.is_finite() && sample_rate >= 8_000.0 {
        Ok(())
    } else {
        Err(ScopeConfigError::InvalidSampleRate)
    }
}

fn validate_ballistics(attack_seconds: f64, release_seconds: f64) -> Result<(), ScopeConfigError> {
    if attack_seconds.is_finite()
        && release_seconds.is_finite()
        && attack_seconds >= 0.0
        && release_seconds >= 0.0
    {
        Ok(())
    } else {
        Err(ScopeConfigError::InvalidBallistics)
    }
}

fn seconds_to_frames(
    sample_rate: f64,
    seconds: f64,
    error: ScopeConfigError,
) -> Result<usize, ScopeConfigError> {
    let frames = (sample_rate * seconds).round();
    if seconds.is_finite()
        && seconds > 0.0
        && frames.is_finite()
        && frames >= 1.0
        && frames <= usize::MAX as f64
    {
        Ok(frames as usize)
    } else {
        Err(error)
    }
}

fn build_spectrum_mapping(config: ScopeConfig, scale: SpectrumScale) -> Box<[SpectrumMapping]> {
    let mut mapping = Vec::with_capacity(config.spectrum_bands);
    let nyquist_bin = config.fft_size / 2;
    let min_mel = hz_to_mel(config.min_spectrum_hz);
    let max_mel = hz_to_mel(config.max_spectrum_hz);
    let log_ratio = config.max_spectrum_hz / config.min_spectrum_hz;
    for index in 0..config.spectrum_bands {
        let low_position = index as f64 / config.spectrum_bands as f64;
        let high_position = (index + 1) as f64 / config.spectrum_bands as f64;
        let (low_hz, high_hz) = match scale {
            SpectrumScale::Linear => (
                config.min_spectrum_hz
                    + (config.max_spectrum_hz - config.min_spectrum_hz) * low_position,
                config.min_spectrum_hz
                    + (config.max_spectrum_hz - config.min_spectrum_hz) * high_position,
            ),
            SpectrumScale::Mel => (
                mel_to_hz(min_mel + (max_mel - min_mel) * low_position),
                mel_to_hz(min_mel + (max_mel - min_mel) * high_position),
            ),
            SpectrumScale::Log => (
                config.min_spectrum_hz * log_ratio.powf(low_position),
                config.min_spectrum_hz * log_ratio.powf(high_position),
            ),
        };
        let first_bin =
            frequency_to_bin(low_hz, config.sample_rate, config.fft_size).clamp(1, nyquist_bin);
        let last_bin = frequency_to_bin(high_hz, config.sample_rate, config.fft_size)
            .clamp(first_bin, nyquist_bin);
        mapping.push(SpectrumMapping {
            first_bin,
            last_bin,
            low_hz,
            high_hz,
        });
    }
    mapping.into_boxed_slice()
}

fn hz_to_mel(frequency_hz: f64) -> f64 {
    2_595.0 * (1.0 + frequency_hz / 700.0).log10()
}

fn mel_to_hz(mel: f64) -> f64 {
    700.0 * (10.0_f64.powf(mel / 2_595.0) - 1.0)
}

fn frequency_to_bin(frequency_hz: f64, sample_rate: f64, fft_size: usize) -> usize {
    (frequency_hz * fft_size as f64 / sample_rate).floor() as usize
}

fn reverse_low_bits(value: usize, bit_count: u32) -> usize {
    let mut source = value;
    let mut reversed = 0_usize;
    for _ in 0..bit_count {
        reversed = (reversed << 1) | (source & 1);
        source >>= 1;
    }
    reversed
}

fn fft_in_place(values: &mut [Complex], twiddles: &[Complex]) {
    let mut stage_width = 2;
    while stage_width <= values.len() {
        let half_width = stage_width / 2;
        let twiddle_step = values.len() / stage_width;
        for group_start in (0..values.len()).step_by(stage_width) {
            for offset in 0..half_width {
                let lower = values[group_start + offset];
                let upper = values[group_start + offset + half_width]
                    .multiply(twiddles[offset * twiddle_step]);
                values[group_start + offset] = lower.add(upper);
                values[group_start + offset + half_width] = lower.subtract(upper);
            }
        }
        if stage_width == values.len() {
            break;
        }
        stage_width *= 2;
    }
}

fn sanitize_sample(sample: f32) -> f32 {
    if sample.is_finite() { sample } else { 0.0 }
}

fn rms(square_sum: f64, frames: f64) -> Option<f64> {
    (frames > 0.0).then(|| (square_sum.max(0.0) / frames).sqrt())
}

fn amplitude_to_dbfs(amplitude: f64) -> Option<f32> {
    (amplitude > 0.0 && amplitude.is_finite()).then(|| (20.0 * amplitude.log10()) as f32)
}

fn dbfs_with_floor(amplitude: f32) -> f32 {
    let amplitude = if amplitude.is_finite() {
        amplitude.max(SPECTRUM_FLOOR_AMPLITUDE)
    } else {
        f32::MAX
    };
    (20.0 * amplitude.log10()).max(SPECTRUM_FLOOR_DBFS)
}

fn ballistics_coefficient(time_constant_seconds: f64, elapsed_seconds: f64) -> f32 {
    if time_constant_seconds == 0.0 {
        0.0
    } else {
        (-elapsed_seconds / time_constant_seconds).exp() as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RATE: f64 = 48_000.0;

    fn config() -> ScopeConfig {
        ScopeConfig {
            sample_rate: SAMPLE_RATE,
            max_block_frames: 128,
            correlation_window_seconds: 8.0 / SAMPLE_RATE,
            waveform_buckets: 4,
            waveform_seconds: 8.0 / SAMPLE_RATE,
            goniometer_points: 3,
            goniometer_decimation_frames: 2,
            oscilloscope_frames: 16,
            fft_size: 64,
            fft_hop_frames: 32,
            spectrum_bands: 16,
            spectrum_scale: SpectrumScale::Log,
            spectrogram_rows: 2,
            min_spectrum_hz: 100.0,
            max_spectrum_hz: 20_000.0,
            spectrum_attack_seconds: 0.0,
            spectrum_release_seconds: 0.0,
        }
    }

    fn assert_close(actual: f32, expected: f32, tolerance: f32) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "expected {expected} +/- {tolerance}, got {actual}"
        );
    }

    #[test]
    fn recommended_config_tracks_sample_rate_and_nyquist() {
        let config = ScopeConfig::recommended(8_000.0, 64).unwrap();
        assert_eq!(config.max_spectrum_hz, 4_000.0);
        assert_eq!(config.max_block_frames, 64);
    }

    #[test]
    fn constructor_rejects_invalid_fixed_storage() {
        let mut invalid = config();
        invalid.fft_size = 63;
        assert_eq!(
            ScopeAnalyzer::new(invalid).unwrap_err(),
            ScopeConfigError::InvalidFftSize
        );

        invalid = config();
        invalid.spectrogram_rows = 0;
        assert_eq!(
            ScopeAnalyzer::new(invalid).unwrap_err(),
            ScopeConfigError::InvalidSpectrogramRows
        );

        invalid = config();
        invalid.max_spectrum_hz = SAMPLE_RATE;
        assert_eq!(
            ScopeAnalyzer::new(invalid).unwrap_err(),
            ScopeConfigError::InvalidSpectrumRange
        );

        invalid = config();
        invalid.oscilloscope_frames = 0;
        assert_eq!(
            ScopeAnalyzer::new(invalid).unwrap_err(),
            ScopeConfigError::InvalidOscilloscopeFrames
        );
    }

    #[test]
    fn process_rejects_bad_shape_before_mutating() {
        let mut analyzer = ScopeAnalyzer::new(config()).unwrap();
        assert_eq!(
            analyzer.process_stereo(&[0.0, 1.0], &[0.0]),
            Err(ScopeProcessError::UnevenChannels)
        );
        assert_eq!(analyzer.snapshot().processed_frames, 0);

        let samples = [0.0_f32; 129];
        assert_eq!(
            analyzer.process_stereo(&samples, &samples),
            Err(ScopeProcessError::BlockTooLarge {
                maximum: 128,
                actual: 129,
            })
        );
        assert_eq!(analyzer.snapshot().processed_frames, 0);
    }

    #[test]
    fn identical_channels_are_mono_and_fully_correlated() {
        let mut analyzer = ScopeAnalyzer::new(config()).unwrap();
        let samples = [0.5, -0.5, 0.25, -0.25, 0.75, -0.75, 0.125, -0.125];
        analyzer.process_mono(&samples).unwrap();
        let snapshot = analyzer.snapshot();
        assert_close(snapshot.correlation.unwrap(), 1.0, 0.000_01);
        assert_close(snapshot.stereo_width.unwrap(), 0.0, 0.000_01);
        assert_eq!(snapshot.side_rms_dbfs, None);
        assert!(snapshot.mid_rms_dbfs.is_some());
    }

    #[test]
    fn anti_phase_channels_are_wide_and_negatively_correlated() {
        let mut analyzer = ScopeAnalyzer::new(config()).unwrap();
        let left = [0.5, -0.5, 0.25, -0.25, 0.75, -0.75, 0.125, -0.125];
        let right = left.map(|sample| -sample);
        analyzer.process_stereo(&left, &right).unwrap();
        let snapshot = analyzer.snapshot();
        assert_close(snapshot.correlation.unwrap(), -1.0, 0.000_01);
        assert_close(snapshot.stereo_width.unwrap(), 1.0, 0.000_01);
        assert_eq!(snapshot.mid_rms_dbfs, None);
        assert!(snapshot.side_rms_dbfs.is_some());
        assert_eq!(snapshot.side_to_mid_ratio, None);
    }

    #[test]
    fn waveform_buckets_keep_chronological_min_max_after_wrap() {
        let mut analyzer = ScopeAnalyzer::new(config()).unwrap();
        let left = [-1.0, 0.5, -0.5, 0.25, 0.75, -0.25, 0.4, -0.8, 0.8, -0.4];
        let right = left.map(|sample| sample * 0.5);
        analyzer.process_stereo(&left, &right).unwrap();

        assert_eq!(analyzer.waveform_bucket_count(), 4);
        let mut buckets = [WaveformBucket::empty(); 4];
        assert_eq!(analyzer.copy_waveform_buckets(&mut buckets), 4);
        assert_eq!(buckets[0].frames, 2);
        assert_close(buckets[0].left_min, -0.5, 0.000_01);
        assert_close(buckets[0].left_max, 0.25, 0.000_01);
        assert_close(buckets[2].left_min, -0.8, 0.000_01);
        assert_close(buckets[2].left_max, 0.4, 0.000_01);
        assert_close(buckets[3].left_min, -0.4, 0.000_01);
        assert_close(buckets[3].left_max, 0.8, 0.000_01);
    }

    #[test]
    fn goniometer_decimates_and_wraps_in_chronological_order() {
        let mut analyzer = ScopeAnalyzer::new(config()).unwrap();
        let left = [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0];
        let right = [0.0; 8];
        analyzer.process_stereo(&left, &right).unwrap();

        let mut points = [GoniometerPoint::default(); 3];
        assert_eq!(analyzer.copy_goniometer_points(&mut points), 3);
        assert_close(points[0].mid, 1.0, 0.000_01);
        assert_close(points[1].mid, 2.0, 0.000_01);
        assert_close(points[2].mid, 3.0, 0.000_01);
        assert_close(points[0].side, 1.0, 0.000_01);
    }

    #[test]
    fn fft_detects_bin_centered_tone_and_writes_spectrogram() {
        let mut analyzer = ScopeAnalyzer::new(config()).unwrap();
        let frequency = 6_000.0;
        let mut samples = [0.0_f32; 64];
        for (index, sample) in samples.iter_mut().enumerate() {
            *sample = (2.0 * PI * frequency * index as f64 / SAMPLE_RATE).sin() as f32;
        }
        analyzer.process_mono(&samples).unwrap();

        let tone_band = analyzer
            .spectrum()
            .iter()
            .find(|band| band.low_hz <= frequency && frequency <= band.high_hz)
            .unwrap();
        assert!(
            tone_band.level_dbfs > -0.2,
            "got {} dBFS",
            tone_band.level_dbfs
        );
        assert!(
            tone_band.level_dbfs < 0.2,
            "got {} dBFS",
            tone_band.level_dbfs
        );
        assert_eq!(analyzer.spectrum_frame_count(), 1);
        assert_eq!(analyzer.spectrogram_row_count(), 1);
        assert_eq!(
            analyzer.spectrogram_row(0).unwrap().len(),
            analyzer.spectrogram_band_count()
        );
    }

    #[test]
    fn linear_spectrum_keeps_true_fft_bin_positions() {
        let mut analyzer = ScopeAnalyzer::new(config()).unwrap();
        let frequency = 6_000.0;
        let mut samples = [0.0_f32; 64];
        for (index, sample) in samples.iter_mut().enumerate() {
            *sample = (2.0 * PI * frequency * index as f64 / SAMPLE_RATE).sin() as f32;
        }
        analyzer.process_mono(&samples).unwrap();

        // The test config uses a 64-point FFT at 48 kHz, so 6 kHz is bin 8.
        let bins = analyzer.linear_spectrum();
        assert_eq!(bins.len(), 33);
        assert!(bins[8] > 0.95, "bin 8 magnitude was {}", bins[8]);
        assert!(bins[7] < 0.55, "adjacent bin leaked {}", bins[7]);
        assert!(bins[9] < 0.55, "adjacent bin leaked {}", bins[9]);
    }

    #[test]
    fn spectrum_scale_swaps_preallocated_mappings_and_clears_display_history() {
        let mut analyzer = ScopeAnalyzer::new(config()).unwrap();
        let spectrum_storage = analyzer.spectrum.as_ptr();
        analyzer.process_mono(&[0.5; 64]).unwrap();
        assert_eq!(analyzer.spectrum_frame_count(), 1);
        let log_first = analyzer.spectrum()[0];

        analyzer.set_spectrum_scale(SpectrumScale::Linear);
        assert_eq!(analyzer.spectrum.as_ptr(), spectrum_storage);
        assert_eq!(analyzer.spectrum_scale(), SpectrumScale::Linear);
        assert_eq!(analyzer.spectrum_frame_count(), 0);
        assert_eq!(analyzer.spectrogram_row_count(), 0);
        let linear_first = analyzer.spectrum()[0];
        assert_eq!(linear_first.low_hz, 100.0);
        assert!(linear_first.high_hz > log_first.high_hz);

        analyzer.set_spectrum_scale(SpectrumScale::Mel);
        assert_eq!(analyzer.spectrum_scale(), SpectrumScale::Mel);
        assert!(analyzer.spectrum()[0].high_hz < linear_first.high_hz);
    }

    #[test]
    fn masked_processing_skips_fft_but_retains_requested_oscilloscope_samples() {
        let mut analyzer = ScopeAnalyzer::new(config()).unwrap();
        analyzer
            .process_mono_masked(&[0.25; 64], ScopeWork::OSCILLOSCOPE)
            .unwrap();
        assert_eq!(analyzer.spectrum_frame_count(), 0);
        assert_eq!(analyzer.snapshot().correlation, None);
        assert_eq!(analyzer.oscilloscope_frame_count(), 16);
        let mut output = [OscilloscopeSample::default(); 4];
        assert_eq!(
            analyzer.copy_oscilloscope(
                &mut output,
                OscilloscopeMode::Free,
                OscilloscopeCycles::Multi
            ),
            4
        );
        assert!(
            output
                .iter()
                .all(|sample| sample.left == 0.25 && sample.right == 0.25)
        );
    }

    #[test]
    fn oscilloscope_free_copy_keeps_newest_chronological_samples_after_wrap() {
        let mut analyzer = ScopeAnalyzer::new(config()).unwrap();
        let left = [
            0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0, 13.0, 14.0, 15.0,
            16.0, 17.0, 18.0, 19.0,
        ];
        analyzer
            .process_stereo_masked(&left, &left, ScopeWork::OSCILLOSCOPE)
            .unwrap();
        let mut output = [OscilloscopeSample::default(); 4];
        assert_eq!(
            analyzer.copy_oscilloscope(
                &mut output,
                OscilloscopeMode::Free,
                OscilloscopeCycles::Single
            ),
            4
        );
        for (index, sample) in output.iter().enumerate() {
            assert_close(sample.left, 16.0 + index as f32, 0.000_01);
            assert_close(sample.right, 16.0 + index as f32, 0.000_01);
        }
    }

    #[test]
    fn pitch_oscilloscope_aligns_complete_cycles_without_mutating_raw_history() {
        let mut pitch_config = config();
        pitch_config.oscilloscope_frames = 128;
        let mut analyzer = ScopeAnalyzer::new(pitch_config).unwrap();
        let mut samples = [0.0_f32; 128];
        for (index, sample) in samples.iter_mut().enumerate() {
            *sample = (2.0 * PI * 6_000.0 * index as f64 / SAMPLE_RATE).sin() as f32;
        }
        analyzer
            .process_mono_masked(&samples, ScopeWork::OSCILLOSCOPE)
            .unwrap();
        let mut output = [OscilloscopeSample::default(); 32];
        assert_eq!(
            analyzer.copy_oscilloscope(
                &mut output,
                OscilloscopeMode::Pitch,
                OscilloscopeCycles::Single
            ),
            32
        );
        let low = output
            .iter()
            .map(|sample| sample.left)
            .fold(f32::INFINITY, f32::min);
        let high = output
            .iter()
            .map(|sample| sample.left)
            .fold(f32::NEG_INFINITY, f32::max);
        assert!(low < -0.8, "triggered trace lost negative half: {low}");
        assert!(high > 0.8, "triggered trace lost positive half: {high}");
        assert_eq!(analyzer.oscilloscope_frame_count(), 128);
    }

    #[test]
    fn reset_clears_linear_spectrum() {
        let mut analyzer = ScopeAnalyzer::new(config()).unwrap();
        let samples = [1.0_f32; 64];
        analyzer.process_mono(&samples).unwrap();
        assert!(analyzer.linear_spectrum().iter().any(|value| *value > 0.0));

        analyzer.reset();

        assert!(analyzer.linear_spectrum().iter().all(|value| *value == 0.0));
    }

    #[test]
    fn spectrogram_is_circular_and_keeps_newest_rows() {
        let mut analyzer = ScopeAnalyzer::new(config()).unwrap();
        let mut samples = [0.0_f32; 128];
        for (index, sample) in samples.iter_mut().enumerate() {
            *sample = (2.0 * PI * 6_000.0 * index as f64 / SAMPLE_RATE).sin() as f32;
        }
        analyzer.process_mono(&samples).unwrap();
        assert_eq!(analyzer.spectrum_frame_count(), 3);
        assert_eq!(analyzer.spectrogram_row_count(), 2);
        assert!(
            analyzer
                .spectrogram_row(0)
                .unwrap()
                .iter()
                .all(|level| level.is_finite())
        );
        assert!(analyzer.spectrogram_row(2).is_none());
    }

    #[test]
    fn non_finite_input_becomes_silence_without_poisoning_output() {
        let mut analyzer = ScopeAnalyzer::new(config()).unwrap();
        analyzer
            .process_stereo(&[f32::NAN, f32::INFINITY], &[f32::NEG_INFINITY, f32::NAN])
            .unwrap();
        let snapshot = analyzer.snapshot();
        assert_eq!(snapshot.correlation, None);
        assert_eq!(snapshot.left_rms_dbfs, None);
        assert_eq!(snapshot.right_rms_dbfs, None);
        let bucket = analyzer.waveform_bucket(0).unwrap();
        assert_eq!(bucket.left_min, 0.0);
        assert_eq!(bucket.right_max, 0.0);
    }

    #[test]
    fn reset_keeps_fixed_buffers_and_forgets_measurement() {
        let mut analyzer = ScopeAnalyzer::new(config()).unwrap();
        let waveform = analyzer.waveform.as_ptr();
        let goniometer = analyzer.goniometer.as_ptr();
        let oscilloscope = analyzer.oscilloscope.as_ptr();
        let spectrum = analyzer.spectrum.as_ptr();
        analyzer.process_mono(&[0.5; 64]).unwrap();
        assert!(analyzer.snapshot().spectrum_ready);
        analyzer.reset();
        assert_eq!(analyzer.waveform.as_ptr(), waveform);
        assert_eq!(analyzer.goniometer.as_ptr(), goniometer);
        assert_eq!(analyzer.oscilloscope.as_ptr(), oscilloscope);
        assert_eq!(analyzer.spectrum.as_ptr(), spectrum);
        assert_eq!(analyzer.snapshot().processed_frames, 0);
        assert_eq!(analyzer.waveform_bucket_count(), 0);
        assert_eq!(analyzer.spectrogram_row_count(), 0);
    }
}
