//! Standards-based loudness and peak measurement.
//!
//! This crate is deliberately independent from plugin formats, windows, and
//! renderers. It implements the measurement core shared by Neta's plugin and
//! future standalone shells.
//!
//! Loudness follows ITU-R BS.1770-5 and EBU R 128:
//!
//! - two-stage K-weighting;
//! - 400 ms momentary and 3 s short-term windows;
//! - 400 ms integrated blocks with 75% overlap;
//! - absolute (-70 LUFS) and relative (-10 LU) gates;
//! - EBU Tech 3342 loudness range;
//! - 4x true-peak interpolation using the Annex 2 FIR;
//! - sample peak and rolling RMS per channel.
//!
//! Construction and reset may touch allocated storage. [`LoudnessMeter::process_block`]
//! never allocates, locks, or performs I/O.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Realtime-safe stereo scope analysis.
pub mod scope;

use std::error::Error;
use std::f64::consts::PI;
use std::fmt;

const LOUDNESS_OFFSET: f64 = -0.691;
const ABSOLUTE_GATE_LUFS: f64 = -70.0;
const INTEGRATED_RELATIVE_GATE_LU: f64 = -10.0;
const LRA_RELATIVE_GATE_LU: f64 = -20.0;
const DEFAULT_HISTORY_SECONDS: usize = 12 * 60 * 60;
/// Lowest displayed loudness bin, matching the absolute R128 gate.
pub const LOUDNESS_HISTOGRAM_MIN_LUFS: f64 = ABSOLUTE_GATE_LUFS;
/// Loudness resolution of bounded histogram output, in LU.
///
/// 0.1 LU is an order of magnitude finer than the ±1 LU tolerance EBU Tech
/// 3341 allows, so binning never costs a compliance case.
pub const LOUDNESS_HISTOGRAM_BIN_LU: f64 = 0.1;
/// Loudest displayed histogram bin. Higher programme values share top bin.
pub const LOUDNESS_HISTOGRAM_MAX_LUFS: f64 = 10.0;

const HISTOGRAM_BIN_LU: f64 = LOUDNESS_HISTOGRAM_BIN_LU;
const HISTOGRAM_MAX_LUFS: f64 = LOUDNESS_HISTOGRAM_MAX_LUFS;

const TRUE_PEAK_PHASES: usize = 4;
const TRUE_PEAK_TAPS: usize = 12;

// ITU-R BS.1770-5 Annex 2, order-48, four-phase FIR interpolator.
const TRUE_PEAK_FIR: [[f64; TRUE_PEAK_TAPS]; TRUE_PEAK_PHASES] = [
    [
        0.001_708_984_375,
        0.010_986_328_125,
        -0.019_653_320_312_5,
        0.033_203_125,
        -0.059_448_242_187_5,
        0.137_329_101_562_5,
        0.972_167_968_75,
        -0.102_294_921_875,
        0.047_607_421_875,
        -0.026_611_328_125,
        0.014_892_578_125,
        -0.008_300_781_25,
    ],
    [
        -0.029_174_804_687_5,
        0.029_296_875,
        -0.051_757_812_5,
        0.089_111_328_125,
        -0.166_503_906_25,
        0.465_087_890_625,
        0.779_785_156_25,
        -0.200_317_382_812_5,
        0.101_562_5,
        -0.058_227_539_062_5,
        0.033_081_054_687_5,
        -0.018_920_898_437_5,
    ],
    [
        -0.018_920_898_437_5,
        0.033_081_054_687_5,
        -0.058_227_539_062_5,
        0.101_562_5,
        -0.200_317_382_812_5,
        0.779_785_156_25,
        0.465_087_890_625,
        -0.166_503_906_25,
        0.089_111_328_125,
        -0.051_757_812_5,
        0.029_296_875,
        -0.029_174_804_687_5,
    ],
    [
        -0.008_300_781_25,
        0.014_892_578_125,
        -0.026_611_328_125,
        0.047_607_421_875,
        -0.102_294_921_875,
        0.972_167_968_75,
        0.137_329_101_562_5,
        -0.059_448_242_187_5,
        0.033_203_125,
        -0.019_653_320_312_5,
        0.010_986_328_125,
        0.001_708_984_375,
    ],
];

/// A two-pole, two-zero filter section.
///
/// Direct form I keeps coefficients in the same convention used by
/// BS.1770 tables 1 and 2. `a0` is normalized to one.
#[derive(Clone, Copy, Debug, Default)]
pub struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    x: [f64; 2],
    y: [f64; 2],
}

impl Biquad {
    /// Creates a section from normalized coefficients.
    pub fn new(b: [f64; 3], a: [f64; 2]) -> Self {
        Self {
            b,
            a,
            x: [0.0; 2],
            y: [0.0; 2],
        }
    }

    /// Creates a section that passes input unchanged.
    pub fn passthrough() -> Self {
        Self::new([1.0, 0.0, 0.0], [0.0, 0.0])
    }

    /// Clears delay state.
    pub fn reset(&mut self) {
        self.x = [0.0; 2];
        self.y = [0.0; 2];
    }

    /// Filters one sample.
    #[inline]
    pub fn process(&mut self, input: f64) -> f64 {
        let output = self.b[0] * input + self.b[1] * self.x[0] + self.b[2] * self.x[1]
            - self.a[0] * self.y[0]
            - self.a[1] * self.y[1];
        self.x = [input, self.x[0]];
        self.y = [output, self.y[0]];
        output
    }

    /// Returns DC gain implied by coefficients.
    pub fn dc_gain(&self) -> f64 {
        (self.b[0] + self.b[1] + self.b[2]) / (1.0 + self.a[0] + self.a[1])
    }

    #[cfg(test)]
    fn coefficients(&self) -> ([f64; 3], [f64; 2]) {
        (self.b, self.a)
    }
}

/// Two-stage K-weighting filter for one channel.
#[derive(Clone, Copy, Debug)]
pub struct KWeighting {
    shelf: Biquad,
    high_pass: Biquad,
}

impl KWeighting {
    /// Builds coefficients for `sample_rate`.
    ///
    /// At 48 kHz, exact tabulated BS.1770 coefficients are used. Other sample
    /// rates use the equivalent De Man parameterization and bilinear transform.
    pub fn new(sample_rate: f64) -> Result<Self, ConfigError> {
        validate_sample_rate(sample_rate)?;

        if (sample_rate - 48_000.0).abs() < f64::EPSILON {
            return Ok(Self {
                shelf: Biquad::new(
                    [
                        1.535_124_859_586_97,
                        -2.691_696_189_406_38,
                        1.198_392_810_852_85,
                    ],
                    [-1.690_659_293_182_41, 0.732_480_774_215_85],
                ),
                high_pass: Biquad::new(
                    [1.0, -2.0, 1.0],
                    [-1.990_047_454_833_98, 0.990_072_250_366_21],
                ),
            });
        }

        let shelf_frequency = 1_681.974_450_955_533;
        let shelf_gain_db = 3.999_843_853_973_347;
        let shelf_q = 0.707_175_236_955_419_6;
        let k = (PI * shelf_frequency / sample_rate).tan();
        let vh = 10.0_f64.powf(shelf_gain_db / 20.0);
        let vb = vh.powf(0.499_666_774_154_541_6);
        let a0 = 1.0 + k / shelf_q + k * k;
        let shelf = Biquad::new(
            [
                (vh + vb * k / shelf_q + k * k) / a0,
                2.0 * (k * k - vh) / a0,
                (vh - vb * k / shelf_q + k * k) / a0,
            ],
            [2.0 * (k * k - 1.0) / a0, (1.0 - k / shelf_q + k * k) / a0],
        );

        let high_pass_frequency = 38.135_470_876_024_44;
        let high_pass_q = 0.500_327_037_323_877_3;
        let k = (PI * high_pass_frequency / sample_rate).tan();
        let a0 = 1.0 + k / high_pass_q + k * k;
        let high_pass = Biquad::new(
            [1.0 / a0, -2.0 / a0, 1.0 / a0],
            [
                2.0 * (k * k - 1.0) / a0,
                (1.0 - k / high_pass_q + k * k) / a0,
            ],
        );

        Ok(Self { shelf, high_pass })
    }

    /// Filters one sample.
    #[inline]
    pub fn process(&mut self, sample: f64) -> f64 {
        self.high_pass.process(self.shelf.process(sample))
    }

    /// Clears both filter stages.
    pub fn reset(&mut self) {
        self.shelf.reset();
        self.high_pass.reset();
    }
}

/// Meter construction failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigError {
    /// Sample rate is non-finite, non-positive, or too low for this meter.
    InvalidSampleRate,
    /// No channels were configured.
    NoChannels,
    /// A channel weight is non-finite or negative.
    InvalidChannelWeight,
    /// Maximum block size is zero.
    InvalidBlockSize,
    /// Requested history duration is zero or cannot be represented.
    InvalidHistoryDuration,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidSampleRate => "sample rate must be finite and at least 8 kHz",
            Self::NoChannels => "at least one channel is required",
            Self::InvalidChannelWeight => "channel weights must be finite and non-negative",
            Self::InvalidBlockSize => "maximum block size must be greater than zero",
            Self::InvalidHistoryDuration => {
                "history duration must be positive and fit addressable memory"
            }
        };
        formatter.write_str(message)
    }
}

impl Error for ConfigError {}

/// Audio block rejected by the configured meter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessError {
    /// Input channel count differs from configured channel count.
    ChannelCount {
        /// Expected channel count.
        expected: usize,
        /// Received channel count.
        actual: usize,
    },
    /// Input block exceeds maximum size declared at construction.
    BlockTooLarge {
        /// Configured maximum.
        maximum: usize,
        /// Received frame count.
        actual: usize,
    },
    /// Channel slices do not all contain the same number of frames.
    UnevenChannels,
}

