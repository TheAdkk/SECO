//! Renderer-independent visuals for Neta.
//!
//! This crate deliberately has no window, GPU, CLAP, or audio-thread API.
//! It turns already-published analysis data into bounded 2D primitives that
//! a WebKit compatibility editor and a future WGPU renderer can both draw.
//! All coordinates are normalized device coordinates: `[-1, 1]` on each
//! axis. Composition happens off the audio thread.

#![forbid(unsafe_code)]

pub mod dalia;
pub mod mesh;

use std::f32::consts::SQRT_2;

const DEFAULT_SPECTROGRAM_ROWS: usize = 64;
const DEFAULT_SPECTROGRAM_BANDS: usize = 48;
const WAVE_X0: f32 = -0.43;
const WAVE_X1: f32 = 0.17;
const WAVE_CENTER_Y: f32 = 0.49;
const SPECTRUM_X0: f32 = 0.27;
const SPECTRUM_X1: f32 = 0.91;
const GONIOMETER_CENTER: [f32; 2] = [0.59, -0.46];

/// A linear sRGBA colour. The renderer owns colour-space conversion.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    pub const fn with_alpha(self, a: f32) -> Self {
        Self { a, ..self }
    }

    pub fn mix(self, other: Self, amount: f32) -> Self {
        let t = amount.clamp(0.0, 1.0);
        Self {
            r: self.r + (other.r - self.r) * t,
            g: self.g + (other.g - self.g) * t,
            b: self.b + (other.b - self.b) * t,
            a: self.a + (other.a - self.a) * t,
        }
    }
}

/// Neta's dark room: oxidized black, cobalt tube, hot guava, nickel.
///
/// It deliberately avoids the generic "dark dashboard plus neon green"
/// treatment. Colour conveys which quantity is asking for attention.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub night: Color,
    pub grid: Color,
    pub nickel: Color,
    pub cobalt: Color,
    pub guava: Color,
    pub violet: Color,
}

impl Palette {
    pub const NETA: Self = Self {
        night: Color::rgb(0.018, 0.027, 0.039),
        grid: Color::rgb(0.105, 0.153, 0.173),
        nickel: Color::rgb(0.710, 0.780, 0.784),
        cobalt: Color::rgb(0.102, 0.400, 0.980),
        guava: Color::rgb(1.000, 0.270, 0.204),
        violet: Color::rgb(0.560, 0.310, 0.910),
    };
}

/// Peak-to-peak sample bucket. It preserves transients a simple decimation
/// would erase.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MinMax {
    pub min: f32,
    pub max: f32,
}

/// A stereo observation for the goniometer.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StereoPoint {
    pub left: f32,
    pub right: f32,
}

/// Meter and scope data copied out of an analyzer. `None` means that a
/// duration has not elapsed yet; it is not silently treated as silence.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeterReadings {
    pub momentary_lufs: Option<f32>,
    pub short_term_lufs: Option<f32>,
    pub integrated_lufs: Option<f32>,
    pub lra_lu: Option<f32>,
    pub true_peak_dbfs: f32,
    pub correlation: f32,
    pub stereo_width: f32,
}

/// Borrowed visual input. It is deliberately read-only, so composing a frame
/// cannot mutate analysis state or accidentally become an audio operation.
#[derive(Clone, Copy, Debug)]
pub struct VisualFrame<'a> {
    pub meter: MeterReadings,
    pub waveform: &'a [MinMax],
    /// Linear amplitudes, low frequency first. Values outside `[0, 1]` are
    /// clamped by the composer so bad input cannot draw outside the scene.
    pub spectrum: &'a [f32],
    /// Chronological spectrogram cells in row-major order, oldest row first.
    /// Each value is dBFS. Malformed tails are ignored safely.
    pub spectrogram: &'a [f32],
    /// Number of source rows in [`Self::spectrogram`].
    pub spectrogram_rows: usize,
    /// Number of source bands in every spectrogram row.
    pub spectrogram_bands: usize,
    pub goniometer: &'a [StereoPoint],
}

impl<'a> VisualFrame<'a> {
    pub const fn empty() -> Self {
        Self {
            meter: MeterReadings {
                momentary_lufs: None,
                short_term_lufs: None,
                integrated_lufs: None,
                lra_lu: None,
                true_peak_dbfs: 0.0,
                correlation: 0.0,
                stereo_width: 0.0,
            },
            waveform: &[],
            spectrum: &[],
            spectrogram: &[],
            spectrogram_rows: 0,
            spectrogram_bands: 0,
            goniometer: &[],
        }
    }
}

