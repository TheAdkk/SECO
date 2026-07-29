//! Native WGPU renderer for [`neta_visual::VisualScene`].
//!
//! The crate has two layers:
//!
//! - [`PrimitiveRenderer`] draws a renderer-independent scene into any WGPU
//!   texture view. It is usable by a standalone window, an offscreen test, or
//!   a plugin host surface.
//! - [`SurfaceRenderer`] owns a safe WGPU surface for a standalone window.
//!
//! DSP, analysis, and scene composition remain in safe crates. The sole
//! `unsafe` operation is [`create_foreign_surface`], isolated for hosts that
//! only hand a plugin raw native window handle. Standalone windows never need
//! it because `wgpu::Instance::create_surface` is safe for a live `Window`.

#![deny(unsafe_op_in_unsafe_fn)]

use std::borrow::Cow;
use std::error::Error;
use std::fmt;

use neta_visual::{Color, Dot, Line, PRIMITIVE_WGSL, Quad, VisualScene};
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};

// Four arbitrary quad corners plus RGBA. GPU synthesizes six triangle
// vertices per instance, avoiding sixfold CPU staging/write traffic.
const INSTANCE_BYTES: u64 = 12 * std::mem::size_of::<f32>() as u64;
// Full scope/waterfall is about 4.7k quads; Dalia adds 12k point quads.
const DEFAULT_MAX_PRIMITIVES: usize = 20_000;

/// Errors while acquiring a usable GPU/device/surface.
#[derive(Debug)]
pub enum InitError {
    Surface(wgpu::CreateSurfaceError),
    Adapter(wgpu::RequestAdapterError),
    Device(wgpu::RequestDeviceError),
    UnsupportedSurface,
}

impl fmt::Display for InitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Surface(error) => write!(formatter, "could not create Neta GPU surface: {error}"),
            Self::Adapter(error) => write!(formatter, "could not select Neta GPU adapter: {error}"),
            Self::Device(error) => write!(formatter, "could not create Neta GPU device: {error}"),
            Self::UnsupportedSurface => {
                formatter.write_str("selected GPU cannot present to this surface")
            }
        }
    }
}

impl Error for InitError {}

/// What a surface draw did. Visual frames are disposable: a skip is a normal
/// frame-pacing outcome, never an audio error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameResult {
    Presented,
    PresentedAndReconfigured,
    Skipped,
    Reconfigured,
    SurfaceLost,
    ValidationError,
}

/// Raw native handles supplied by a plugin host.
///
/// This is intentionally a value instead of a trait object: the CLAP adapter
/// can validate its platform parent handle once, then pass a narrow record to
/// WGPU without exposing FFI pointers to the visual or meter crates.
#[derive(Clone, Copy, Debug)]
pub struct ForeignSurfaceHandles {
    pub display: Option<RawDisplayHandle>,
    pub window: RawWindowHandle,
}

/// Creates a WGPU surface for a host-owned native child view/window.
///
/// # Safety
///
/// `handles` must describe a live native surface that remains valid for every
/// use of the returned `Surface`, and all platform thread-affinity rules must
/// be honoured. The caller must drop the surface before the host destroys or
/// reparents that view. This is the only Neta-owned unsafe boundary for host
/// GPU embedding; use `Instance::create_surface(window)` for normal windows.
pub unsafe fn create_foreign_surface<'window>(
    instance: &wgpu::Instance,
    handles: ForeignSurfaceHandles,
) -> Result<wgpu::Surface<'window>, wgpu::CreateSurfaceError> {
    // SAFETY: upheld by this function's explicit contract. The raw handles
    // are copied into WGPU; their underlying host objects outlive the surface.
    unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: handles.display,
            raw_window_handle: handles.window,
        })
    }
}

/// A reusable GPU primitive encoder. CPU staging memory is allocated at
/// construction and reused for each UI frame; it is never touched by audio.
pub struct PrimitiveRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    instances: wgpu::Buffer,
    staging: Vec<u8>,
    max_primitives: usize,
}

impl PrimitiveRenderer {
    /// Builds renderer resources for one target format.
    pub fn new(device: wgpu::Device, queue: wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        Self::with_capacity(device, queue, format, DEFAULT_MAX_PRIMITIVES)
    }

