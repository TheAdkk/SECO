//! Persisted Neta state and tiny RT parser.
//!
//! Editor state may allocate while the main thread decodes it. Audio only
//! reads RuntimeSettings, copied from bytes without allocation or locks.

pub(crate) const MODULES: [&str; 9] = [
    "loudness",
    "spectrum",
    "spectrogram",
    "waveform",
    "stereo",
    "object",
    "visuals",
    "vu",
    "oscilloscope",
];
pub(crate) const MODULE_COUNT: usize = MODULES.len();

const STATE_VERSION: u8 = 2;
const USER_SLOT_COUNT: usize = 4;
const MAX_PATH: usize = 4_096;
const MAX_SLOT: usize = 8_192;
const MIN_WIDTH: u32 = 52;
const MAX_WIDTH: u32 = 2_000;
const TARGETS: [i8; 5] = [-24, -23, -16, -14, -9];
/// Object zoom bounds, in tenths. The floor keeps the cloud on screen; the
/// ceiling is where a point cloud stops being a car and becomes its points.
const OBJECT_ZOOM_MIN: u16 = 5;
const OBJECT_ZOOM_MAX: u16 = 40;
const DEFAULT_ENABLED: [bool; MODULE_COUNT] =
    [true, true, true, true, true, true, false, true, true];
const DEFAULT_ORDER: [usize; MODULE_COUNT] = [0, 7, 1, 2, 3, 8, 4, 5, 6];
const DEFAULT_WIDTHS: [u32; MODULE_COUNT] = [150, 120, 220, 180, 180, 180, 160, 120, 170];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum AnalysisSource {
    #[default]
    Stereo,
    Left,
    Right,
    Mid,
    Side,
}