impl fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChannelCount { expected, actual } => {
                write!(formatter, "expected {expected} channels, received {actual}")
            }
            Self::BlockTooLarge { maximum, actual } => {
                write!(
                    formatter,
                    "block has {actual} frames; configured maximum is {maximum}"
                )
            }
            Self::UnevenChannels => {
                formatter.write_str("all channel slices must have equal length")
            }
        }
    }
}

impl Error for ProcessError {}

#[derive(Clone, Debug)]
struct TruePeakFilter {
    history: [f64; TRUE_PEAK_TAPS],
    cursor: usize,
}

impl Default for TruePeakFilter {
    fn default() -> Self {
        Self {
            history: [0.0; TRUE_PEAK_TAPS],
            cursor: 0,
        }
    }
}

impl TruePeakFilter {
    #[inline]
    fn process(&mut self, sample: f64) -> f64 {
        self.history[self.cursor] = sample;
        self.cursor = (self.cursor + 1) % TRUE_PEAK_TAPS;

        let mut peak = sample.abs();
        for phase in TRUE_PEAK_FIR {
            let mut value = 0.0;
            for (tap, coefficient) in phase.into_iter().enumerate() {
                let index = (self.cursor + TRUE_PEAK_TAPS - 1 - tap) % TRUE_PEAK_TAPS;
                value += coefficient * self.history[index];
            }
            peak = peak.max(value.abs());
        }
        peak
    }

    fn reset(&mut self) {
        self.history.fill(0.0);
        self.cursor = 0;
    }
}

#[derive(Debug)]
struct ChannelState {
    weight: f64,
    weighting: KWeighting,
    true_peak_filter: TruePeakFilter,
    raw_square_ring: Box<[f64]>,
    raw_square_cursor: usize,
    raw_square_count: usize,
    raw_square_sum: f64,
    sample_peak: f64,
    true_peak: f64,
}

impl ChannelState {
    fn new(weight: f64, sample_rate: f64, momentary_frames: usize) -> Result<Self, ConfigError> {
        Ok(Self {
            weight,
            weighting: KWeighting::new(sample_rate)?,
            true_peak_filter: TruePeakFilter::default(),
            raw_square_ring: vec![0.0; momentary_frames].into_boxed_slice(),
            raw_square_cursor: 0,
            raw_square_count: 0,
            raw_square_sum: 0.0,
            sample_peak: 0.0,
            true_peak: 0.0,
        })
    }

    fn reset(&mut self) {
        self.weighting.reset();
        self.true_peak_filter.reset();
        self.raw_square_ring.fill(0.0);
        self.raw_square_cursor = 0;
        self.raw_square_count = 0;
        self.raw_square_sum = 0.0;
        self.sample_peak = 0.0;
        self.true_peak = 0.0;
    }

    fn process(&mut self, samples: &[f32], weighted_energy: &mut [f64]) {
        for (sample, aggregate) in samples.iter().zip(weighted_energy) {
            // Hosts occasionally emit a bad sample while devices reconnect.
            // Silence it locally; one NaN must not poison sums, filters, or
            // programme history for rest of a monitoring session.
            let sample = if sample.is_finite() {
                f64::from(*sample)
            } else {
                0.0
            };
            let magnitude = sample.abs();
            self.sample_peak = self.sample_peak.max(magnitude);
            self.true_peak = self.true_peak.max(self.true_peak_filter.process(sample));

            let square = sample * sample;
            let leaving = self.raw_square_ring[self.raw_square_cursor];
            self.raw_square_ring[self.raw_square_cursor] = square;
            self.raw_square_cursor = (self.raw_square_cursor + 1) % self.raw_square_ring.len();
            self.raw_square_sum += square - leaving;
            self.raw_square_count = (self.raw_square_count + 1).min(self.raw_square_ring.len());

            let filtered = self.weighting.process(sample);
            *aggregate += self.weight * filtered * filtered;
        }
    }
}

/// Current programme-level measurements.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeterSnapshot {
    /// Loudness over the latest complete 400 ms window.
    pub momentary_lufs: Option<f64>,
    /// Highest 400 ms loudness since reset.
    pub max_momentary_lufs: Option<f64>,
    /// Loudness over the latest complete 3 s window.
    pub short_term_lufs: Option<f64>,
    /// Highest 3 s loudness since reset.
    pub max_short_term_lufs: Option<f64>,
    /// Start-to-current integrated loudness with both R128 gates.
    pub integrated_lufs: Option<f64>,
    /// EBU Tech 3342 loudness range.
    pub loudness_range_lu: Option<f64>,
    /// Highest true peak since reset.
    pub max_true_peak_dbfs: Option<f64>,
    /// Peak-to-short-term loudness ratio.
    pub psr_db: Option<f64>,
    /// Peak-to-integrated loudness ratio.
    pub plr_db: Option<f64>,
    /// Processed programme duration.
    pub duration_seconds: f64,
    /// Whether LRA has reached EBU's 60-second stability indication point.
    pub lra_stable: bool,
    /// Whether fixed history storage still contains the complete programme.
    pub history_complete: bool,
}

/// Constant-time programme values safe to read from an audio callback.
///
/// Unlike [`MeterSnapshot`], this never scans or sorts history: gated values
/// come from a fixed-size loudness histogram, so the cost is the same for a
/// four-minute track and an eight-hour session.
///
/// The price is resolution. Gate decisions are quantized to
/// [`HISTOGRAM_BIN_LU`], so these agree with [`LoudnessMeter::snapshot`] to
/// within a bin rather than exactly — an order of magnitude inside EBU's
/// tolerance, and `snapshot` remains the reference for delivery paperwork.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RealtimeMeterSnapshot {
    /// Loudness over the latest complete 400 ms window.
    pub momentary_lufs: Option<f64>,
    /// Loudness over the latest complete 3 s window.
    pub short_term_lufs: Option<f64>,
    /// Highest true peak since reset.
    pub max_true_peak_dbfs: Option<f64>,
    /// Peak-to-short-term loudness ratio.
    pub psr_db: Option<f64>,
    /// Start-to-current integrated loudness with both R128 gates, from the
    /// binned distribution rather than a history scan.
    pub integrated_lufs: Option<f64>,
    /// EBU Tech 3342 loudness range, from the same binned distribution.
    pub loudness_range_lu: Option<f64>,
    /// Peak-to-integrated loudness ratio.
    pub plr_db: Option<f64>,
    /// Number of programme frames processed since reset.
    pub processed_frames: u64,
}

/// Programme series used for bounded loudness histogram output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoudnessHistogramKind {
    /// 400 ms blocks sampled every 100 ms, used for integrated loudness.
    Integrated,
    /// 3 s blocks sampled every 100 ms, used for loudness range.
    ShortTerm,
}

/// One resampled bounded loudness histogram bucket.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LoudnessHistogramBin {
    /// Inclusive low edge of this display bucket, in LUFS.
    pub low_lufs: f64,
    /// Exclusive high edge of this display bucket, in LUFS.
    pub high_lufs: f64,
    /// Number of source blocks represented by this bucket.
    pub count: u64,
    /// Energy-derived mean loudness of represented source blocks.
    ///
    /// `None` means no programme block fell in this display bucket.
    pub mean_lufs: Option<f64>,
}

/// Current per-channel measurements.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChannelMetrics {
    /// Highest sample magnitude since reset, in dBFS.
    pub sample_peak_dbfs: Option<f64>,
    /// Highest 4x-oversampled true peak since reset, in dBTP.
    pub true_peak_dbfs: Option<f64>,
    /// RMS over the latest window, up to 400 ms while starting.
    pub rms_dbfs: Option<f64>,
}

const VU_STANDARD_RESPONSE_SECONDS: f64 = 0.3;
const VU_DEFAULT_ATTACK_SECONDS: f64 = 0.03;
const VU_DEFAULT_RELEASE_SECONDS: f64 = 0.3;
const VU_DEFAULT_PEAK_HOLD_SECONDS: f64 = 1.5;
const VU_DEFAULT_CALIBRATION_DBFS: f64 = -18.0;

/// Detector presented by [`VuMeter`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VuMode {
    /// Standard 300 ms VU-like RMS response.
    #[default]
    Vu,
    /// RMS detector with configurable attack and release.
    Rms,
    /// Absolute sample peak detector with configurable attack and release.
    Peak,
}

/// Fixed settings for a stereo [`VuMeter`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VuConfig {
    /// Audio sample rate in Hz.
    pub sample_rate: f64,
    /// Largest planar input block accepted by [`VuMeter::process_stereo`].
    pub max_block_frames: usize,
    /// Detector presented by the meter.
    pub mode: VuMode,
    /// dBFS value that should display as 0 calibrated VU/dB.
    ///
    /// For example, `-18.0` makes a `-18 dBFS` detector level report `0.0`
    /// through [`VuChannelSnapshot::calibrated_db`].
    pub calibration_dbfs: f64,
    /// Rise time constant for RMS and peak modes, in seconds.
    ///
    /// Zero makes rises instantaneous. VU mode always uses its standard
    /// 300 ms response rather than this setting.
    pub attack_seconds: f64,
    /// Fall time constant for RMS and peak modes, in seconds.
    ///
    /// Zero makes falls instantaneous. VU mode always uses its standard
    /// 300 ms response rather than this setting.
    pub release_seconds: f64,
    /// Time a display peak remains pinned before it starts falling, in seconds.
    pub peak_hold_seconds: f64,
}

impl VuConfig {
    /// Returns practical stereo settings with -18 dBFS calibration.
    pub fn recommended(sample_rate: f64, max_block_frames: usize) -> Result<Self, VuConfigError> {
        let config = Self {
            sample_rate,
            max_block_frames,
            mode: VuMode::Vu,
            calibration_dbfs: VU_DEFAULT_CALIBRATION_DBFS,
            attack_seconds: VU_DEFAULT_ATTACK_SECONDS,
            release_seconds: VU_DEFAULT_RELEASE_SECONDS,
            peak_hold_seconds: VU_DEFAULT_PEAK_HOLD_SECONDS,
        };
        validate_vu_config(config)?;
        Ok(config)
    }
}