    /// Same as [`Self::new`] with a fixed maximum primitive count. Overfull
    /// scenes are sampled/truncated at a primitive boundary, never allocating
    /// in the frame path.
    pub fn with_capacity(
        device: wgpu::Device,
        queue: wgpu::Queue,
        format: wgpu::TextureFormat,
        max_primitives: usize,
    ) -> Self {
        let max_primitives = max_primitives.max(1);
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("neta primitive shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(PRIMITIVE_WGSL)),
        });
        let attributes = [
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 0,
                shader_location: 0,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 8,
                shader_location: 1,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 16,
                shader_location: 2,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 24,
                shader_location: 3,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: 32,
                shader_location: 4,
            },
        ];
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: INSTANCE_BYTES,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &attributes,
        };
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("neta primitive pipeline"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[vertex_layout],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("neta primitive instances"),
            size: INSTANCE_BYTES * max_primitives as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            device,
            queue,
            pipeline,
            instances,
            staging: Vec::with_capacity(max_primitives * INSTANCE_BYTES as usize),
            max_primitives,
        }
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// Renders a scene into `view`; caller owns presentation and surface
    /// recovery. UI thread only.
    pub fn render_to_view(&mut self, scene: &VisualScene, view: &wgpu::TextureView) {
        self.render_to_view_with_dalia(scene, view, &[], &[], 0.0);
    }

    /// Renders the Neta scene plus an optional Dalia point cloud. Positions
    /// are `xyz` triplets and colours are RGB triplets. Partial/malformed
    /// tails are ignored safely.
    pub fn render_to_view_with_dalia(
        &mut self,
        scene: &VisualScene,
        view: &wgpu::TextureView,
        positions: &[f32],
        colors: &[f32],
        reactivity: f32,
    ) {
        let primitive_count = self.encode(scene, positions, colors, reactivity);
        if primitive_count > 0 {
            self.queue.write_buffer(&self.instances, 0, &self.staging);
        }
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("neta scene encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("neta scene pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(to_wgpu_color(scene.clear)),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if primitive_count > 0 {
                pass.set_pipeline(&self.pipeline);
                pass.set_vertex_buffer(0, self.instances.slice(..));
                pass.draw(0..6, 0..primitive_count as u32);
            }
        }
        self.queue.submit(Some(encoder.finish()));
    }

    fn encode(
        &mut self,
        scene: &VisualScene,
        positions: &[f32],
        colors: &[f32],
        reactivity: f32,
    ) -> usize {
        self.staging.clear();
        for quad in &scene.quads {
            self.push_quad(*quad);
        }
        for line in &scene.lines {
            self.push_line(*line);
        }
        for dot in &scene.dots {
            self.push_dot(*dot);
        }
        self.push_dalia_points(positions, colors, reactivity);
        self.staging.len() / INSTANCE_BYTES as usize
    }

    fn push_dalia_points(&mut self, positions: &[f32], colors: &[f32], reactivity: f32) {
        let reactivity = if reactivity.is_finite() {
            reactivity.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let motion_scale = 1.0 + reactivity * 0.32;
        let point_count = (positions.len() / 3).min(colors.len() / 3);
        let used_primitives = self.staging.len() / INSTANCE_BYTES as usize;
        let available_points = self.max_primitives.saturating_sub(used_primitives);
        let drawn_points = point_count.min(available_points);
        for draw_index in 0..drawn_points {
            // If a caller exceeds fixed capacity, retain an even sample
            // across the cloud rather than only its earliest vertices.
            let source_index = evenly_spaced_index(draw_index, drawn_points, point_count);
            let offset = source_index * 3;
            let [x, y, z] = [
                positions[offset],
                positions[offset + 1],
                positions[offset + 2],
            ];
            let depth = (1.65 + z).clamp(0.35, 3.0);
            let projected = [
                0.48 + x * 0.33 * motion_scale / depth,
                0.415 + y * 0.33 * motion_scale / depth,
            ];
            let radius = (0.0035 * (1.0 + reactivity * 0.8) / depth).clamp(0.0015, 0.011);
            self.push_dot(Dot {
                position: projected,
                radius,
                color: Color {
                    r: colors[offset],
                    g: colors[offset + 1],
                    b: colors[offset + 2],
                    a: 0.72,
                },
            });
        }
    }

    fn push_line(&mut self, line: Line) {
        let dx = line.to[0] - line.from[0];
        let dy = line.to[1] - line.from[1];
        let length = (dx * dx + dy * dy).sqrt();
        if !length.is_finite() || length <= f32::EPSILON || !line.width.is_finite() {
            return;
        }
        let radius = line.width.abs() * 0.5;
        let nx = -dy / length * radius;
        let ny = dx / length * radius;
        self.push_polygon(
            [line.from[0] + nx, line.from[1] + ny],
            [line.to[0] + nx, line.to[1] + ny],
            [line.to[0] - nx, line.to[1] - ny],
            [line.from[0] - nx, line.from[1] - ny],
            line.color,
        );
    }

    fn push_dot(&mut self, dot: Dot) {
        if !dot.radius.is_finite() {
            return;
        }
        let radius = dot.radius.abs();
        self.push_quad(Quad {
            min: [dot.position[0] - radius, dot.position[1] - radius],
            max: [dot.position[0] + radius, dot.position[1] + radius],
            color: dot.color,
        });
    }

    fn push_quad(&mut self, quad: Quad) {
        self.push_polygon(
            quad.min,
            [quad.max[0], quad.min[1]],
            quad.max,
            [quad.min[0], quad.max[1]],
            quad.color,
        );
    }

    fn push_polygon(&mut self, a: [f32; 2], b: [f32; 2], c: [f32; 2], d: [f32; 2], color: Color) {
        if self.staging.len() / INSTANCE_BYTES as usize >= self.max_primitives {
            return;
        }
        if !valid_point(a)
            || !valid_point(b)
            || !valid_point(c)
            || !valid_point(d)
            || !valid_color(color)
        {
            return;
        }
        for point in [a, b, c, d] {
            self.push_point(point);
        }
        self.push_color(color);
    }

    fn push_point(&mut self, position: [f32; 2]) {
        for value in position {
            self.staging.extend_from_slice(&value.to_ne_bytes());
        }
    }

    fn push_color(&mut self, color: Color) {
        for value in [color.r, color.g, color.b, color.a] {
            self.staging.extend_from_slice(&value.to_ne_bytes());
        }
    }
}