/// A line segment in normalized device coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Line {
    pub from: [f32; 2],
    pub to: [f32; 2],
    pub width: f32,
    pub color: Color,
}

/// A solid axis-aligned rectangle in normalized device coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quad {
    pub min: [f32; 2],
    pub max: [f32; 2],
    pub color: Color,
}

/// A circular point in normalized device coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Dot {
    pub position: [f32; 2],
    pub radius: f32,
    pub color: Color,
}

/// Renderer input. Vectors are allocated once by [`VisualComposer::new`]
/// and cleared/reused per composition. Excess source points are downsampled
/// rather than growing a frame unexpectedly.
#[derive(Debug, Default)]
pub struct VisualScene {
    pub clear: Color,
    pub lines: Vec<Line>,
    pub quads: Vec<Quad>,
    pub dots: Vec<Dot>,
}

impl VisualScene {
    fn clear_keep_capacity(&mut self, clear: Color) {
        self.clear = clear;
        self.lines.clear();
        self.quads.clear();
        self.dots.clear();
    }
}

/// Maps dBFS / LUFS onto Neta's meter runway.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeterScale {
    pub floor_db: f32,
    pub ceiling_db: f32,
}

impl Default for MeterScale {
    fn default() -> Self {
        Self {
            floor_db: -60.0,
            ceiling_db: 3.0,
        }
    }
}

impl MeterScale {
    /// Maps a dB value to `[0, 1]`. Non-finite values map to the floor.
    pub fn unit(self, db: f32) -> f32 {
        if !db.is_finite() || self.ceiling_db <= self.floor_db {
            return 0.0;
        }
        ((db - self.floor_db) / (self.ceiling_db - self.floor_db)).clamp(0.0, 1.0)
    }
}

/// Converts between log-frequency space and a horizontal screen coordinate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogFrequencyScale {
    pub min_hz: f32,
    pub max_hz: f32,
}

impl Default for LogFrequencyScale {
    fn default() -> Self {
        Self {
            min_hz: 20.0,
            max_hz: 20_000.0,
        }
    }
}

impl LogFrequencyScale {
    pub fn x_for_hz(self, hz: f32) -> f32 {
        if !(self.min_hz > 0.0 && self.max_hz > self.min_hz && hz.is_finite()) {
            return 0.0;
        }
        let min = self.min_hz.ln();
        let max = self.max_hz.ln();
        ((hz.max(self.min_hz).min(self.max_hz).ln() - min) / (max - min)).clamp(0.0, 1.0)
    }

    pub fn hz_at_x(self, x: f32) -> f32 {
        if !(self.min_hz > 0.0 && self.max_hz > self.min_hz) {
            return self.min_hz;
        }
        let min = self.min_hz.ln();
        let max = self.max_hz.ln();
        (min + (max - min) * x.clamp(0.0, 1.0)).exp()
    }
}

/// A bounded, renderer-neutral scene builder.
#[derive(Debug)]
pub struct VisualComposer {
    palette: Palette,
    meter_scale: MeterScale,
    pub scene: VisualScene,
    max_waveform_points: usize,
    max_spectrum_points: usize,
    max_spectrogram_rows: usize,
    max_spectrogram_bands: usize,
    max_goniometer_points: usize,
}

impl VisualComposer {
    pub fn new(
        palette: Palette,
        max_waveform_points: usize,
        max_spectrum_points: usize,
        max_goniometer_points: usize,
    ) -> Self {
        Self::with_spectrogram(
            palette,
            max_waveform_points,
            max_spectrum_points,
            max_goniometer_points,
            DEFAULT_SPECTROGRAM_ROWS,
            DEFAULT_SPECTROGRAM_BANDS,
        )
    }

    /// Same as [`Self::new`] with an explicit bounded waterfall resolution.
    pub fn with_spectrogram(
        palette: Palette,
        max_waveform_points: usize,
        max_spectrum_points: usize,
        max_goniometer_points: usize,
        max_spectrogram_rows: usize,
        max_spectrogram_bands: usize,
    ) -> Self {
        // 14 grid lines + 4 meter peak lines + 2 vectorscope axes + two
        // envelope lines between every adjacent waveform point. Sources are
        // bounded before drawing, so this is a strict per-frame ceiling.
        let line_capacity =
            20usize.saturating_add(max_waveform_points.saturating_sub(1).saturating_mul(2));
        // Four meter rails, four active fills, LRA rail/fill, one bar per
        // display band, and one bounded waterfall cell per display bin.
        let quad_capacity = 10usize
            .saturating_add(max_spectrum_points)
            .saturating_add(max_spectrogram_rows.saturating_mul(max_spectrogram_bands));
        let dot_capacity = max_goniometer_points;
        Self {
            palette,
            meter_scale: MeterScale::default(),
            scene: VisualScene {
                clear: palette.night,
                lines: Vec::with_capacity(line_capacity),
                quads: Vec::with_capacity(quad_capacity),
                dots: Vec::with_capacity(dot_capacity),
            },
            max_waveform_points,
            max_spectrum_points,
            max_spectrogram_rows,
            max_spectrogram_bands,
            max_goniometer_points,
        }
    }

