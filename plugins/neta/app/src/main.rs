//! Standalone native Neta instrument.
//!
//! Starts in an explicitly labelled demo programme until a platform system
//! capture backend is available. Its visual, analysis and bridge path are the
//! same path a capture callback will use.

#![forbid(unsafe_code)]

mod capture;
mod engine;
mod object;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use capture::{CaptureStream, InputPlan};
use engine::{SAMPLE_RATE, StandaloneEngine};
use neta_wgpu::SurfaceRenderer;
use object::ImportedObject;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowAttributes, WindowId};

const FRAME_INTERVAL: Duration = Duration::from_nanos(16_666_667);
const GPU_RECOVERY_INTERVAL: Duration = Duration::from_secs(1);

struct App {
    instance: wgpu::Instance,
    window: Option<Arc<Window>>,
    renderer: Option<SurfaceRenderer>,
    engine: StandaloneEngine,
    source: SourceMode,
    input_plan: Option<InputPlan>,
    _capture: Option<CaptureStream>,
    object: Option<ImportedObject>,
    last_frame: Instant,
    next_frame: Instant,
    next_gpu_recovery: Instant,
    occluded: bool,
    startup_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceMode {
    Demo,
    Input,
}

struct LaunchOptions {
    source: SourceMode,
    object_path: Option<PathBuf>,
}

impl App {
    fn new(options: LaunchOptions) -> Result<Self, String> {
        let source = options.source;
        let input_plan = match source {
            SourceMode::Demo => None,
            SourceMode::Input => Some(capture::prepare_default_input()?),
        };
        let sample_rate = input_plan
            .as_ref()
            .map(InputPlan::sample_rate)
            .unwrap_or(SAMPLE_RATE);
        Ok(Self {
            instance: wgpu::Instance::default(),
            window: None,
            renderer: None,
            engine: StandaloneEngine::new(sample_rate)?,
            source,
            input_plan,
            _capture: None,
            object: options
                .object_path
                .as_deref()
                .map(ImportedObject::load)
                .transpose()?,
            last_frame: Instant::now(),
            next_frame: Instant::now(),
            next_gpu_recovery: Instant::now(),
            occluded: false,
            startup_error: None,
        })
    }

    fn fail_startup(&mut self, event_loop: &ActiveEventLoop, message: impl Into<String>) {
        self.startup_error = Some(message.into());
        event_loop.exit();
    }

    fn create_window(&mut self, event_loop: &ActiveEventLoop) {
        let source_title = match self.source {
            SourceMode::Demo => "Neta — demo source",
            SourceMode::Input => "Neta — default input capture",
        };
        let title = self
            .object
            .as_ref()
            .map(|object| format!("{source_title} · object: {}", object.label()))
            .unwrap_or_else(|| source_title.to_owned());
        let attributes = WindowAttributes::default()
            .with_title(title)
            .with_inner_size(PhysicalSize::new(1_280, 760))
            .with_min_inner_size(PhysicalSize::new(800, 520));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => window,
            Err(error) => {
                self.fail_startup(event_loop, format!("could not create Neta window: {error}"));
                return;
            }
        };
        let window = Arc::new(window);
        self.window = Some(window);
        let renderer = match self.create_renderer() {
            Ok(renderer) => renderer,
            Err(error) => {
                self.fail_startup(event_loop, error.to_string());
                return;
            }
        };
        self.renderer = Some(renderer);
        self.last_frame = Instant::now();
        self.next_frame = self.last_frame;
        if let Some(plan) = self.input_plan.take() {
            let device_name = plan.device_name();
            let Some(producer) = self.engine.take_capture_producer() else {
                self.fail_startup(event_loop, "capture bridge was already taken".to_owned());
                return;
            };
            let stream = match plan.start(producer) {
                Ok(stream) => stream,
                Err(error) => {
                    self.fail_startup(event_loop, error.to_string());
                    return;
                }
            };
            if let Some(window) = &self.window {
                window.set_title(&format!("Neta — input: {device_name}"));
            }
            self._capture = Some(stream);
        }
    }