/// A standalone-window renderer. It accepts safe `Window` surfaces created by
/// the application and handles expected swap-chain recovery states.
pub struct SurfaceRenderer {
    surface: wgpu::Surface<'static>,
    primitives: PrimitiveRenderer,
    config: wgpu::SurfaceConfiguration,
}

impl SurfaceRenderer {
    /// Creates device, queue, pipeline, and a configured surface.
    pub async fn new(
        instance: &wgpu::Instance,
        surface: wgpu::Surface<'static>,
        width: u32,
        height: u32,
    ) -> Result<Self, InitError> {
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: Some(&surface),
            })
            .await
            .map_err(InitError::Adapter)?;
        if !adapter.is_surface_supported(&surface) {
            return Err(InitError::UnsupportedSurface);
        }
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("neta GPU device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                memory_hints: wgpu::MemoryHints::Performance,
                trace: wgpu::Trace::Off,
            })
            .await
            .map_err(InitError::Device)?;
        let mut config = surface
            .get_default_config(&adapter, width.max(1), height.max(1))
            .ok_or(InitError::UnsupportedSurface)?;
        config.desired_maximum_frame_latency = 1;
        surface.configure(&device, &config);
        let primitives = PrimitiveRenderer::new(device, queue, config.format);
        Ok(Self {
            surface,
            primitives,
            config,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface
            .configure(self.primitives.device(), &self.config);
    }

    pub fn render(&mut self, scene: &VisualScene) -> FrameResult {
        self.render_with_dalia(scene, &[], &[], 0.0)
    }

    /// Presents a visual scene with Dalia's native generated point cloud.
    /// Empty slices draw the meter/scope scene alone.
    pub fn render_with_dalia(
        &mut self,
        scene: &VisualScene,
        positions: &[f32],
        colors: &[f32],
        reactivity: f32,
    ) -> FrameResult {
        match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => {
                self.render_frame(scene, positions, colors, reactivity, frame);
                FrameResult::Presented
            }
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                self.render_frame(scene, positions, colors, reactivity, frame);
                self.surface
                    .configure(self.primitives.device(), &self.config);
                FrameResult::PresentedAndReconfigured
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                FrameResult::Skipped
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface
                    .configure(self.primitives.device(), &self.config);
                FrameResult::Reconfigured
            }
            wgpu::CurrentSurfaceTexture::Lost => FrameResult::SurfaceLost,
            wgpu::CurrentSurfaceTexture::Validation => FrameResult::ValidationError,
        }
    }

    fn render_frame(
        &mut self,
        scene: &VisualScene,
        positions: &[f32],
        colors: &[f32],
        reactivity: f32,
        frame: wgpu::SurfaceTexture,
    ) {
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        self.primitives
            .render_to_view_with_dalia(scene, &view, positions, colors, reactivity);
        frame.present();
    }
}