    pub fn palette(&self) -> Palette {
        self.palette
    }

    pub fn compose(&mut self, frame: VisualFrame<'_>) -> &VisualScene {
        self.scene.clear_keep_capacity(self.palette.night);
        self.draw_grid();
        self.draw_meters(frame.meter);
        self.draw_waveform(frame.waveform);
        self.draw_spectrum(frame.spectrum);
        self.draw_spectrogram(
            frame.spectrogram,
            frame.spectrogram_rows,
            frame.spectrogram_bands,
        );
        self.draw_goniometer(frame.goniometer, frame.meter.correlation);
        &self.scene
    }

    fn draw_grid(&mut self) {
        let grid = self.palette.grid.with_alpha(0.52);
        // Four deliberate rooms: meter, waveform, spectrum, goniometer.
        for x in [-0.94, -0.49, 0.23, 0.94] {
            self.scene.lines.push(Line {
                from: [x, -0.92],
                to: [x, 0.92],
                width: 0.003,
                color: grid,
            });
        }
        for y in [-0.88, -0.04, 0.08, 0.88] {
            self.scene.lines.push(Line {
                from: [-0.92, y],
                to: [0.92, y],
                width: 0.003,
                color: grid,
            });
        }
    }

    fn draw_meters(&mut self, meter: MeterReadings) {
        // Four adjacent runways: M, S, I, TP. Momentary and short-term use
        // loudness units; true peak uses dBFS, but both share a practical
        // display scale so a clipped peak is visually unmistakable.
        let levels = [
            meter.momentary_lufs,
            meter.short_term_lufs,
            meter.integrated_lufs,
            Some(meter.true_peak_dbfs),
        ];
        let colours = [
            self.palette.guava,
            self.palette.cobalt,
            self.palette.nickel,
            self.palette.violet,
        ];
        for (index, (level, color)) in levels.into_iter().zip(colours).enumerate() {
            let x0 = -0.90 + index as f32 * 0.105;
            let x1 = x0 + 0.070;
            self.scene.quads.push(Quad {
                min: [x0, -0.80],
                max: [x1, 0.80],
                color: self.palette.grid.with_alpha(0.28),
            });
            if let Some(db) = level {
                let top = -0.80 + self.meter_scale.unit(db) * 1.60;
                self.scene.quads.push(Quad {
                    min: [x0 + 0.010, -0.79],
                    max: [x1 - 0.010, top.max(-0.79)],
                    color,
                });
                self.scene.lines.push(Line {
                    from: [x0, top],
                    to: [x1, top],
                    width: 0.009,
                    color: color.mix(Color::rgb(1.0, 1.0, 1.0), 0.18),
                });
            }
        }
        // LRA is a range rather than another absolute loudness value. Its
        // short rail below the runways gives the native renderer a stable,
        // exact UI-thread view without pretending it belongs in RT telemetry.
        self.scene.quads.push(Quad {
            min: [-0.90, -0.90],
            max: [-0.52, -0.855],
            color: self.palette.grid.with_alpha(0.32),
        });
        if let Some(lra) = meter.lra_lu {
            let right = -0.90 + 0.38 * (lra / 20.0).clamp(0.0, 1.0);
            self.scene.quads.push(Quad {
                min: [-0.897, -0.897],
                max: [right.max(-0.896), -0.858],
                color: self.palette.guava.with_alpha(0.88),
            });
        }
    }

    fn draw_waveform(&mut self, waveform: &[MinMax]) {
        let samples = bounded_indices(waveform.len(), self.max_waveform_points);
        let count = samples.len();
        if count == 0 {
            return;
        }
        let mut previous_top = None;
        let mut previous_bottom = None;
        for (draw_index, source_index) in samples.enumerate() {
            let item = waveform[source_index];
            let x = WAVE_X0 + (WAVE_X1 - WAVE_X0) * position(draw_index, count);
            let top = WAVE_CENTER_Y + item.max.clamp(-1.0, 1.0) * 0.29;
            let bottom = WAVE_CENTER_Y + item.min.clamp(-1.0, 1.0) * 0.29;
            if let (Some(last_top), Some(last_bottom)) = (previous_top, previous_bottom) {
                self.scene.lines.push(Line {
                    from: last_top,
                    to: [x, top],
                    width: 0.006,
                    color: self.palette.nickel,
                });
                self.scene.lines.push(Line {
                    from: last_bottom,
                    to: [x, bottom],
                    width: 0.006,
                    color: self.palette.nickel.with_alpha(0.72),
                });
            }
            previous_top = Some([x, top]);
            previous_bottom = Some([x, bottom]);
        }
    }