impl Default for VuConfig {
    fn default() -> Self {
        // These fixed values are valid by construction.
        Self::recommended(48_000.0, 1_024).expect("valid default VU configuration")
    }
}

/// Invalid [`VuConfig`] value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VuConfigError {
    /// Sample rate is non-finite or below 8 kHz.
    InvalidSampleRate,
    /// Maximum input block size is zero.
    InvalidBlockSize,
    /// Calibration reference is not finite.
    InvalidCalibration,
    /// Attack or release is non-finite or negative.
    InvalidBallistics,
    /// Peak hold duration is non-finite, negative, or cannot fit a frame count.
    InvalidPeakHold,
}

impl fmt::Display for VuConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidSampleRate => "sample rate must be finite and at least 8 kHz",
            Self::InvalidBlockSize => "maximum block size must be greater than zero",
            Self::InvalidCalibration => "VU calibration must be finite",
            Self::InvalidBallistics => "VU attack and release must be finite and non-negative",
            Self::InvalidPeakHold => "VU peak hold must be finite, non-negative, and representable",
        };
        formatter.write_str(message)
    }
}

impl Error for VuConfigError {}

/// Input block rejected by [`VuMeter`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VuProcessError {
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

impl fmt::Display for VuProcessError {
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

impl Error for VuProcessError {}

/// Current display values for one [`VuMeter`] channel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VuChannelSnapshot {
    /// Ballistic detector level in dBFS.
    pub level_dbfs: Option<f64>,
    /// Ballistic detector level relative to configured calibration.
    pub calibrated_db: Option<f64>,
    /// Retained peak indicator in dBFS.
    pub peak_hold_dbfs: Option<f64>,
    /// Retained peak indicator relative to configured calibration.
    pub peak_hold_calibrated_db: Option<f64>,
}

/// Current bounded stereo [`VuMeter`] state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VuSnapshot {
    /// Active detector mode.
    pub mode: VuMode,
    /// Left channel display values.
    pub left: VuChannelSnapshot,
    /// Right channel display values.
    pub right: VuChannelSnapshot,
    /// Number of stereo frames accepted since construction or reset.
    pub processed_frames: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct VuChannel {
    detector: f64,
    peak_hold: f64,
    peak_hold_frames_remaining: u64,
}

impl VuChannel {
    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Realtime-safe stereo VU/RMS/peak meter.
///
/// Construction computes all rate-derived coefficients. Processing has no
/// allocation, locking, formatting, or I/O. It observes audio only and never
/// changes input samples.
#[derive(Clone, Debug)]
pub struct VuMeter {
    config: VuConfig,
    attack_coefficient: f64,
    release_coefficient: f64,
    peak_release_coefficient: f64,
    peak_hold_frames: u64,
    left: VuChannel,
    right: VuChannel,
    processed_frames: u64,
}

impl VuMeter {
    /// Creates a meter from explicit fixed settings.
    pub fn new(config: VuConfig) -> Result<Self, VuConfigError> {
        validate_vu_config(config)?;
        let mut meter = Self {
            config,
            attack_coefficient: 0.0,
            release_coefficient: 0.0,
            peak_release_coefficient: 0.0,
            peak_hold_frames: 0,
            left: VuChannel::default(),
            right: VuChannel::default(),
            processed_frames: 0,
        };
        meter.update_rate_constants();
        Ok(meter)
    }

    /// Creates a meter with practical stereo defaults.
    pub fn stereo(sample_rate: f64, max_block_frames: usize) -> Result<Self, VuConfigError> {
        Self::new(VuConfig::recommended(sample_rate, max_block_frames)?)
    }

    /// Returns current fixed settings.
    pub fn config(&self) -> VuConfig {
        self.config
    }

    /// Changes detector mode and clears incompatible detector state.
    pub fn set_mode(&mut self, mode: VuMode) {
        if self.config.mode != mode {
            self.config.mode = mode;
            self.update_rate_constants();
            self.left.reset();
            self.right.reset();
        }
    }

    /// Changes 0 VU/dBFS display reference without reallocating.
    pub fn set_calibration_dbfs(&mut self, calibration_dbfs: f64) -> Result<(), VuConfigError> {
        if !calibration_dbfs.is_finite() {
            return Err(VuConfigError::InvalidCalibration);
        }
        self.config.calibration_dbfs = calibration_dbfs;
        Ok(())
    }

    /// Changes RMS/peak detector attack and release without reallocating.
    pub fn set_ballistics(
        &mut self,
        attack_seconds: f64,
        release_seconds: f64,
    ) -> Result<(), VuConfigError> {
        validate_vu_ballistics(attack_seconds, release_seconds)?;
        self.config.attack_seconds = attack_seconds;
        self.config.release_seconds = release_seconds;
        self.update_rate_constants();
        Ok(())
    }

    /// Changes peak indicator hold duration without reallocating.
    pub fn set_peak_hold_seconds(&mut self, peak_hold_seconds: f64) -> Result<(), VuConfigError> {
        self.peak_hold_frames = vu_hold_frames(self.config.sample_rate, peak_hold_seconds)?;
        self.config.peak_hold_seconds = peak_hold_seconds;
        Ok(())
    }

    /// Processes one bounded planar stereo block without allocation.
    pub fn process_stereo(&mut self, left: &[f32], right: &[f32]) -> Result<(), VuProcessError> {
        if left.len() != right.len() {
            return Err(VuProcessError::UnevenChannels);
        }
        if left.len() > self.config.max_block_frames {
            return Err(VuProcessError::BlockTooLarge {
                maximum: self.config.max_block_frames,
                actual: left.len(),
            });
        }
        let mode = self.config.mode;
        let attack_coefficient = self.attack_coefficient;
        let release_coefficient = self.release_coefficient;
        let peak_release_coefficient = self.peak_release_coefficient;
        let peak_hold_frames = self.peak_hold_frames;
        for (&left, &right) in left.iter().zip(right) {
            let left = if left.is_finite() {
                f64::from(left)
            } else {
                0.0
            };
            let right = if right.is_finite() {
                f64::from(right)
            } else {
                0.0
            };
            Self::process_channel_sample(
                &mut self.left,
                left,
                mode,
                attack_coefficient,
                release_coefficient,
                peak_release_coefficient,
                peak_hold_frames,
            );
            Self::process_channel_sample(
                &mut self.right,
                right,
                mode,
                attack_coefficient,
                release_coefficient,
                peak_release_coefficient,
                peak_hold_frames,
            );
        }
        self.processed_frames = self.processed_frames.saturating_add(left.len() as u64);
        Ok(())
    }

    /// Clears detector and peak-hold state without changing settings.
    pub fn reset(&mut self) {
        self.left.reset();
        self.right.reset();
        self.processed_frames = 0;
    }

    /// Returns latest two-channel levels in constant time.
    pub fn snapshot(&self) -> VuSnapshot {
        VuSnapshot {
            mode: self.config.mode,
            left: self.channel_snapshot(self.left),
            right: self.channel_snapshot(self.right),
            processed_frames: self.processed_frames,
        }
    }

    fn update_rate_constants(&mut self) {
        let (attack_seconds, release_seconds) = match self.config.mode {
            VuMode::Vu => (VU_STANDARD_RESPONSE_SECONDS, VU_STANDARD_RESPONSE_SECONDS),
            VuMode::Rms | VuMode::Peak => (self.config.attack_seconds, self.config.release_seconds),
        };
        self.attack_coefficient =
            sample_ballistics_coefficient(attack_seconds, self.config.sample_rate);
        self.release_coefficient =
            sample_ballistics_coefficient(release_seconds, self.config.sample_rate);
        self.peak_release_coefficient = self.release_coefficient;
        self.peak_hold_frames =
            vu_hold_frames(self.config.sample_rate, self.config.peak_hold_seconds)
                .expect("validated VU peak hold");
    }

    fn process_channel_sample(
        channel: &mut VuChannel,
        sample: f64,
        mode: VuMode,
        attack_coefficient: f64,
        release_coefficient: f64,
        peak_release_coefficient: f64,
        peak_hold_frames: u64,
    ) {
        let target = match mode {
            VuMode::Vu | VuMode::Rms => sample * sample,
            VuMode::Peak => sample.abs(),
        };
        let coefficient = if target > channel.detector {
            attack_coefficient
        } else {
            release_coefficient
        };
        channel.detector = target + (channel.detector - target) * coefficient;
        let amplitude = Self::detector_amplitude(mode, channel.detector);
        let held = Self::detector_amplitude(mode, channel.peak_hold);
        if amplitude >= held {
            channel.peak_hold = channel.detector;
            channel.peak_hold_frames_remaining = peak_hold_frames;
        } else if channel.peak_hold_frames_remaining > 0 {
            channel.peak_hold_frames_remaining -= 1;
        } else {
            channel.peak_hold = target + (channel.peak_hold - target) * peak_release_coefficient;
        }
    }

    fn detector_amplitude(mode: VuMode, detector: f64) -> f64 {
        match mode {
            VuMode::Vu | VuMode::Rms => detector.max(0.0).sqrt(),
            VuMode::Peak => detector.max(0.0),
        }
    }