fn evenly_spaced_index(index: usize, output_len: usize, input_len: usize) -> usize {
    if output_len <= 1 || input_len <= output_len {
        index
    } else {
        index * (input_len - 1) / (output_len - 1)
    }
}

fn valid_point(point: [f32; 2]) -> bool {
    point[0].is_finite() && point[1].is_finite()
}

fn valid_color(color: Color) -> bool {
    color.r.is_finite() && color.g.is_finite() && color.b.is_finite() && color.a.is_finite()
}

fn to_wgpu_color(color: Color) -> wgpu::Color {
    let finite = |value: f32| {
        if value.is_finite() {
            value.clamp(0.0, 1.0) as f64
        } else {
            0.0
        }
    };
    wgpu::Color {
        r: finite(color.r),
        g: finite(color.g),
        b: finite(color.b),
        a: finite(color.a),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use neta_visual::{Palette, Quad};

    use super::*;

    #[test]
    fn scene_colors_sanitize_non_finite_values() {
        let color = to_wgpu_color(Color {
            r: f32::NAN,
            g: 2.0,
            b: -1.0,
            a: 1.0,
        });
        assert_eq!(color.r, 0.0);
        assert_eq!(color.g, 1.0);
        assert_eq!(color.b, 0.0);
        assert_eq!(color.a, 1.0);
        assert!(valid_color(Palette::NETA.cobalt));
    }

    #[test]
    fn invalid_geometry_is_rejected_before_gpu_upload() {
        assert!(!valid_point([f32::INFINITY, 0.0]));
        assert!(!valid_color(Color::rgb(f32::NAN, 0.0, 0.0)));
    }

    #[test]
    fn offscreen_renderer_writes_non_clear_pixels() {
        const WIDTH: u32 = 64;
        const HEIGHT: u32 = 64;
        let instance = wgpu::Instance::default();
        let adapter =
            match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                force_fallback_adapter: false,
                compatible_surface: None,
            })) {
                Ok(adapter) => adapter,
                // Some minimal CI containers expose no GPU or software adapter.
                // Platform render CI still runs this test where an adapter exists.
                Err(error) => {
                    eprintln!("skipping offscreen WGPU smoke test: {error}");
                    return;
                }
            };
        let (device, queue) =
            match pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("neta offscreen test device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                trace: wgpu::Trace::Off,
            })) {
                Ok(pair) => pair,
                Err(error) => {
                    eprintln!("skipping offscreen WGPU smoke test: {error}");
                    return;
                }
            };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("neta offscreen target"),
            size: wgpu::Extent3d {
                width: WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut renderer = PrimitiveRenderer::new(device, queue, wgpu::TextureFormat::Rgba8Unorm);
        let scene = VisualScene {
            clear: Palette::NETA.night,
            lines: Vec::new(),
            quads: vec![Quad {
                min: [-0.6, -0.6],
                max: [0.6, 0.6],
                color: Palette::NETA.guava,
            }],
            dots: Vec::new(),
        };
        renderer.render_to_view(&scene, &view);

        let readback = renderer.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("neta offscreen readback"),
            size: u64::from(WIDTH * HEIGHT * 4),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder =
            renderer
                .device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("neta offscreen copy"),
                });
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(WIDTH * 4),
                    rows_per_image: Some(HEIGHT),
                },
            },
            texture.size(),
        );
        renderer.queue().submit(Some(encoder.finish()));

        let (sender, receiver) = mpsc::channel();
        readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.send(result);
            });
        renderer
            .device()
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("offscreen GPU poll should succeed");
        receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("offscreen map callback should run")
            .expect("offscreen map should succeed");
        let mapped = readback.slice(..).get_mapped_range();
        assert!(
            mapped
                .chunks_exact(4)
                .any(|pixel| pixel[0] > 180 && pixel[1] > 30 && pixel[2] > 20),
            "rendered image contained only clear pixels"
        );
        drop(mapped);
        readback.unmap();
    }
}