    fn create_renderer(&self) -> Result<SurfaceRenderer, String> {
        let window = self
            .window
            .as_ref()
            .ok_or_else(|| "could not create Neta GPU renderer without a window".to_owned())?;
        let size = window.inner_size();
        let surface = self
            .instance
            .create_surface(Arc::clone(window))
            .map_err(|error| format!("could not create Neta GPU surface: {error}"))?;
        pollster::block_on(SurfaceRenderer::new(
            &self.instance,
            surface,
            size.width,
            size.height,
        ))
        .map_err(|error| error.to_string())
    }

    fn redraw(&mut self) {
        let now = Instant::now();
        let delta = now.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = now;
        self.restore_renderer_if_due(now);
        if self
            ._capture
            .as_ref()
            .is_some_and(|capture| !capture.healthy())
        {
            if let Some(window) = &self.window {
                window.set_title("Neta — input capture error");
            }
        }
        match self.source {
            SourceMode::Demo => self.engine.step_demo(delta),
            SourceMode::Input => self.engine.consume_available(delta),
        }
        let object = self.object.as_ref();
        let engine = &mut self.engine;
        let (scene, dalia_geometry, dalia_colors, reactivity) = engine.render_parts();
        let (geometry, colors) = object
            .map(|object| (object.positions(), object.colors()))
            .unwrap_or((dalia_geometry, dalia_colors));
        let Some(renderer) = &mut self.renderer else {
            return;
        };
        let outcome = renderer.render_with_dalia(scene, geometry, colors, reactivity);
        if matches!(
            outcome,
            neta_wgpu::FrameResult::SurfaceLost | neta_wgpu::FrameResult::ValidationError
        ) {
            // A live Arc<Window> makes standalone recovery safe: the new
            // surface cannot outlive its native owner. Retry on the next
            // cadence tick, then no more often than once per second.
            self.renderer = None;
            self.next_gpu_recovery = now;
            if let Some(window) = &self.window {
                window.set_title("Neta — GPU recovery pending");
            }
        }
    }

    fn restore_renderer_if_due(&mut self, now: Instant) {
        if self.renderer.is_some() || now < self.next_gpu_recovery {
            return;
        }
        self.next_gpu_recovery = now + GPU_RECOVERY_INTERVAL;
        match self.create_renderer() {
            Ok(renderer) => self.renderer = Some(renderer),
            Err(_) => {
                if let Some(window) = &self.window {
                    window.set_title("Neta — GPU recovery pending");
                }
            }
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            self.create_window(event_loop);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size.width, size.height);
                }
            }
            WindowEvent::Occluded(occluded) => {
                self.occluded = occluded;
                if !occluded {
                    let now = Instant::now();
                    self.last_frame = now;
                    self.next_frame = now;
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state.is_pressed() => {
                if let winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::Space) =
                    event.physical_key
                {
                    self.engine.next_dalia_preset();
                }
            }
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.occluded {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        }
        let now = Instant::now();
        if now >= self.next_frame {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
            self.next_frame = now + FRAME_INTERVAL;
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_frame));
    }
}

fn launch_options() -> Result<LaunchOptions, String> {
    let mut options = LaunchOptions {
        source: SourceMode::Demo,
        object_path: None,
    };
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--demo" => options.source = SourceMode::Demo,
            "--input" => options.source = SourceMode::Input,
            "--object" => {
                let path = arguments
                    .next()
                    .ok_or_else(|| "--object needs a .obj, .gltf, or .glb path".to_owned())?;
                options.object_path = Some(PathBuf::from(path));
            }
            "--system-audio" => {
                return Err(
                    "system-audio tap is not wired yet; select a loopback device as default input and run `neta-app --input`".to_owned(),
                );
            }
            "--help" | "-h" => {
                return Err(
                    "usage: neta-app [--demo | --input] [--object FILE.obj|FILE.gltf|FILE.glb]"
                        .to_owned(),
                );
            }
            unknown => return Err(format!("unknown argument `{unknown}`; use --help")),
        }
    }
    Ok(options)
}

fn main() -> Result<(), String> {
    let event_loop = EventLoop::new().map_err(|error| error.to_string())?;
    let mut app = App::new(launch_options()?)?;
    let result = event_loop.run_app(&mut app);
    if let Some(error) = app.startup_error {
        return Err(error);
    }
    result.map_err(|error| error.to_string())
}