    fn channel_snapshot(&self, channel: VuChannel) -> VuChannelSnapshot {
        let level_dbfs =
            amplitude_to_db(Self::detector_amplitude(self.config.mode, channel.detector));
        let peak_hold_dbfs = amplitude_to_db(Self::detector_amplitude(
            self.config.mode,
            channel.peak_hold,
        ));
        VuChannelSnapshot {
            level_dbfs,
            calibrated_db: level_dbfs.map(|level| level - self.config.calibration_dbfs),
            peak_hold_dbfs,
            peak_hold_calibrated_db: peak_hold_dbfs
                .map(|level| level - self.config.calibration_dbfs),
        }
    }
}

/// One loudness bin: how many blocks landed in it, and their exact energy.
///
/// The energy is summed rather than reconstructed from the bin, so gated
/// averages stay exact for every block the gate keeps. Only the gate
/// *decision* is quantized.
#[derive(Clone, Copy, Debug, Default)]
struct HistogramBin {
    count: u64,
    energy: f64,
}

/// Block loudness distribution with running absolute-gate totals.
///
/// R128 gating needs two passes over the programme: one to find the relative
/// threshold, one to average whatever survives it. Keeping every block and
/// rescanning is `O(n)` in programme length and grows without bound — which
/// is why [`LoudnessMeter::snapshot`] is a UI-thread call. Binning by
/// loudness makes both passes `O(bins)`, and a constant is what makes
/// integrated loudness and LRA safe to read from an audio callback.
///
/// Blocks under the absolute gate are dropped on arrival: the gate would
/// discard them anyway, and not storing them keeps the bin range small.
#[derive(Debug)]
struct LoudnessHistogram {
    bins: Vec<HistogramBin>,
    absolute_sum: f64,
    absolute_count: u64,
}

impl LoudnessHistogram {
    fn new() -> Self {
        let span = HISTOGRAM_MAX_LUFS - ABSOLUTE_GATE_LUFS;
        let bin_count = (span / HISTOGRAM_BIN_LU).ceil() as usize + 1;
        Self {
            bins: vec![HistogramBin::default(); bin_count],
            absolute_sum: 0.0,
            absolute_count: 0,
        }
    }

    fn clear(&mut self) {
        self.bins.fill(HistogramBin::default());
        self.absolute_sum = 0.0;
        self.absolute_count = 0;
    }

    /// Loudness at the centre of `index`.
    fn bin_loudness(&self, index: usize) -> f64 {
        ABSOLUTE_GATE_LUFS + (index as f64 + 0.5) * HISTOGRAM_BIN_LU
    }

    /// Files one block. Never allocates: the bins are sized at construction.
    fn push(&mut self, energy: f64) {
        let loudness = loudness_from_energy(energy);
        // Written as a positive guard so a NaN is dropped rather than binned:
        // the comparison is false either way, and the strictness matches the
        // exact path, so both agree about a block sitting on the gate.
        if loudness > ABSOLUTE_GATE_LUFS {
            self.absolute_sum += energy;
            self.absolute_count += 1;

            let offset = (loudness - ABSOLUTE_GATE_LUFS) / HISTOGRAM_BIN_LU;
            let index = (offset as usize).min(self.bins.len() - 1);
            self.bins[index].count += 1;
            self.bins[index].energy += energy;
        }
    }

    /// Mean loudness of everything above the absolute gate, which is what
    /// both relative gates are measured from.
    fn absolute_gated_loudness(&self) -> Option<f64> {
        (self.absolute_count > 0)
            .then(|| loudness_from_energy(self.absolute_sum / self.absolute_count as f64))
    }

    /// Integrated loudness: the second gate applied to the distribution.
    fn integrated(&self) -> Option<f64> {
        let relative_gate = self.absolute_gated_loudness()? + INTEGRATED_RELATIVE_GATE_LU;
        let mut sum = 0.0;
        let mut count = 0_u64;
        for (index, bin) in self.bins.iter().enumerate() {
            if bin.count > 0 && self.bin_loudness(index) > relative_gate {
                sum += bin.energy;
                count += bin.count;
            }
        }
        (count > 0).then(|| loudness_from_energy(sum / count as f64))
    }

    /// Loudness range: the spread between the 10th and 95th percentile of
    /// the blocks above the LRA gate (EBU Tech 3342).
    fn range(&self) -> Option<f64> {
        let relative_gate = self.absolute_gated_loudness()? + LRA_RELATIVE_GATE_LU;
        let mut total = 0_u64;
        for (index, bin) in self.bins.iter().enumerate() {
            if self.bin_loudness(index) > relative_gate {
                total += bin.count;
            }
        }
        if total == 0 {
            return None;
        }
        // Same rank convention as the exact path's `percentile`.
        let rank = |probability: f64| ((total - 1) as f64 * probability).round() as u64;
        let low_rank = rank(0.10);
        let high_rank = rank(0.95);

        let mut seen = 0_u64;
        let mut low = None;
        let mut high = None;
        for (index, bin) in self.bins.iter().enumerate() {
            if bin.count == 0 || self.bin_loudness(index) <= relative_gate {
                continue;
            }
            let next = seen + bin.count;
            if low.is_none() && next > low_rank {
                low = Some(self.bin_loudness(index));
            }
            if high.is_none() && next > high_rank {
                high = Some(self.bin_loudness(index));
            }
            seen = next;
        }
        high.zip(low).map(|(high, low)| high - low)
    }

    fn copy_resampled(&self, destination: &mut [LoudnessHistogramBin]) -> usize {
        let amount = destination.len().min(self.bins.len());
        if amount == 0 {
            return 0;
        }
        for (destination_index, output) in destination[..amount].iter_mut().enumerate() {
            let first = destination_index * self.bins.len() / amount;
            let end = ((destination_index + 1) * self.bins.len() / amount).max(first + 1);
            let mut count = 0_u64;
            let mut energy = 0.0_f64;
            for bin in &self.bins[first..end] {
                count = count.saturating_add(bin.count);
                energy += bin.energy;
            }
            *output = LoudnessHistogramBin {
                low_lufs: ABSOLUTE_GATE_LUFS + first as f64 * HISTOGRAM_BIN_LU,
                high_lufs: ABSOLUTE_GATE_LUFS + end as f64 * HISTOGRAM_BIN_LU,
                count,
                mean_lufs: (count > 0).then(|| loudness_from_energy(energy / count as f64)),
            };
        }
        amount
    }
}

/// Realtime-safe BS.1770/EBU R128 measurement engine.
#[derive(Debug)]
pub struct LoudnessMeter {
    sample_rate: f64,
    max_block_frames: usize,
    channels: Vec<ChannelState>,
    weighted_energy: Box<[f64]>,
    energy_ring: Box<[f64]>,
    energy_cursor: usize,
    energy_count: usize,
    momentary_frames: usize,
    short_term_frames: usize,
    step_frames: usize,
    momentary_sum: f64,
    short_term_sum: f64,
    max_momentary_energy: f64,
    max_short_term_energy: f64,
    samples_processed: u64,
    integrated_blocks: Vec<f64>,
    short_term_blocks: Vec<f64>,
    percentile_scratch: Vec<f64>,
    /// The same integrated blocks, binned, so gating is `O(bins)` instead of
    /// `O(programme)` and can therefore run on the audio thread.
    integrated_histogram: LoudnessHistogram,
    /// The short-term series, binned, for LRA on the same terms.
    short_term_histogram: LoudnessHistogram,
    max_history_blocks: usize,
    history_complete: bool,
}

impl LoudnessMeter {
    /// Creates a meter with twelve hours of fixed history storage.
    pub fn new(
        sample_rate: f64,
        channel_weights: &[f64],
        max_block_frames: usize,
    ) -> Result<Self, ConfigError> {
        Self::with_history(
            sample_rate,
            channel_weights,
            max_block_frames,
            DEFAULT_HISTORY_SECONDS,
        )
    }