    fn draw_spectrum(&mut self, spectrum: &[f32]) {
        let samples = bounded_indices(spectrum.len(), self.max_spectrum_points);
        let count = samples.len();
        for (draw_index, source_index) in samples.enumerate() {
            let amplitude = spectrum[source_index].clamp(0.0, 1.0);
            let x0 = SPECTRUM_X0 + (SPECTRUM_X1 - SPECTRUM_X0) * position(draw_index, count);
            let x1 = SPECTRUM_X0 + (SPECTRUM_X1 - SPECTRUM_X0) * position(draw_index + 1, count);
            self.scene.quads.push(Quad {
                min: [x0 + 0.003, 0.15],
                max: [x1 - 0.003, 0.15 + amplitude * 0.64],
                color: self
                    .palette
                    .cobalt
                    .mix(self.palette.violet, amplitude * 0.8),
            });
        }
    }

    fn draw_spectrogram(&mut self, values: &[f32], rows: usize, bands: usize) {
        if rows == 0 || bands == 0 {
            return;
        }
        let available_rows = rows.min(values.len() / bands);
        let row_indices = bounded_indices(available_rows, self.max_spectrogram_rows);
        let drawn_rows = row_indices.len();
        let drawn_bands = bands.min(self.max_spectrogram_bands);
        if drawn_rows == 0 || drawn_bands == 0 {
            return;
        }

        for (draw_row, source_row) in row_indices.enumerate() {
            let row = &values[source_row * bands..(source_row + 1) * bands];
            let y0 = -0.84 + 0.72 * position(draw_row, drawn_rows);
            let y1 = -0.84 + 0.72 * position(draw_row + 1, drawn_rows);
            for draw_band in 0..drawn_bands {
                let source_band = bounded_index(draw_band, drawn_bands, bands);
                let dbfs = row[source_band];
                let unit = if dbfs.is_finite() {
                    ((dbfs + 100.0) / 70.0).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let x0 = WAVE_X0 + (WAVE_X1 - WAVE_X0) * position(draw_band, drawn_bands);
                let x1 = WAVE_X0 + (WAVE_X1 - WAVE_X0) * position(draw_band + 1, drawn_bands);
                self.scene.quads.push(Quad {
                    min: [x0 + 0.001, y0 + 0.001],
                    max: [x1 - 0.001, y1 - 0.001],
                    color: self
                        .palette
                        .grid
                        .mix(self.palette.cobalt, unit * 0.75)
                        .mix(self.palette.guava, (unit - 0.68).max(0.0) / 0.32)
                        .with_alpha(0.92),
                });
            }
        }
    }

    fn draw_goniometer(&mut self, points: &[StereoPoint], correlation: f32) {
        // Center/right panel is a vector display: vertical is mono, diagonal
        // spread tells the eye how much side energy is present.
        self.scene.lines.push(Line {
            from: [GONIOMETER_CENTER[0], -0.84],
            to: [GONIOMETER_CENTER[0], -0.08],
            width: 0.004,
            color: self.palette.grid,
        });
        self.scene.lines.push(Line {
            from: [SPECTRUM_X0, GONIOMETER_CENTER[1]],
            to: [SPECTRUM_X1, GONIOMETER_CENTER[1]],
            width: 0.004,
            color: self.palette.grid,
        });

        let samples = bounded_indices(points.len(), self.max_goniometer_points);
        let stable = ((correlation.clamp(-1.0, 1.0) + 1.0) * 0.5).clamp(0.0, 1.0);
        let color = self
            .palette
            .guava
            .mix(self.palette.cobalt, stable)
            .with_alpha(0.38);
        for source_index in samples {
            let point = points[source_index];
            let side = ((point.left - point.right) / SQRT_2).clamp(-1.0, 1.0);
            let mid = ((point.left + point.right) / SQRT_2).clamp(-1.0, 1.0);
            self.scene.dots.push(Dot {
                position: [
                    GONIOMETER_CENTER[0] + side * 0.28,
                    GONIOMETER_CENTER[1] + mid * 0.28,
                ],
                radius: 0.007,
                color,
            });
        }
    }
}

/// Iterator over at most `limit` evenly distributed source indices.
fn bounded_indices(len: usize, limit: usize) -> impl ExactSizeIterator<Item = usize> {
    let count = len.min(limit);
    (0..count).map(move |index| bounded_index(index, count, len))
}

fn bounded_index(index: usize, count: usize, len: usize) -> usize {
    if count <= 1 || len <= count {
        index
    } else {
        index * (len - 1) / (count - 1)
    }
}

fn position(index: usize, count: usize) -> f32 {
    if count <= 1 {
        0.5
    } else {
        index as f32 / (count - 1) as f32
    }
}

/// WGSL shared by the first WGPU adapter. It contains no host-side state;
/// the adapter supplies camera/instance data. Keeping it here makes shader
/// review and fallback renderers use the exact same visual vocabulary.
pub const PRIMITIVE_WGSL: &str = r#"
struct VertexOutput {
  @builtin(position) position: vec4<f32>,
  @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(
  @builtin(vertex_index) vertex_index: u32,
  @location(0) a: vec2<f32>,
  @location(1) b: vec2<f32>,
  @location(2) c: vec2<f32>,
  @location(3) d: vec2<f32>,
  @location(4) color: vec4<f32>,
) -> VertexOutput {
  var position = a;
  switch vertex_index {
    case 0u: { position = a; }
    case 1u: { position = b; }
    case 2u: { position = c; }
    case 3u: { position = a; }
    case 4u: { position = c; }
    default: { position = d; }
  }
  var out: VertexOutput;
  out.position = vec4<f32>(position, 0.0, 1.0);
  out.color = color;
  return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
  return in.color;
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_scale_clamps_and_is_monotonic() {
        let scale = MeterScale::default();
        assert_eq!(scale.unit(f32::NAN), 0.0);
        assert_eq!(scale.unit(-100.0), 0.0);
        assert_eq!(scale.unit(10.0), 1.0);
        assert!(scale.unit(-23.0) > scale.unit(-40.0));
    }

    #[test]
    fn log_frequency_scale_round_trips() {
        let scale = LogFrequencyScale::default();
        for hz in [20.0, 63.0, 1_000.0, 7_500.0, 20_000.0] {
            let recovered = scale.hz_at_x(scale.x_for_hz(hz));
            assert!((recovered - hz).abs() / hz < 0.000_1, "{hz} -> {recovered}");
        }
    }

    #[test]
    fn composer_bounds_full_size_data_without_reallocating() {
        let mut composer = VisualComposer::new(Palette::NETA, 256, 96, 1_024);
        let initial_lines = composer.scene.lines.capacity();
        let initial_quads = composer.scene.quads.capacity();
        let initial_dots = composer.scene.dots.capacity();
        let waveform = [MinMax {
            min: -1.0,
            max: 1.0,
        }; 512];
        let spectrum = [0.5; 192];
        let spectrogram = [-42.0; 96 * 128];
        let goniometer = [StereoPoint {
            left: 0.5,
            right: -0.5,
        }; 2_048];
        let scene = composer.compose(VisualFrame {
            meter: MeterReadings {
                momentary_lufs: Some(-12.0),
                short_term_lufs: Some(-13.0),
                integrated_lufs: Some(-14.0),
                lra_lu: Some(8.0),
                true_peak_dbfs: -0.1,
                ..Default::default()
            },
            waveform: &waveform,
            spectrum: &spectrum,
            spectrogram: &spectrogram,
            spectrogram_rows: 128,
            spectrogram_bands: 96,
            goniometer: &goniometer,
        });
        assert!(scene.lines.len() <= initial_lines);
        assert!(scene.quads.len() <= initial_quads);
        assert!(scene.dots.len() <= initial_dots);
        assert_eq!(scene.dots.len(), 1_024);
    }

    #[test]
    fn goniometer_maps_mono_to_vertical_axis() {
        let mut composer = VisualComposer::new(Palette::NETA, 0, 0, 4);
        let scene = composer.compose(VisualFrame {
            goniometer: &[StereoPoint {
                left: 0.75,
                right: 0.75,
            }],
            ..VisualFrame::empty()
        });
        assert_eq!(scene.dots.len(), 1);
        assert!((scene.dots[0].position[0] - GONIOMETER_CENTER[0]).abs() < 0.000_1);
    }

    #[test]
    fn shader_has_both_stages() {
        assert!(PRIMITIVE_WGSL.contains("@vertex"));
        assert!(PRIMITIVE_WGSL.contains("@fragment"));
    }
}