impl AnalysisSource {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Stereo => "stereo",
            Self::Left => "left",
            Self::Right => "right",
            Self::Mid => "mid",
            Self::Side => "side",
        }
    }

    fn parse(value: &[u8]) -> Option<Self> {
        if value == b"stereo" {
            Some(Self::Stereo)
        } else if value == b"left" {
            Some(Self::Left)
        } else if value == b"right" {
            Some(Self::Right)
        } else if value == b"mid" {
            Some(Self::Mid)
        } else if value == b"side" {
            Some(Self::Side)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum FftSize {
    Fft1024,
    #[default]
    Fft2048,
    Fft4096,
    Fft8192,
    Fft16384,
}

impl FftSize {
    pub(crate) const fn frames(self) -> usize {
        match self {
            Self::Fft1024 => 1_024,
            Self::Fft2048 => 2_048,
            Self::Fft4096 => 4_096,
            Self::Fft8192 => 8_192,
            Self::Fft16384 => 16_384,
        }
    }

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Fft1024 => 0,
            Self::Fft2048 => 1,
            Self::Fft4096 => 2,
            Self::Fft8192 => 3,
            Self::Fft16384 => 4,
        }
    }

    fn parse(value: &[u8]) -> Option<Self> {
        if value == b"1024" {
            Some(Self::Fft1024)
        } else if value == b"2048" {
            Some(Self::Fft2048)
        } else if value == b"4096" {
            Some(Self::Fft4096)
        } else if value == b"8192" {
            Some(Self::Fft8192)
        } else if value == b"16384" {
            Some(Self::Fft16384)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum SpectrumScale {
    #[default]
    Log,
    Mel,
    Linear,
}

impl SpectrumScale {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Mel => "mel",
            Self::Linear => "linear",
        }
    }

    fn parse(value: &[u8]) -> Option<Self> {
        if value == b"log" {
            Some(Self::Log)
        } else if value == b"mel" {
            Some(Self::Mel)
        } else if value == b"linear" {
            Some(Self::Linear)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum VuMode {
    #[default]
    Vu,
    Rms,
    Peak,
}

impl VuMode {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Vu => "vu",
            Self::Rms => "rms",
            Self::Peak => "peak",
        }
    }

    fn parse(value: &[u8]) -> Option<Self> {
        if value == b"vu" {
            Some(Self::Vu)
        } else if value == b"rms" {
            Some(Self::Rms)
        } else if value == b"peak" {
            Some(Self::Peak)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum OscilloscopeMode {
    #[default]
    Pitch,
    Free,
}

impl OscilloscopeMode {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Pitch => "pitch",
            Self::Free => "free",
        }
    }

    fn parse(value: &[u8]) -> Option<Self> {
        if value == b"pitch" {
            Some(Self::Pitch)
        } else if value == b"free" {
            Some(Self::Free)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum OscilloscopeCycles {
    Single,
    #[default]
    Multi,
}

impl OscilloscopeCycles {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Multi => "multi",
        }
    }

    fn parse(value: &[u8]) -> Option<Self> {
        if value == b"single" {
            Some(Self::Single)
        } else if value == b"multi" {
            Some(Self::Multi)
        } else {
            None
        }
    }
}

/// Audio-thread view. All fields are Copy. This parser owns no String,
/// Vec, RefCell, Mutex, or host object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeSettings {
    enabled: [bool; MODULE_COUNT],
    pub(crate) analysis_source: AnalysisSource,
    pub(crate) trim_db: i8,
    fft_size: FftSize,
    pub(crate) spectrum_scale: SpectrumScale,
    pub(crate) spectrum_smoothing: u8,
    pub(crate) spectrum_tilt_db: i8,
    pub(crate) spectrum_hold: bool,
    pub(crate) vu_mode: VuMode,
    pub(crate) vu_calibration: i8,
    pub(crate) oscilloscope_mode: OscilloscopeMode,
    pub(crate) oscilloscope_cycles: OscilloscopeCycles,
    pub(crate) loudness_timeline: bool,
    pub(crate) loudness_histogram: bool,
    pub(crate) loudness_overs: bool,
    pub(crate) target_lufs: i8,
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_ENABLED,
            analysis_source: AnalysisSource::Stereo,
            trim_db: 0,
            fft_size: FftSize::Fft2048,
            spectrum_scale: SpectrumScale::Log,
            spectrum_smoothing: 90,
            spectrum_tilt_db: 0,
            spectrum_hold: false,
            vu_mode: VuMode::Vu,
            vu_calibration: -18,
            oscilloscope_mode: OscilloscopeMode::Pitch,
            oscilloscope_cycles: OscilloscopeCycles::Multi,
            loudness_timeline: true,
            loudness_histogram: true,
            loudness_overs: true,
            target_lufs: -14,
        }
    }
}

impl RuntimeSettings {
    /// RT-safe scalar parser. Unknown or malformed values retain defaults.
    pub(crate) fn parse(block: &[u8]) -> Self {
        let mut settings = Self::default();
        let v2 = state_version(block) >= STATE_VERSION;
        let mut cursor = 0;
        while let Some((key, value)) = next_field(block, &mut cursor) {
            if key == b"m" {
                parse_enabled(value, v2, &mut settings.enabled);
            } else if key == b"route" {
                if let Some(source) = AnalysisSource::parse(value) {
                    settings.analysis_source = source;
                }
            } else if key == b"trim" {
                if let Some(value) = valid_i8(value, -24, 24) {
                    settings.trim_db = value;
                }
            } else if key == b"fft" {
                if let Some(value) = FftSize::parse(value) {
                    settings.fft_size = value;
                }
            } else if key == b"scale" {
                if let Some(value) = SpectrumScale::parse(value) {
                    settings.spectrum_scale = value;
                }
            } else if key == b"smooth" {
                if let Some(value) =
                    valid_i8(value, 0, 100).and_then(|value| u8::try_from(value).ok())
                {
                    settings.spectrum_smoothing = value;
                }
            } else if key == b"tilt" {
                if let Some(value) = valid_i8(value, -18, 18) {
                    settings.spectrum_tilt_db = value;
                }
            } else if key == b"hold" {
                if let Some(value) = parse_bool(value) {
                    settings.spectrum_hold = value;
                }
            } else if key == b"vu" {
                if let Some(value) = VuMode::parse(value) {
                    settings.vu_mode = value;
                }
            } else if key == b"cal" {
                if let Some(value) = valid_i8(value, -36, -6) {
                    settings.vu_calibration = value;
                }
            } else if key == b"osc" {
                if let Some(value) = OscilloscopeMode::parse(value) {
                    settings.oscilloscope_mode = value;
                }
            } else if key == b"cycles" {
                if let Some(value) = OscilloscopeCycles::parse(value) {
                    settings.oscilloscope_cycles = value;
                }
            } else if key == b"timeline" {
                if let Some(value) = parse_bool(value) {
                    settings.loudness_timeline = value;
                }
            } else if key == b"hist" {
                if let Some(value) = parse_bool(value) {
                    settings.loudness_histogram = value;
                }
            } else if key == b"overs" {
                if let Some(value) = parse_bool(value) {
                    settings.loudness_overs = value;
                }
            } else if key == b"target" {
                if let Some(value) = valid_i8(value, -60, 0).filter(|value| TARGETS.contains(value))
                {
                    settings.target_lufs = value;
                }
            }
        }
        settings
    }

    pub(crate) const fn module_enabled(&self, module: usize) -> bool {
        if module < MODULE_COUNT {
            self.enabled[module]
        } else {
            false
        }
    }

    pub(crate) const fn fft_index(&self) -> usize {
        self.fft_size.index()
    }

    pub(crate) const fn fft_frames(&self) -> usize {
        self.fft_size.frames()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Theme {
    #[default]
    Midnight,
    Coal,
    Mono,
}

impl Theme {
    const fn name(self) -> &'static str {
        match self {
            Self::Midnight => "midnight",
            Self::Coal => "coal",
            Self::Mono => "mono",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        match value {
            "midnight" => Some(Self::Midnight),
            "coal" => Some(Self::Coal),
            "mono" => Some(Self::Mono),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ColorMap {
    #[default]
    Neta,
    Inferno,
    Viridis,
}

impl ColorMap {
    const fn name(self) -> &'static str {
        match self {
            Self::Neta => "neta",
            Self::Inferno => "inferno",
            Self::Viridis => "viridis",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        match value {
            "neta" => Some(Self::Neta),
            "inferno" => Some(Self::Inferno),
            "viridis" => Some(Self::Viridis),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum FpsLimit {
    #[default]
    Vsync,
    Fps60,
    Fps30,
    Fps15,
}

impl FpsLimit {
    const fn name(self) -> &'static str {
        match self {
            Self::Vsync => "vsync",
            Self::Fps60 => "60",
            Self::Fps30 => "30",
            Self::Fps15 => "15",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        match value {
            "vsync" => Some(Self::Vsync),
            "60" => Some(Self::Fps60),
            "30" => Some(Self::Fps30),
            "15" => Some(Self::Fps15),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum SpectrumStyle {
    Fft,
    Bars,
    #[default]
    Both,
}

impl SpectrumStyle {
    const fn name(self) -> &'static str {
        match self {
            Self::Fft => "fft",
            Self::Bars => "bars",
            Self::Both => "both",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        match value {
            "fft" => Some(Self::Fft),
            "bars" => Some(Self::Bars),
            "both" => Some(Self::Both),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum SpectrogramHistory {
    Off,
    #[default]
    Fast,
    Slow,
}

impl SpectrogramHistory {
    const fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Fast => "fast",
            Self::Slow => "slow",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "fast" => Some(Self::Fast),
            "slow" => Some(Self::Slow),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum StereoStyle {
    #[default]
    Linear,
    Vectorscope,
}

impl StereoStyle {
    const fn name(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::Vectorscope => "vectorscope",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        match value {
            "linear" => Some(Self::Linear),
            "vectorscope" => Some(Self::Vectorscope),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum StereoColor {
    #[default]
    Static,
    Rgb,
    Multiband,
}

impl StereoColor {
    const fn name(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Rgb => "rgb",
            Self::Multiband => "multiband",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        match value {
            "static" => Some(Self::Static),
            "rgb" => Some(Self::Rgb),
            "multiband" => Some(Self::Multiband),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum PointSize {
    #[default]
    Auto,
    Small,
    Medium,
    Large,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum WaveformColor {
    #[default]
    Pink,
    Cyan,
    Amber,
}

impl WaveformColor {
    const fn name(self) -> &'static str {
        match self {
            Self::Pink => "pink",
            Self::Cyan => "cyan",
            Self::Amber => "amber",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "pink" => Some(Self::Pink),
            "cyan" => Some(Self::Cyan),
            "amber" => Some(Self::Amber),
            _ => None,
        }
    }
}

impl PointSize {
    const fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "small" => Some(Self::Small),
            "medium" => Some(Self::Medium),
            "large" => Some(Self::Large),
            _ => None,
        }
    }
}

/// How much furniture each panel carries.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Chrome {
    /// No borders, no panel fill, no caption bar. The rail reads as one
    /// instrument and the header appears only under the pointer.
    #[default]
    Bare,
    /// Every panel in its own bordered card, caption always visible.
    Framed,
}

impl Chrome {
    const fn name(self) -> &'static str {
        match self {
            Self::Bare => "bare",
            Self::Framed => "framed",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "bare" => Some(Self::Bare),
            "framed" => Some(Self::Framed),
            _ => None,
        }
    }
}

/// How the waveform module draws its envelope.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum WaveformStyle {
    /// Filled body between the peaks.
    #[default]
    Band,
    /// One column per published point, the shape a DAW draws.
    Bars,
    /// Outline only, for reading the peaks against a busy background.
    Line,
}

impl WaveformStyle {
    const fn name(self) -> &'static str {
        match self {
            Self::Band => "band",
            Self::Bars => "bars",
            Self::Line => "line",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "band" => Some(Self::Band),
            "bars" => Some(Self::Bars),
            "line" => Some(Self::Line),
            _ => None,
        }
    }
}

/// How the oscilloscope lays its two channels out.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum OscilloscopeStyle {
    /// Both traces on one axis. Phase is readable, detail overlaps.
    #[default]
    Overlay,
    /// Left above, right below. Detail is readable, phase needs the eye to
    /// travel.
    Split,
    /// One trace from the mid signal.
    Sum,
}

impl OscilloscopeStyle {
    const fn name(self) -> &'static str {
        match self {
            Self::Overlay => "overlay",
            Self::Split => "split",
            Self::Sum => "sum",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "overlay" => Some(Self::Overlay),
            "split" => Some(Self::Split),
            "sum" => Some(Self::Sum),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum BuiltinPreset {
    #[default]
    Default,
    Mix,
    Master,
    Scope,
}

impl BuiltinPreset {
    const fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Mix => "mix",
            Self::Master => "master",
            Self::Scope => "scope",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        match value {
            "default" => Some(Self::Default),
            "mix" => Some(Self::Mix),
            "master" => Some(Self::Master),
            "scope" => Some(Self::Scope),
            _ => None,
        }
    }
}

/// Main-thread state. Slots contain page-encoded snapshots for four user
/// preset positions. They never cross into process().
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    pub(crate) runtime: RuntimeSettings,
    pub(crate) order: [usize; MODULE_COUNT],
    pub(crate) widths: [u32; MODULE_COUNT],
    pub(crate) model: String,
    pub(crate) theme: Theme,
    pub(crate) color_map: ColorMap,
    pub(crate) fps_limit: FpsLimit,
    pub(crate) spectrum_style: SpectrumStyle,
    pub(crate) spectrogram_history: SpectrogramHistory,
    pub(crate) spectrogram_speed: u8,
    pub(crate) spectrogram_loop: bool,
    pub(crate) spectrogram_timecode: bool,
    pub(crate) stereo_style: StereoStyle,
    pub(crate) stereo_color: StereoColor,
    pub(crate) point_size: PointSize,
    pub(crate) waveform_color: WaveformColor,
    pub(crate) waveform_style: WaveformStyle,
    /// Amplitude zoom for the waveform, as a whole factor. Quiet material is
    /// a flat line at 1x and a shape at 4x, and neither reading is wrong.
    pub(crate) waveform_gain: u8,
    pub(crate) oscilloscope_style: OscilloscopeStyle,
    pub(crate) oscilloscope_gain: u8,
    /// The spectrum's slow-release peak trace.
    pub(crate) spectrum_trace: bool,
    /// Value readout under the pointer, on every panel that has one.
    pub(crate) hover_readout: bool,
    pub(crate) chrome: Chrome,
    pub(crate) object_yaw_tenths: i16,
    pub(crate) object_pitch_tenths: i16,
    /// Object camera zoom in tenths. Clamped rather than free: past a point
    /// the cloud is a handful of points filling the panel.
    pub(crate) object_zoom_tenths: u16,
    pub(crate) object_spin: bool,
    pub(crate) preset: BuiltinPreset,
    pub(crate) slots: [String; USER_SLOT_COUNT],
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            runtime: RuntimeSettings::default(),
            order: DEFAULT_ORDER,
            widths: DEFAULT_WIDTHS,
            model: String::new(),
            theme: Theme::Midnight,
            color_map: ColorMap::Neta,
            fps_limit: FpsLimit::Vsync,
            spectrum_style: SpectrumStyle::Both,
            spectrogram_history: SpectrogramHistory::Fast,
            spectrogram_speed: 4,
            spectrogram_loop: true,
            spectrogram_timecode: false,
            stereo_style: StereoStyle::Linear,
            stereo_color: StereoColor::Static,
            point_size: PointSize::Auto,
            waveform_color: WaveformColor::Pink,
            waveform_style: WaveformStyle::Band,
            waveform_gain: 1,
            oscilloscope_style: OscilloscopeStyle::Overlay,
            oscilloscope_gain: 1,
            spectrum_trace: true,
            hover_readout: true,
            chrome: Chrome::Bare,
            object_yaw_tenths: 0,
            object_pitch_tenths: 0,
            object_zoom_tenths: 10,
            object_spin: true,
            preset: BuiltinPreset::Default,
            slots: std::array::from_fn(|_| String::new()),
        }
    }
}

impl Settings {
    pub(crate) fn parse(block: &str) -> Self {
        let mut settings = Self {
            runtime: RuntimeSettings::parse(block.as_bytes()),
            ..Self::default()
        };
        for field in block.split(';') {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            match key.trim() {
                "o" => {
                    if let Some(order) = parse_order(value) {
                        settings.order = order;
                    }
                }
                "w" => {
                    if let Some(widths) = parse_widths(value) {
                        settings.widths = widths;
                    }
                }
                "model" if value.len() <= MAX_PATH && !value.contains(['\n', '\r']) => {
                    settings.model = value.to_owned()
                }
                "theme" => {
                    if let Some(value) = Theme::parse(value) {
                        settings.theme = value;
                    }
                }
                "map" => {
                    if let Some(value) = ColorMap::parse(value) {
                        settings.color_map = value;
                    }
                }
                "fps" => {
                    if let Some(value) = FpsLimit::parse(value) {
                        settings.fps_limit = value;
                    }
                }
                "style" => {
                    if let Some(value) = SpectrumStyle::parse(value) {
                        settings.spectrum_style = value;
                    }
                }
                "sphist" => {
                    if let Some(value) = SpectrogramHistory::parse(value) {
                        settings.spectrogram_history = value;
                    }
                }
                "speed" => {
                    if let Ok(value) = value.parse::<u8>()
                        && (1..=4).contains(&value)
                    {
                        settings.spectrogram_speed = value;
                    }
                }
                "loop" => settings.spectrogram_loop = value == "1",
                "timecode" => settings.spectrogram_timecode = value == "1",
                "stereo" => {
                    if let Some(value) = StereoStyle::parse(value) {
                        settings.stereo_style = value;
                    }
                }
                "stereocolor" => {
                    if let Some(value) = StereoColor::parse(value) {
                        settings.stereo_color = value;
                    }
                }
                "points" => {
                    if let Some(value) = PointSize::parse(value) {
                        settings.point_size = value;
                    }
                }
                "wavecolor" => {
                    if let Some(value) = WaveformColor::parse(value) {
                        settings.waveform_color = value;
                    }
                }
                "wavestyle" => {
                    if let Some(value) = WaveformStyle::parse(value) {
                        settings.waveform_style = value;
                    }
                }
                "wavegain" => {
                    if let Some(value) = parse_gain(value) {
                        settings.waveform_gain = value;
                    }
                }
                "oscstyle" => {
                    if let Some(value) = OscilloscopeStyle::parse(value) {
                        settings.oscilloscope_style = value;
                    }
                }
                "oscgain" => {
                    if let Some(value) = parse_gain(value) {
                        settings.oscilloscope_gain = value;
                    }
                }
                "chrome" => {
                    if let Some(value) = Chrome::parse(value) {
                        settings.chrome = value;
                    }
                }
                "trace" => settings.spectrum_trace = value == "1",
                "hover" => settings.hover_readout = value == "1",
                "ospin" => settings.object_spin = value == "1",
                "oz" => {
                    if let Ok(value) = value.parse::<u16>()
                        && (OBJECT_ZOOM_MIN..=OBJECT_ZOOM_MAX).contains(&value)
                    {
                        settings.object_zoom_tenths = value;
                    }
                }
                "oy" => {
                    if let Ok(value) = value.parse::<i16>()
                        && (-3600..=3600).contains(&value)
                    {
                        settings.object_yaw_tenths = value;
                    }
                }
                "op" => {
                    if let Ok(value) = value.parse::<i16>()
                        && (-850..=850).contains(&value)
                    {
                        settings.object_pitch_tenths = value;
                    }
                }
                "preset" => {
                    if let Some(value) = BuiltinPreset::parse(value) {
                        settings.preset = value;
                    }
                }
                _ => {
                    if let Some(index) = key
                        .strip_prefix("slot")
                        .and_then(|index| index.parse::<usize>().ok())
                        .filter(|index| *index < USER_SLOT_COUNT)
                        && value.len() <= MAX_SLOT
                        && !value.contains(['\n', '\r'])
                    {
                        settings.slots[index] = value.to_owned();
                    }
                }
            }
        }
        settings
    }

    pub(crate) fn to_script(&self) -> String {
        let flags = self
            .runtime
            .enabled
            .iter()
            .map(|value| if *value { "true" } else { "false" })
            .collect::<Vec<_>>()
            .join(",");
        let order = self
            .order
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let widths = self
            .widths
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let slots = self
            .slots
            .iter()
            .map(|slot| crate::model::quote(slot))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "window.__neta_settings&&window.__neta_settings({{v:2,e:[{flags}],o:[{order}],w:[{widths}],model:{},target:{},theme:{:?},map:{:?},fps:{:?},route:{:?},trim:{},fft:{},scale:{:?},style:{:?},smooth:{},tilt:{},hold:{},vu:{:?},cal:{},osc:{:?},cycles:{:?},timeline:{},hist:{},overs:{},sphist:{:?},speed:{},loop:{},timecode:{},stereo:{:?},stereocolor:{:?},points:{:?},wavecolor:{:?},wavestyle:{:?},wavegain:{},oscstyle:{:?},oscgain:{},trace:{},hover:{},chrome:{:?},oy:{},op:{},oz:{},ospin:{},preset:{:?},slots:[{slots}]}});",
            crate::model::quote(&self.model),
            self.runtime.target_lufs,
            self.theme.name(),
            self.color_map.name(),
            self.fps_limit.name(),
            self.runtime.analysis_source.name(),
            self.runtime.trim_db,
            self.runtime.fft_frames(),
            self.runtime.spectrum_scale.name(),
            self.spectrum_style.name(),
            self.runtime.spectrum_smoothing,
            self.runtime.spectrum_tilt_db,
            bool_text(self.runtime.spectrum_hold),
            self.runtime.vu_mode.name(),
            self.runtime.vu_calibration,
            self.runtime.oscilloscope_mode.name(),
            self.runtime.oscilloscope_cycles.name(),
            bool_text(self.runtime.loudness_timeline),
            bool_text(self.runtime.loudness_histogram),
            bool_text(self.runtime.loudness_overs),
            self.spectrogram_history.name(),
            self.spectrogram_speed,
            bool_text(self.spectrogram_loop),
            bool_text(self.spectrogram_timecode),
            self.stereo_style.name(),
            self.stereo_color.name(),
            self.point_size.name(),
            self.waveform_color.name(),
            self.waveform_style.name(),
            self.waveform_gain,
            self.oscilloscope_style.name(),
            self.oscilloscope_gain,
            bool_text(self.spectrum_trace),
            bool_text(self.hover_readout),
            self.chrome.name(),
            self.object_yaw_tenths,
            self.object_pitch_tenths,
            self.object_zoom_tenths,
            bool_text(self.object_spin),
            self.preset.name(),
        )
    }
}

/// Amplitude zoom, as one of the three whole factors the page offers. Written
/// as a whitelist rather than a range because a 3x waveform is a slider the
/// page does not have.
fn parse_gain(value: &str) -> Option<u8> {
    match value {
        "1" => Some(1),
        "2" => Some(2),
        "4" => Some(4),
        _ => None,
    }
}

const fn bool_text(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn state_version(block: &[u8]) -> u8 {
    let mut cursor = 0;
    while let Some((key, value)) = next_field(block, &mut cursor) {
        if key == b"v" {
            return parse_i32(value)
                .and_then(|value| u8::try_from(value).ok())
                .unwrap_or(1);
        }
    }
    1
}

fn next_field<'a>(block: &'a [u8], cursor: &mut usize) -> Option<(&'a [u8], &'a [u8])> {
    while *cursor < block.len() && (block[*cursor] == b';' || block[*cursor].is_ascii_whitespace())
    {
        *cursor += 1;
    }
    if *cursor == block.len() {
        return None;
    }
    let start = *cursor;
    while *cursor < block.len() && block[*cursor] != b';' {
        *cursor += 1;
    }
    let field = &block[start..*cursor];
    let equals = field.iter().position(|byte| *byte == b'=')?;
    Some((&field[..equals], &field[equals + 1..]))
}

fn parse_enabled(value: &[u8], v2: bool, enabled: &mut [bool; MODULE_COUNT]) {
    let length = if v2 { MODULE_COUNT } else { 7 };
    for (index, flag) in value.iter().copied().enumerate().take(length) {
        enabled[index] = flag == b'1';
    }
}

fn parse_order(value: &str) -> Option<[usize; MODULE_COUNT]> {
    let mut order = [usize::MAX; MODULE_COUNT];
    let mut seen = [false; MODULE_COUNT];
    let mut count = 0;
    for character in value.bytes() {
        if !character.is_ascii_digit() {
            return None;
        }
        let index = usize::from(character - b'0');
        if index >= MODULE_COUNT || seen[index] || count == MODULE_COUNT {
            return None;
        }
        order[count] = index;
        seen[index] = true;
        count += 1;
    }
    (count == MODULE_COUNT).then_some(order)
}

fn parse_widths(value: &str) -> Option<[u32; MODULE_COUNT]> {
    let mut widths = [0; MODULE_COUNT];
    let mut count = 0;
    for text in value.split(',') {
        if count == MODULE_COUNT {
            return None;
        }
        let width = text.parse::<u32>().ok()?;
        if !(MIN_WIDTH..=MAX_WIDTH).contains(&width) {
            return None;
        }
        widths[count] = width;
        count += 1;
    }
    (count == MODULE_COUNT).then_some(widths)
}

fn parse_i32(value: &[u8]) -> Option<i32> {
    let (negative, digits) = match value.split_first() {
        Some((&b'-', rest)) => (true, rest),
        _ => (false, value),
    };
    if digits.is_empty() {
        return None;
    }
    let mut number = 0_i32;
    for digit in digits {
        if !digit.is_ascii_digit() {
            return None;
        }
        number = number
            .checked_mul(10)?
            .checked_add(i32::from(*digit - b'0'))?;
    }
    if negative {
        number.checked_neg()
    } else {
        Some(number)
    }
}

fn valid_i8(value: &[u8], minimum: i8, maximum: i8) -> Option<i8> {
    parse_i32(value)
        .and_then(|value| i8::try_from(value).ok())
        .filter(|value| (minimum..=maximum).contains(value))
}

fn parse_bool(value: &[u8]) -> Option<bool> {
    match value {
        b"0" | b"false" => Some(false),
        b"1" | b"true" => Some(true),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_keeps_legacy_module_positions_and_new_defaults() {
        let runtime = RuntimeSettings::parse(b"m=0101010;target=-23");
        assert!(!runtime.module_enabled(0));
        assert!(runtime.module_enabled(1));
        assert!(!runtime.module_enabled(2));
        assert!(runtime.module_enabled(3));
        assert!(!runtime.module_enabled(4));
        assert!(runtime.module_enabled(5));
        assert!(!runtime.module_enabled(6));
        assert!(runtime.module_enabled(7));
        assert!(runtime.module_enabled(8));
        assert_eq!(runtime.target_lufs, -23);
    }

    #[test]
    fn v2_runtime_parser_is_total_and_copy_only() {
        let runtime = RuntimeSettings::parse(b"v=2;m=101010101;route=side;trim=24;fft=16384;scale=mel;smooth=0;tilt=-18;hold=1;vu=peak;cal=-24;osc=free;cycles=single;timeline=0;hist=0;overs=0;target=-9");
        assert_eq!(runtime.analysis_source, AnalysisSource::Side);
        assert_eq!(runtime.fft_index(), 4);
        assert_eq!(runtime.spectrum_scale, SpectrumScale::Mel);
        assert_eq!(runtime.vu_mode, VuMode::Peak);
        assert_eq!(runtime.oscilloscope_mode, OscilloscopeMode::Free);
        assert_eq!(runtime.oscilloscope_cycles, OscilloscopeCycles::Single);
        assert!(runtime.module_enabled(8));
        assert_eq!(
            RuntimeSettings::parse(b"trim=99;route=nope"),
            RuntimeSettings::default()
        );
    }

    #[test]
    fn settings_persist_global_module_and_slot_choices() {
        let settings = Settings::parse(
            "v=2;m=111111110;o=071238456;w=100,101,102,103,104,105,106,107,108;model=/tmp/a.obj;theme=coal;map=viridis;fps=30;route=mid;trim=-3;fft=4096;scale=linear;style=bars;smooth=40;tilt=4;hold=1;vu=rms;cal=-20;osc=free;cycles=single;timeline=0;hist=0;overs=1;sphist=slow;speed=3;loop=1;timecode=1;stereo=vectorscope;stereocolor=rgb;points=large;wavecolor=amber;oy=120;op=-40;preset=master;slot0=user-layout",
        );
        assert_eq!(settings.runtime.analysis_source, AnalysisSource::Mid);
        assert_eq!(settings.fps_limit, FpsLimit::Fps30);
        assert_eq!(settings.spectrum_style, SpectrumStyle::Bars);
        assert_eq!(settings.waveform_color, WaveformColor::Amber);
        assert_eq!(settings.slots[0], "user-layout");
        let script = settings.to_script();
        assert!(script.contains("v:2"));
        assert!(script.contains("fft:4096"));
        assert!(script.contains("wavecolor:\"amber\""));
        assert!(script.contains("slots:[\"user-layout\""));
    }

    #[test]
    fn layout_rejects_missing_and_duplicate_values() {
        assert_eq!(parse_order("01234567"), None);
        assert_eq!(parse_order("071238456"), Some([0, 7, 1, 2, 3, 8, 4, 5, 6]));
        assert_eq!(parse_order("012345667"), None);
        assert_eq!(parse_widths("100,101,102"), None);
    }

    #[test]
    fn user_slot_keeps_base64_padding() {
        let settings = Settings::parse("v=2;slot0=eyJ2IjoyfQ==");
        assert_eq!(settings.slots[0], "eyJ2IjoyfQ==");
        assert!(settings.to_script().contains("eyJ2IjoyfQ=="));
    }
}