    /// Creates a meter with an explicit maximum history duration.
    ///
    /// History is allocated here so the audio callback never grows a vector.
    /// If a programme exceeds this duration, live windows continue but
    /// `history_complete` becomes false and new integrated/LRA blocks stop.
    pub fn with_history(
        sample_rate: f64,
        channel_weights: &[f64],
        max_block_frames: usize,
        max_history_seconds: usize,
    ) -> Result<Self, ConfigError> {
        validate_sample_rate(sample_rate)?;
        if channel_weights.is_empty() {
            return Err(ConfigError::NoChannels);
        }
        if channel_weights
            .iter()
            .any(|weight| !weight.is_finite() || *weight < 0.0)
        {
            return Err(ConfigError::InvalidChannelWeight);
        }
        if max_block_frames == 0 {
            return Err(ConfigError::InvalidBlockSize);
        }
        if max_history_seconds == 0 {
            return Err(ConfigError::InvalidHistoryDuration);
        }

        let momentary_frames = seconds_to_frames(sample_rate, 0.4)?;
        let short_term_frames = seconds_to_frames(sample_rate, 3.0)?;
        let step_frames = seconds_to_frames(sample_rate, 0.1)?;
        let max_history_blocks = max_history_seconds
            .checked_mul(10)
            .and_then(|blocks| blocks.checked_add(1))
            .ok_or(ConfigError::InvalidHistoryDuration)?;

        let channels = channel_weights
            .iter()
            .map(|weight| ChannelState::new(*weight, sample_rate, momentary_frames))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            sample_rate,
            max_block_frames,
            channels,
            weighted_energy: vec![0.0; max_block_frames].into_boxed_slice(),
            energy_ring: vec![0.0; short_term_frames].into_boxed_slice(),
            energy_cursor: 0,
            energy_count: 0,
            momentary_frames,
            short_term_frames,
            step_frames,
            momentary_sum: 0.0,
            short_term_sum: 0.0,
            max_momentary_energy: 0.0,
            max_short_term_energy: 0.0,
            samples_processed: 0,
            integrated_blocks: Vec::with_capacity(max_history_blocks),
            short_term_blocks: Vec::with_capacity(max_history_blocks),
            percentile_scratch: Vec::with_capacity(max_history_blocks),
            integrated_histogram: LoudnessHistogram::new(),
            short_term_histogram: LoudnessHistogram::new(),
            max_history_blocks,
            history_complete: true,
        })
    }

    /// Creates a stereo meter with standard left/right weights.
    pub fn stereo(sample_rate: f64, max_block_frames: usize) -> Result<Self, ConfigError> {
        Self::new(sample_rate, &[1.0, 1.0], max_block_frames)
    }

    /// Processes one planar block without allocation.
    ///
    /// All slices must have equal length and remain within
    /// `max_block_frames`.
    pub fn process_block<'a, I>(&mut self, channels: I) -> Result<(), ProcessError>
    where
        I: ExactSizeIterator<Item = &'a [f32]> + Clone,
    {
        let expected_channels = self.channels.len();
        if channels.len() != expected_channels {
            return Err(ProcessError::ChannelCount {
                expected: expected_channels,
                actual: channels.len(),
            });
        }

        let mut validator = channels.clone();
        let Some(first) = validator.next() else {
            return Err(ProcessError::ChannelCount {
                expected: expected_channels,
                actual: 0,
            });
        };
        let frames = first.len();
        if frames > self.max_block_frames {
            return Err(ProcessError::BlockTooLarge {
                maximum: self.max_block_frames,
                actual: frames,
            });
        }
        if validator.any(|samples| samples.len() != frames) {
            return Err(ProcessError::UnevenChannels);
        }

        let mut channels = channels;
        let first = channels.next().expect("validated nonempty channels");
        let energy = &mut self.weighted_energy[..frames];
        energy.fill(0.0);
        self.channels[0].process(first, energy);

        for (index, samples) in channels.enumerate() {
            self.channels[index + 1].process(samples, energy);
        }

        for index in 0..frames {
            self.push_energy(self.weighted_energy[index]);
        }
        Ok(())
    }

    /// Clears filters, windows, peaks, and programme history without freeing
    /// preallocated storage.
    pub fn reset(&mut self) {
        for channel in &mut self.channels {
            channel.reset();
        }
        self.weighted_energy.fill(0.0);
        self.energy_ring.fill(0.0);
        self.energy_cursor = 0;
        self.energy_count = 0;
        self.momentary_sum = 0.0;
        self.short_term_sum = 0.0;
        self.max_momentary_energy = 0.0;
        self.max_short_term_energy = 0.0;
        self.samples_processed = 0;
        self.integrated_blocks.clear();
        self.short_term_blocks.clear();
        self.percentile_scratch.clear();
        self.integrated_histogram.clear();
        self.short_term_histogram.clear();
        self.history_complete = true;
    }

    /// Returns current 400 ms/3 s/true-peak values in constant time.
    ///
    /// This method does not allocate, lock, sort, format, or inspect history,
    /// so a plugin may publish it through a bounded visualization channel from
    /// `process_block`. It intentionally omits integrated loudness and LRA.
    pub fn realtime_snapshot(&self) -> RealtimeMeterSnapshot {
        let momentary_lufs = (self.energy_count >= self.momentary_frames)
            .then(|| loudness_from_energy(self.momentary_sum / self.momentary_frames as f64));
        let short_term_lufs = (self.energy_count >= self.short_term_frames)
            .then(|| loudness_from_energy(self.short_term_sum / self.short_term_frames as f64));
        let max_true_peak = self
            .channels
            .iter()
            .map(|channel| channel.true_peak)
            .fold(0.0, f64::max);
        let max_true_peak_dbfs = amplitude_to_db(max_true_peak);
        let integrated_lufs = self.integrated_histogram.integrated();
        RealtimeMeterSnapshot {
            momentary_lufs,
            short_term_lufs,
            max_true_peak_dbfs,
            psr_db: max_true_peak_dbfs
                .zip(short_term_lufs)
                .map(|(peak, loudness)| peak - loudness),
            integrated_lufs,
            loudness_range_lu: self.short_term_histogram.range(),
            plr_db: max_true_peak_dbfs
                .zip(integrated_lufs)
                .map(|(peak, loudness)| peak - loudness),
            processed_frames: self.samples_processed,
        }
    }

    /// Computes current programme measurements.
    ///
    /// This scans programme history and sorts preallocated scratch storage for
    /// LRA. It does not allocate, but belongs on a control/UI thread rather
    /// than a deadline-sensitive audio callback.
    pub fn snapshot(&mut self) -> MeterSnapshot {
        let momentary_lufs = (self.energy_count >= self.momentary_frames)
            .then(|| loudness_from_energy(self.momentary_sum / self.momentary_frames as f64));
        let max_momentary_lufs = (self.max_momentary_energy > 0.0)
            .then(|| loudness_from_energy(self.max_momentary_energy));
        let short_term_lufs = (self.energy_count >= self.short_term_frames)
            .then(|| loudness_from_energy(self.short_term_sum / self.short_term_frames as f64));
        let max_short_term_lufs = (self.max_short_term_energy > 0.0)
            .then(|| loudness_from_energy(self.max_short_term_energy));
        let integrated_lufs = integrated_loudness(&self.integrated_blocks);
        let loudness_range_lu =
            loudness_range(&self.short_term_blocks, &mut self.percentile_scratch);
        let max_true_peak = self
            .channels
            .iter()
            .map(|channel| channel.true_peak)
            .fold(0.0, f64::max);
        let max_true_peak_dbfs = amplitude_to_db(max_true_peak);
        let psr_db = max_true_peak_dbfs
            .zip(short_term_lufs)
            .map(|(peak, loudness)| peak - loudness);
        let plr_db = max_true_peak_dbfs
            .zip(integrated_lufs)
            .map(|(peak, loudness)| peak - loudness);

        MeterSnapshot {
            momentary_lufs,
            max_momentary_lufs,
            short_term_lufs,
            max_short_term_lufs,
            integrated_lufs,
            loudness_range_lu,
            max_true_peak_dbfs,
            psr_db,
            plr_db,
            duration_seconds: self.samples_processed as f64 / self.sample_rate,
            lra_stable: self.samples_processed as f64 >= 60.0 * self.sample_rate,
            history_complete: self.history_complete,
        }
    }

    /// Returns current metrics for one configured channel.
    pub fn channel_metrics(&self, channel: usize) -> Option<ChannelMetrics> {
        self.channels.get(channel).map(|state| {
            let rms = (state.raw_square_count > 0)
                .then(|| (state.raw_square_sum / state.raw_square_count as f64).sqrt())
                .and_then(amplitude_to_db);
            ChannelMetrics {
                sample_peak_dbfs: amplitude_to_db(state.sample_peak),
                true_peak_dbfs: amplitude_to_db(state.true_peak),
                rms_dbfs: rms,
            }
        })
    }

    /// Number of configured channels.
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// Copies a fixed, resampled loudness histogram into caller-owned storage.
    ///
    /// The source has a constant number of 0.1 LU bins. Shorter destinations
    /// merge adjacent bins from the full range; longer destinations receive
    /// one source bin each and return the fixed source count. This method
    /// never allocates, locks, or scans unbounded programme history.
    pub fn copy_loudness_histogram(
        &self,
        kind: LoudnessHistogramKind,
        destination: &mut [LoudnessHistogramBin],
    ) -> usize {
        let histogram = match kind {
            LoudnessHistogramKind::Integrated => &self.integrated_histogram,
            LoudnessHistogramKind::ShortTerm => &self.short_term_histogram,
        };
        histogram.copy_resampled(destination)
    }

    fn push_energy(&mut self, energy: f64) {
        let leaving_short = (self.energy_count >= self.short_term_frames)
            .then_some(self.energy_ring[self.energy_cursor]);
        let momentary_index = (self.energy_cursor + self.short_term_frames - self.momentary_frames)
            % self.short_term_frames;
        let leaving_momentary = (self.samples_processed >= self.momentary_frames as u64)
            .then_some(self.energy_ring[momentary_index]);

        self.energy_ring[self.energy_cursor] = energy;
        self.energy_cursor = (self.energy_cursor + 1) % self.short_term_frames;
        self.energy_count = (self.energy_count + 1).min(self.short_term_frames);
        self.momentary_sum += energy - leaving_momentary.unwrap_or(0.0);
        self.short_term_sum += energy - leaving_short.unwrap_or(0.0);
        self.samples_processed += 1;

        if self.energy_count >= self.momentary_frames {
            self.max_momentary_energy = self
                .max_momentary_energy
                .max(self.momentary_sum / self.momentary_frames as f64);
        }
        if self.energy_count >= self.short_term_frames {
            self.max_short_term_energy = self
                .max_short_term_energy
                .max(self.short_term_sum / self.short_term_frames as f64);
        }

        if self.samples_processed >= self.momentary_frames as u64
            && (self.samples_processed - self.momentary_frames as u64) % self.step_frames as u64
                == 0
        {
            let block_energy = self.momentary_sum / self.momentary_frames as f64;
            self.record_integrated_block(block_energy);
        }

        if self.samples_processed >= self.short_term_frames as u64
            && (self.samples_processed - self.short_term_frames as u64) % self.step_frames as u64
                == 0
        {
            let block_energy = self.short_term_sum / self.short_term_frames as f64;
            self.record_short_term_block(block_energy);
        }
    }

    fn record_integrated_block(&mut self, energy: f64) {
        // The histogram takes every block regardless of history capacity:
        // its size does not depend on programme length, so a long session
        // stops the exact path but never the realtime one.
        self.integrated_histogram.push(energy);
        if self.integrated_blocks.len() < self.max_history_blocks {
            self.integrated_blocks.push(energy);
        } else {
            self.history_complete = false;
        }
    }

    fn record_short_term_block(&mut self, energy: f64) {
        self.short_term_histogram.push(energy);
        if self.short_term_blocks.len() < self.max_history_blocks {
            self.short_term_blocks.push(energy);
        } else {
            self.history_complete = false;
        }
    }
}

fn validate_sample_rate(sample_rate: f64) -> Result<(), ConfigError> {
    if sample_rate.is_finite() && sample_rate >= 8_000.0 {
        Ok(())
    } else {
        Err(ConfigError::InvalidSampleRate)
    }
}

fn validate_vu_config(config: VuConfig) -> Result<(), VuConfigError> {
    if !config.sample_rate.is_finite() || config.sample_rate < 8_000.0 {
        return Err(VuConfigError::InvalidSampleRate);
    }
    if config.max_block_frames == 0 {
        return Err(VuConfigError::InvalidBlockSize);
    }
    if !config.calibration_dbfs.is_finite() {
        return Err(VuConfigError::InvalidCalibration);
    }
    validate_vu_ballistics(config.attack_seconds, config.release_seconds)?;
    let _ = vu_hold_frames(config.sample_rate, config.peak_hold_seconds)?;
    Ok(())
}

fn validate_vu_ballistics(attack_seconds: f64, release_seconds: f64) -> Result<(), VuConfigError> {
    if attack_seconds.is_finite()
        && release_seconds.is_finite()
        && attack_seconds >= 0.0
        && release_seconds >= 0.0
    {
        Ok(())
    } else {
        Err(VuConfigError::InvalidBallistics)
    }
}

fn vu_hold_frames(sample_rate: f64, seconds: f64) -> Result<u64, VuConfigError> {
    let frames = sample_rate * seconds;
    if seconds.is_finite() && seconds >= 0.0 && frames.is_finite() && frames <= u64::MAX as f64 {
        Ok(frames.round() as u64)
    } else {
        Err(VuConfigError::InvalidPeakHold)
    }
}

fn sample_ballistics_coefficient(time_constant_seconds: f64, sample_rate: f64) -> f64 {
    if time_constant_seconds == 0.0 {
        0.0
    } else {
        (-1.0 / (time_constant_seconds * sample_rate)).exp()
    }
}

fn seconds_to_frames(sample_rate: f64, seconds: f64) -> Result<usize, ConfigError> {
    let frames = (sample_rate * seconds).round();
    if frames.is_finite() && frames >= 1.0 && frames <= usize::MAX as f64 {
        Ok(frames as usize)
    } else {
        Err(ConfigError::InvalidSampleRate)
    }
}

fn loudness_from_energy(energy: f64) -> f64 {
    if energy > 0.0 {
        LOUDNESS_OFFSET + 10.0 * energy.log10()
    } else {
        f64::NEG_INFINITY
    }
}

fn amplitude_to_db(amplitude: f64) -> Option<f64> {
    (amplitude > 0.0).then(|| 20.0 * amplitude.log10())
}

fn integrated_loudness(blocks: &[f64]) -> Option<f64> {
    let (absolute_sum, absolute_count) = blocks
        .iter()
        .copied()
        .filter(|energy| loudness_from_energy(*energy) > ABSOLUTE_GATE_LUFS)
        .fold((0.0, 0_usize), |(sum, count), energy| {
            (sum + energy, count + 1)
        });
    if absolute_count == 0 {
        return None;
    }

    let absolute_loudness = loudness_from_energy(absolute_sum / absolute_count as f64);
    let relative_gate = absolute_loudness + INTEGRATED_RELATIVE_GATE_LU;
    let (relative_sum, relative_count) = blocks
        .iter()
        .copied()
        .filter(|energy| {
            let loudness = loudness_from_energy(*energy);
            loudness > ABSOLUTE_GATE_LUFS && loudness > relative_gate
        })
        .fold((0.0, 0_usize), |(sum, count), energy| {
            (sum + energy, count + 1)
        });

    (relative_count > 0).then(|| loudness_from_energy(relative_sum / relative_count as f64))
}

fn loudness_range(blocks: &[f64], scratch: &mut Vec<f64>) -> Option<f64> {
    let (absolute_sum, absolute_count) = blocks
        .iter()
        .copied()
        .filter(|energy| loudness_from_energy(*energy) > ABSOLUTE_GATE_LUFS)
        .fold((0.0, 0_usize), |(sum, count), energy| {
            (sum + energy, count + 1)
        });
    if absolute_count == 0 {
        return None;
    }

    let relative_gate =
        loudness_from_energy(absolute_sum / absolute_count as f64) + LRA_RELATIVE_GATE_LU;
    scratch.clear();
    for energy in blocks.iter().copied() {
        let loudness = loudness_from_energy(energy);
        if loudness > ABSOLUTE_GATE_LUFS && loudness > relative_gate {
            scratch.push(loudness);
        }
    }
    if scratch.is_empty() {
        return None;
    }

    scratch.sort_unstable_by(f64::total_cmp);
    Some(percentile(scratch, 0.95) - percentile(scratch, 0.10))
}

fn percentile(sorted: &[f64], probability: f64) -> f64 {
    let index = (probability * (sorted.len() - 1) as f64).round() as usize;
    sorted[index]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RATE: f64 = 48_000.0;
    const BLOCK: usize = 512;

    fn process_stereo_tone(
        meter: &mut LoudnessMeter,
        seconds: f64,
        frequency: f64,
        peak_dbfs: f64,
        start_sample: &mut usize,
    ) {
        let total = (seconds * SAMPLE_RATE) as usize;
        let amplitude = 10.0_f64.powf(peak_dbfs / 20.0) as f32;
        let mut left = [0.0_f32; BLOCK];
        let mut right = [0.0_f32; BLOCK];
        let mut done = 0;
        while done < total {
            let frames = (total - done).min(BLOCK);
            for index in 0..frames {
                let phase = 2.0 * PI * frequency * (*start_sample + index) as f64 / SAMPLE_RATE;
                let sample = amplitude * phase.sin() as f32;
                left[index] = sample;
                right[index] = sample;
            }
            meter
                .process_block([&left[..frames], &right[..frames]].into_iter())
                .unwrap();
            *start_sample += frames;
            done += frames;
        }
    }

    /// The binned gating has to agree with the exact gating it exists to
    /// replace. The exact path is kept precisely so it can be the oracle:
    /// same programme, both answers, one bin of tolerance.
    ///
    /// The programme is deliberately awkward for gating — a loud section, a
    /// quiet one 18 LU below it, and silence that both gates must discard.
    #[test]
    fn realtime_gating_agrees_with_the_exact_history_scan() {
        let mut meter = LoudnessMeter::stereo(SAMPLE_RATE, BLOCK).unwrap();
        let mut cursor = 0;
        process_stereo_tone(&mut meter, 6.0, 1_000.0, -12.0, &mut cursor);
        process_stereo_tone(&mut meter, 4.0, 1_000.0, -30.0, &mut cursor);
        process_stereo_tone(&mut meter, 3.0, 1_000.0, -90.0, &mut cursor);
        process_stereo_tone(&mut meter, 5.0, 1_000.0, -12.0, &mut cursor);

        let realtime = meter.realtime_snapshot();
        let exact = meter.snapshot();

        let realtime_integrated = realtime.integrated_lufs.expect("realtime integrated");
        let exact_integrated = exact.integrated_lufs.expect("exact integrated");
        assert_close(realtime_integrated, exact_integrated, HISTOGRAM_BIN_LU);

        let realtime_range = realtime.loudness_range_lu.expect("realtime LRA");
        let exact_range = exact.loudness_range_lu.expect("exact LRA");
        // Two quantized percentiles, so the spread can move by two bins.
        assert_close(realtime_range, exact_range, 2.0 * HISTOGRAM_BIN_LU);

        // And the reading is the programme's, not an artefact of binning:
        // the loud sections dominate a -10 LU gate. A -12 dBFS sine is
        // -15.01 dBFS RMS per channel, and two correlated channels at unit
        // weight sum back to about -12 LUFS.
        assert_close(exact_integrated, -12.0, 1.0);
    }

    /// Silence has no loudness, and the realtime path must say so rather
    /// than reporting the bottom of its bin range.
    #[test]
    fn realtime_gating_reports_nothing_for_silence() {
        let mut meter = LoudnessMeter::stereo(SAMPLE_RATE, BLOCK).unwrap();
        let mut cursor = 0;
        process_stereo_tone(&mut meter, 4.0, 1_000.0, -120.0, &mut cursor);

        let realtime = meter.realtime_snapshot();
        assert_eq!(realtime.integrated_lufs, None);
        assert_eq!(realtime.loudness_range_lu, None);
        assert_eq!(realtime.plr_db, None);
    }

    /// The whole point of binning is that cost stops tracking programme
    /// length: the histogram keeps answering after history storage is full
    /// and the exact scan has stopped being complete.
    #[test]
    fn realtime_gating_outlives_bounded_history() {
        let mut meter = LoudnessMeter::with_history(SAMPLE_RATE, &[1.0, 1.0], BLOCK, 8).unwrap();
        let mut cursor = 0;
        process_stereo_tone(&mut meter, 12.0, 1_000.0, -12.0, &mut cursor);

        let exact = meter.snapshot();
        assert!(
            !exact.history_complete,
            "test premise: history should overflow"
        );
        assert!(
            meter.realtime_snapshot().integrated_lufs.is_some(),
            "the histogram is not bounded by history capacity"
        );
    }

    fn assert_close(actual: f64, expected: f64, tolerance: f64) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "expected {expected} +/- {tolerance}, got {actual}"
        );
    }

    fn energy_at_loudness(loudness: f64) -> f64 {
        10.0_f64.powf((loudness - LOUDNESS_OFFSET) / 10.0)
    }

    fn push_level(meter: &mut LoudnessMeter, seconds: f64, loudness: Option<f64>) {
        let frames = (seconds * SAMPLE_RATE).round() as usize;
        let energy = loudness.map_or(0.0, energy_at_loudness);
        for _ in 0..frames {
            meter.push_energy(energy);
        }
    }

    fn true_peak_sine(frequency: f64, amplitude: f64, phase_degrees: f64) -> f64 {
        let mut filter = TruePeakFilter::default();
        let mut peak = 0.0_f64;
        let phase = phase_degrees.to_radians();
        for index in 0..1_024 {
            let sample =
                amplitude * (2.0 * PI * frequency * index as f64 / SAMPLE_RATE + phase).sin();
            let interpolated = filter.process(sample);
            // Official tones use a 10 ms fade. Ignoring FIR startup models
            // the same steady-state measurement without adding envelope math.
            if index >= 64 {
                peak = peak.max(interpolated);
            }
        }
        amplitude_to_db(peak).unwrap()
    }

    #[test]
    fn passthrough_returns_its_input() {
        let mut filter = Biquad::passthrough();
        for sample in [0.0, 1.0, -0.5, 0.25, -1.0] {
            assert_eq!(filter.process(sample), sample);
        }
        assert_eq!(filter.dc_gain(), 1.0);
    }

    #[test]
    fn an_impulse_walks_out_the_numerator() {
        let mut filter = Biquad::new([0.5, -0.25, 0.125], [0.0, 0.0]);
        assert_eq!(filter.process(1.0), 0.5);
        assert_eq!(filter.process(0.0), -0.25);
        assert_eq!(filter.process(0.0), 0.125);
        assert_eq!(filter.process(0.0), 0.0);
    }

    #[test]
    fn feedback_reaches_the_output() {
        let mut filter = Biquad::new([1.0, 0.0, 0.0], [-0.5, 0.0]);
        assert_eq!(filter.process(1.0), 1.0);
        assert_eq!(filter.process(0.0), 0.5);
        assert_eq!(filter.process(0.0), 0.25);
    }

    #[test]
    fn reset_forgets_biquad_state() {
        let mut filter = Biquad::new([1.0, 0.0, 0.0], [-0.5, 0.0]);
        filter.process(1.0);
        filter.reset();
        assert_eq!(filter.process(0.0), 0.0);
    }

    #[test]
    fn k_weighting_uses_exact_48khz_coefficients() {
        let weighting = KWeighting::new(SAMPLE_RATE).unwrap();
        assert_eq!(
            weighting.shelf.coefficients(),
            (
                [
                    1.535_124_859_586_97,
                    -2.691_696_189_406_38,
                    1.198_392_810_852_85
                ],
                [-1.690_659_293_182_41, 0.732_480_774_215_85]
            )
        );
        assert_eq!(
            weighting.high_pass.coefficients(),
            (
                [1.0, -2.0, 1.0],
                [-1.990_047_454_833_98, 0.990_072_250_366_21]
            )
        );
    }

    #[test]
    fn tech_3341_alignment_tone_reads_minus_18_lufs() {
        let mut meter = LoudnessMeter::with_history(SAMPLE_RATE, &[1.0, 1.0], BLOCK, 10).unwrap();
        let mut sample = 0;
        process_stereo_tone(&mut meter, 4.0, 1_000.0, -18.0, &mut sample);
        let snapshot = meter.snapshot();
        assert_close(snapshot.momentary_lufs.unwrap(), -18.0, 0.1);
        assert_close(snapshot.short_term_lufs.unwrap(), -18.0, 0.1);
        assert_close(snapshot.integrated_lufs.unwrap(), -18.0, 0.1);
    }

    #[test]
    fn tech_3341_test_1_minus_23_stereo_tone() {
        let mut meter = LoudnessMeter::with_history(SAMPLE_RATE, &[1.0, 1.0], BLOCK, 5).unwrap();
        let mut sample = 0;
        process_stereo_tone(&mut meter, 4.0, 1_000.0, -23.0, &mut sample);
        let snapshot = meter.snapshot();
        assert_close(snapshot.momentary_lufs.unwrap(), -23.0, 0.1);
        assert_close(snapshot.short_term_lufs.unwrap(), -23.0, 0.1);
        assert_close(snapshot.integrated_lufs.unwrap(), -23.0, 0.1);
    }

    #[test]
    fn tech_3341_test_2_minus_33_stereo_tone() {
        let mut meter = LoudnessMeter::with_history(SAMPLE_RATE, &[1.0, 1.0], BLOCK, 5).unwrap();
        let mut sample = 0;
        process_stereo_tone(&mut meter, 4.0, 1_000.0, -33.0, &mut sample);
        let snapshot = meter.snapshot();
        assert_close(snapshot.momentary_lufs.unwrap(), -33.0, 0.1);
        assert_close(snapshot.short_term_lufs.unwrap(), -33.0, 0.1);
        assert_close(snapshot.integrated_lufs.unwrap(), -33.0, 0.1);
    }

    #[test]
    fn tech_3341_test_3_relative_gate() {
        let mut blocks = vec![energy_at_loudness(-36.0); 100];
        blocks.extend(std::iter::repeat_n(energy_at_loudness(-23.0), 600));
        blocks.extend(std::iter::repeat_n(energy_at_loudness(-36.0), 100));
        assert_close(integrated_loudness(&blocks).unwrap(), -23.0, 0.1);
    }

    #[test]
    fn tech_3341_test_4_absolute_and_relative_gates() {
        let mut blocks = vec![energy_at_loudness(-72.0); 100];
        blocks.extend(std::iter::repeat_n(energy_at_loudness(-36.0), 100));
        blocks.extend(std::iter::repeat_n(energy_at_loudness(-23.0), 600));
        blocks.extend(std::iter::repeat_n(energy_at_loudness(-36.0), 100));
        blocks.extend(std::iter::repeat_n(energy_at_loudness(-72.0), 100));
        assert_close(integrated_loudness(&blocks).unwrap(), -23.0, 0.1);
    }

    #[test]
    fn tech_3341_test_5_three_level_integration() {
        let mut blocks = vec![energy_at_loudness(-26.0); 200];
        blocks.extend(std::iter::repeat_n(energy_at_loudness(-20.0), 201));
        blocks.extend(std::iter::repeat_n(energy_at_loudness(-26.0), 200));
        assert_close(integrated_loudness(&blocks).unwrap(), -23.0, 0.1);
    }

    #[test]
    fn tech_3341_test_6_five_channel_weights() {
        let weights = [1.0, 1.0, 1.0, 1.41, 1.41];
        let levels = [-28.0, -28.0, -24.0, -30.0, -30.0];
        let amplitudes = levels.map(|level| 10.0_f64.powf(level / 20.0) as f32);
        let mut meter = LoudnessMeter::with_history(SAMPLE_RATE, &weights, BLOCK, 5).unwrap();
        let mut channels = vec![vec![0.0_f32; BLOCK]; weights.len()];
        let total = 4 * SAMPLE_RATE as usize;
        let mut sample_index = 0;
        while sample_index < total {
            let frames = (total - sample_index).min(BLOCK);
            for (channel, amplitude) in channels.iter_mut().zip(amplitudes) {
                for (index, sample) in channel[..frames].iter_mut().enumerate() {
                    let phase = 2.0 * PI * 1_000.0 * (sample_index + index) as f64 / SAMPLE_RATE;
                    *sample = amplitude * phase.sin() as f32;
                }
            }
            meter
                .process_block(channels.iter().map(|channel| &channel[..frames]))
                .unwrap();
            sample_index += frames;
        }
        assert_close(meter.snapshot().integrated_lufs.unwrap(), -23.0, 0.1);
    }

    #[test]
    fn tech_3341_test_9_short_term_rectangular_window() {
        let mut meter = LoudnessMeter::with_history(SAMPLE_RATE, &[1.0, 1.0], BLOCK, 20).unwrap();
        for _ in 0..5 {
            push_level(&mut meter, 1.34, Some(-20.0));
            push_level(&mut meter, 1.66, Some(-30.0));
        }
        assert_close(meter.snapshot().short_term_lufs.unwrap(), -23.0, 0.1);
    }

    #[test]
    fn tech_3341_test_10_max_short_term_ignores_leading_silence() {
        for index in 0..20 {
            let mut meter =
                LoudnessMeter::with_history(SAMPLE_RATE, &[1.0, 1.0], BLOCK, 10).unwrap();
            push_level(&mut meter, index as f64 * 0.15, None);
            push_level(&mut meter, 3.0, Some(-23.0));
            push_level(&mut meter, 1.0, None);
            assert_close(meter.snapshot().max_short_term_lufs.unwrap(), -23.0, 0.1);
        }
    }

    #[test]
    fn tech_3341_test_11_live_max_short_term_sequence() {
        let mut meter = LoudnessMeter::with_history(SAMPLE_RATE, &[1.0, 1.0], BLOCK, 130).unwrap();
        for index in 0..20 {
            let leading = index as f64 * 0.15;
            let level = -38.0 + index as f64;
            push_level(&mut meter, leading, None);
            push_level(&mut meter, 3.0, Some(level));
            push_level(&mut meter, 3.0 - leading, None);
            assert_close(meter.snapshot().max_short_term_lufs.unwrap(), level, 0.1);
        }
    }

    #[test]
    fn tech_3341_test_12_momentary_rectangular_window() {
        let mut meter = LoudnessMeter::with_history(SAMPLE_RATE, &[1.0, 1.0], BLOCK, 20).unwrap();
        for _ in 0..25 {
            push_level(&mut meter, 0.18, Some(-20.0));
            push_level(&mut meter, 0.22, Some(-30.0));
        }
        assert_close(meter.snapshot().momentary_lufs.unwrap(), -23.0, 0.1);
    }

    #[test]
    fn tech_3341_test_13_max_momentary_ignores_leading_silence() {
        for index in 0..20 {
            let mut meter =
                LoudnessMeter::with_history(SAMPLE_RATE, &[1.0, 1.0], BLOCK, 3).unwrap();
            push_level(&mut meter, index as f64 * 0.02, None);
            push_level(&mut meter, 0.4, Some(-23.0));
            push_level(&mut meter, 1.0, None);
            assert_close(meter.snapshot().max_momentary_lufs.unwrap(), -23.0, 0.1);
        }
    }

    #[test]
    fn tech_3341_test_14_live_max_momentary_sequence() {
        let mut meter = LoudnessMeter::with_history(SAMPLE_RATE, &[1.0, 1.0], BLOCK, 20).unwrap();
        for index in 0..20 {
            let leading = index as f64 * 0.02;
            let level = -38.0 + index as f64;
            push_level(&mut meter, leading, None);
            push_level(&mut meter, 0.4, Some(level));
            push_level(&mut meter, 0.4 - leading, None);
            assert_close(meter.snapshot().max_momentary_lufs.unwrap(), level, 0.1);
        }
    }

    #[test]
    fn tech_3341_test_15_true_peak_quarter_rate_zero_phase() {
        assert_close(true_peak_sine(SAMPLE_RATE / 4.0, 0.5, 0.0), -6.0, 0.4);
    }

    #[test]
    fn tech_3341_test_16_true_peak_quarter_rate_45_degrees() {
        assert_close(true_peak_sine(SAMPLE_RATE / 4.0, 0.5, 45.0), -6.0, 0.4);
    }

    #[test]
    fn tech_3341_test_17_true_peak_sixth_rate_60_degrees() {
        assert_close(true_peak_sine(SAMPLE_RATE / 6.0, 0.5, 60.0), -6.0, 0.4);
    }

    #[test]
    fn tech_3341_test_18_true_peak_eighth_rate_67_5_degrees() {
        assert_close(true_peak_sine(SAMPLE_RATE / 8.0, 0.5, 67.5), -6.0, 0.4);
    }

    #[test]
    fn tech_3341_test_19_true_peak_above_full_scale() {
        assert_close(true_peak_sine(SAMPLE_RATE / 4.0, 1.41, 45.0), 3.0, 0.4);
    }

    #[test]
    fn tech_3342_test_1_ten_lu_range() {
        let loud = energy_at_loudness(-20.0);
        let quiet = energy_at_loudness(-30.0);
        let mut blocks = vec![loud; 200];
        blocks.extend(std::iter::repeat_n(quiet, 200));
        let mut scratch = Vec::with_capacity(blocks.len());
        assert_close(loudness_range(&blocks, &mut scratch).unwrap(), 10.0, 0.01);
    }

    #[test]
    fn tech_3342_test_2_five_lu_range() {
        let mut blocks = vec![energy_at_loudness(-20.0); 200];
        blocks.extend(std::iter::repeat_n(energy_at_loudness(-15.0), 200));
        let mut scratch = Vec::with_capacity(blocks.len());
        assert_close(loudness_range(&blocks, &mut scratch).unwrap(), 5.0, 0.01);
    }

    #[test]
    fn tech_3342_test_3_twenty_lu_range() {
        let mut blocks = vec![energy_at_loudness(-40.0); 200];
        blocks.extend(std::iter::repeat_n(energy_at_loudness(-20.0), 200));
        let mut scratch = Vec::with_capacity(blocks.len());
        assert_close(loudness_range(&blocks, &mut scratch).unwrap(), 20.0, 0.01);
    }

    #[test]
    fn tech_3342_test_4_relative_gate_yields_fifteen_lu() {
        let mut blocks = vec![energy_at_loudness(-50.0); 200];
        blocks.extend(std::iter::repeat_n(energy_at_loudness(-35.0), 200));
        blocks.extend(std::iter::repeat_n(energy_at_loudness(-20.0), 200));
        blocks.extend(std::iter::repeat_n(energy_at_loudness(-35.0), 200));
        blocks.extend(std::iter::repeat_n(energy_at_loudness(-50.0), 200));
        let mut scratch = Vec::with_capacity(blocks.len());
        assert_close(loudness_range(&blocks, &mut scratch).unwrap(), 15.0, 0.01);
    }

    #[test]
    fn true_peak_finds_peak_between_samples() {
        let mut filter = TruePeakFilter::default();
        let mut sample_peak = 0.0_f64;
        let mut true_peak = 0.0_f64;
        for index in 0..256 {
            let sample = (2.0 * PI * 12_000.0 * index as f64 / SAMPLE_RATE + PI / 4.0).sin();
            sample_peak = sample_peak.max(sample.abs());
            true_peak = true_peak.max(filter.process(sample));
        }
        assert!(sample_peak < 0.708);
        assert!(
            true_peak > 0.95,
            "expected inter-sample peak near unity, got {true_peak}"
        );
    }

    #[test]
    fn process_rejects_wrong_shape_before_audio_work() {
        let mut meter = LoudnessMeter::with_history(SAMPLE_RATE, &[1.0, 1.0], 16, 1).unwrap();
        let mono = [0.0_f32; 16];
        assert_eq!(
            meter.process_block([&mono[..]].into_iter()),
            Err(ProcessError::ChannelCount {
                expected: 2,
                actual: 1
            })
        );
        let large = [0.0_f32; 17];
        assert_eq!(
            meter.process_block([&large[..], &large[..]].into_iter()),
            Err(ProcessError::BlockTooLarge {
                maximum: 16,
                actual: 17
            })
        );
    }

    #[test]
    fn uneven_block_does_not_mutate_meter_state() {
        let mut meter = LoudnessMeter::with_history(SAMPLE_RATE, &[1.0, 1.0], 16, 1).unwrap();
        let left = [0.75_f32; 16];
        let right = [0.75_f32; 15];

        assert_eq!(
            meter.process_block([&left[..], &right[..]].into_iter()),
            Err(ProcessError::UnevenChannels)
        );

        assert_eq!(meter.realtime_snapshot().processed_frames, 0);
        assert_eq!(meter.channel_metrics(0).unwrap().sample_peak_dbfs, None);
        assert_eq!(meter.channel_metrics(1).unwrap().sample_peak_dbfs, None);
    }

    #[test]
    fn non_finite_samples_become_silence_without_poisoning_meter() {
        let mut meter = LoudnessMeter::with_history(SAMPLE_RATE, &[1.0, 1.0], 16, 1).unwrap();
        let left = [0.25, f32::NAN, f32::INFINITY, f32::NEG_INFINITY];
        let right = left;

        meter
            .process_block([&left[..], &right[..]].into_iter())
            .unwrap();

        let metrics = meter.channel_metrics(0).unwrap();
        assert!(metrics.sample_peak_dbfs.unwrap().is_finite());
        assert!(metrics.true_peak_dbfs.unwrap().is_finite());
        assert_eq!(meter.realtime_snapshot().processed_frames, 4);
    }

    #[test]
    fn reset_preserves_capacity_and_clears_measurement() {
        let mut meter = LoudnessMeter::with_history(SAMPLE_RATE, &[1.0, 1.0], BLOCK, 10).unwrap();
        let history_capacity = meter.integrated_blocks.capacity();
        let mut sample = 0;
        process_stereo_tone(&mut meter, 1.0, 1_000.0, -18.0, &mut sample);
        assert!(meter.snapshot().integrated_lufs.is_some());
        meter.reset();
        assert_eq!(meter.integrated_blocks.capacity(), history_capacity);
        let snapshot = meter.snapshot();
        assert_eq!(snapshot.integrated_lufs, None);
        assert_eq!(snapshot.duration_seconds, 0.0);
    }

    #[test]
    fn loudness_histogram_resamples_fixed_storage_without_history_scan() {
        let mut meter = LoudnessMeter::with_history(SAMPLE_RATE, &[1.0, 1.0], BLOCK, 1).unwrap();
        meter.integrated_histogram.push(energy_at_loudness(-32.0));
        meter.integrated_histogram.push(energy_at_loudness(-20.0));
        meter.integrated_histogram.push(energy_at_loudness(-20.0));
        let mut output = [LoudnessHistogramBin::default(); 4];

        assert_eq!(
            meter.copy_loudness_histogram(LoudnessHistogramKind::Integrated, &mut output),
            4
        );
        assert_eq!(output.iter().map(|bin| bin.count).sum::<u64>(), 3);
        assert_eq!(output[0].low_lufs, LOUDNESS_HISTOGRAM_MIN_LUFS);
        assert_close(
            output[3].high_lufs,
            LOUDNESS_HISTOGRAM_MAX_LUFS + LOUDNESS_HISTOGRAM_BIN_LU,
            0.000_001,
        );
        assert!(
            output
                .iter()
                .filter_map(|bin| bin.mean_lufs)
                .any(|mean| mean > -21.0)
        );
    }

    #[test]
    fn vu_meter_reports_calibrated_rms_and_holds_peak() {
        let mut meter = VuMeter::new(VuConfig {
            sample_rate: SAMPLE_RATE,
            max_block_frames: BLOCK,
            mode: VuMode::Rms,
            calibration_dbfs: -18.0,
            attack_seconds: 0.0,
            release_seconds: 0.3,
            peak_hold_seconds: 0.2,
        })
        .unwrap();
        let samples = [10.0_f32.powf(-18.0 / 20.0); BLOCK];
        meter.process_stereo(&samples, &samples).unwrap();
        let snapshot = meter.snapshot();
        assert_close(snapshot.left.level_dbfs.unwrap(), -18.0, 0.01);
        assert_close(snapshot.left.calibrated_db.unwrap(), 0.0, 0.01);

        meter.process_stereo(&[0.0; BLOCK], &[0.0; BLOCK]).unwrap();
        let held = meter.snapshot().left.peak_hold_dbfs.unwrap();
        assert_close(held, -18.0, 0.01);
        meter.set_mode(VuMode::Peak);
        assert_eq!(meter.snapshot().left.level_dbfs, None);
    }

    #[test]
    fn vu_meter_peak_mode_is_instant_and_rejects_bad_blocks() {
        let mut meter = VuMeter::stereo(SAMPLE_RATE, 8).unwrap();
        meter.set_mode(VuMode::Peak);
        meter.set_ballistics(0.0, 0.0).unwrap();
        meter.process_stereo(&[0.5; 8], &[0.25; 8]).unwrap();
        assert_close(meter.snapshot().left.level_dbfs.unwrap(), -6.0206, 0.001);
        assert_close(meter.snapshot().right.level_dbfs.unwrap(), -12.0412, 0.001);
        assert_eq!(
            meter.process_stereo(&[0.0; 8], &[0.0; 7]),
            Err(VuProcessError::UnevenChannels)
        );
        assert_eq!(meter.snapshot().processed_frames, 8);
    }
}
