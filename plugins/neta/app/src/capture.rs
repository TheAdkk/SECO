//! Safe default-input capture for standalone Neta.
//!
//! CPAL gives Neta microphone, interface, or virtual-loopback input on
//! macOS/CoreAudio, Windows/WASAPI, and Linux backends. It does not pretend a
//! default input is a privileged system-output tap: platform system-audio
//! capture remains a separate source mode (ScreenCaptureKit / WASAPI loopback
//! / PipeWire portal). A loopback device such as BlackHole can be selected as
//! the OS default input today without changing Neta's audio path.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use neta_bridge::{AudioBlockMeta, AudioBlockProducer, PushError};

use crate::engine::BLOCK_FRAMES;

const CAPTURE_OK: u32 = 0;
const CAPTURE_ERROR: u32 = 1;

/// Prepared device/configuration. Preparation is control-thread work; the
/// eventual callback owns only fixed buffers and a bridge producer.
pub struct InputPlan {
    device: cpal::Device,
    config: cpal::SupportedStreamConfig,
}

impl InputPlan {
    pub fn sample_rate(&self) -> u32 {
        self.config.sample_rate()
    }

    pub fn device_name(&self) -> String {
        self.device.to_string()
    }

    pub fn start(self, producer: AudioBlockProducer) -> Result<CaptureStream, String> {
        let sample_format = self.config.sample_format();
        let config: cpal::StreamConfig = self.config.into();
        let channels = usize::from(config.channels.max(1));
        let status = Arc::new(AtomicU32::new(CAPTURE_OK));
        let stream = match sample_format {
            cpal::SampleFormat::F32 => build_stream::<f32>(
                &self.device,
                config,
                channels,
                producer,
                Arc::clone(&status),
            ),
            cpal::SampleFormat::F64 => build_stream::<f64>(
                &self.device,
                config,
                channels,
                producer,
                Arc::clone(&status),
            ),
            cpal::SampleFormat::I8 => build_stream::<i8>(
                &self.device,
                config,
                channels,
                producer,
                Arc::clone(&status),
            ),
            cpal::SampleFormat::I16 => build_stream::<i16>(
                &self.device,
                config,
                channels,
                producer,
                Arc::clone(&status),
            ),
            cpal::SampleFormat::I24 => build_stream::<cpal::I24>(
                &self.device,
                config,
                channels,
                producer,
                Arc::clone(&status),
            ),
            cpal::SampleFormat::I32 => build_stream::<i32>(
                &self.device,
                config,
                channels,
                producer,
                Arc::clone(&status),
            ),
            cpal::SampleFormat::I64 => build_stream::<i64>(
                &self.device,
                config,
                channels,
                producer,
                Arc::clone(&status),
            ),
            cpal::SampleFormat::U8 => build_stream::<u8>(
                &self.device,
                config,
                channels,
                producer,
                Arc::clone(&status),
            ),
            cpal::SampleFormat::U16 => build_stream::<u16>(
                &self.device,
                config,
                channels,
                producer,
                Arc::clone(&status),
            ),
            cpal::SampleFormat::U24 => build_stream::<cpal::U24>(
                &self.device,
                config,
                channels,
                producer,
                Arc::clone(&status),
            ),
            cpal::SampleFormat::U32 => build_stream::<u32>(
                &self.device,
                config,
                channels,
                producer,
                Arc::clone(&status),
            ),
            cpal::SampleFormat::U64 => build_stream::<u64>(
                &self.device,
                config,
                channels,
                producer,
                Arc::clone(&status),
            ),
            unsupported => return Err(format!("unsupported input sample format {unsupported}")),
        }?;
        stream.play().map_err(|error| error.to_string())?;
        Ok(CaptureStream {
            _stream: stream,
            status,
        })
    }
}

/// Picks the current OS default input. This is explicit rather than hidden:
/// macOS may ask for microphone access and a virtual loopback is a user choice.
pub fn prepare_default_input() -> Result<InputPlan, String> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| "no default audio input device is available".to_owned())?;
    let config = device
        .default_input_config()
        .map_err(|error| error.to_string())?;
    Ok(InputPlan { device, config })
}

/// Kept alive by the app for as long as the capture should run.
pub struct CaptureStream {
    _stream: cpal::Stream,
    status: Arc<AtomicU32>,
}

impl CaptureStream {
    pub fn healthy(&self) -> bool {
        self.status.load(Relaxed) == CAPTURE_OK
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    producer: AudioBlockProducer,
    status: Arc<AtomicU32>,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let sample_rate = config.sample_rate;
    let mut ingress = CaptureIngress::new(producer, sample_rate, channels);
    device
        .build_input_stream(
            config,
            move |data: &[T], _| ingress.push_interleaved(data),
            move |_error| status.store(CAPTURE_ERROR, Relaxed),
            None,
        )
        .map_err(|error| error.to_string())
}

/// One CPAL callback's safe handoff state. Its only dynamic data lives inside
/// `AudioBlockProducer`, which was fully allocated before the stream started.
struct CaptureIngress {
    producer: AudioBlockProducer,
    left: [f32; BLOCK_FRAMES],
    right: [f32; BLOCK_FRAMES],
    filled: usize,
    stream_frame: u64,
    sample_rate: u32,
    channels: usize,
}

impl CaptureIngress {
    fn new(producer: AudioBlockProducer, sample_rate: u32, channels: usize) -> Self {
        Self {
            producer,
            left: [0.0; BLOCK_FRAMES],
            right: [0.0; BLOCK_FRAMES],
            filled: 0,
            stream_frame: 0,
            sample_rate,
            channels,
        }
    }

    fn push_interleaved<T>(&mut self, data: &[T])
    where
        T: cpal::SizedSample,
        f32: cpal::FromSample<T>,
    {
        for frame in data.chunks_exact(self.channels) {
            let left = frame[0].to_sample::<f32>();
            let right = frame.get(1).copied().unwrap_or(frame[0]).to_sample::<f32>();
            self.left[self.filled] = sanitize(left);
            self.right[self.filled] = sanitize(right);
            self.filled += 1;
            if self.filled == BLOCK_FRAMES {
                self.flush();
            }
        }
    }

    fn flush(&mut self) {
        if self.filled == 0 {
            return;
        }
        let source: [&[f32]; 2] = [&self.left[..self.filled], &self.right[..self.filled]];
        let meta = AudioBlockMeta::new(self.stream_frame, self.sample_rate);
        // A full queue is normal under UI/GPU stalls. Drop this newest block;
        // never wait in capture. A later bridge policy may discard oldest if
        // the consumer can coordinate that choice safely.
        match self.producer.try_push(meta, &source) {
            Ok(()) | Err(PushError::Full) => {}
            Err(error) => debug_assert!(false, "capture producer contract failed: {error}"),
        }
        self.stream_frame = self.stream_frame.saturating_add(self.filled as u64);
        self.filled = 0;
    }
}

fn sanitize(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use neta_bridge::{AudioBlockRing, RingConfig};

    use super::*;

    #[test]
    fn ingress_chunks_stereo_without_allocating_or_reordering() {
        let (producer, mut consumer) = AudioBlockRing::new(RingConfig::new(2, 2, BLOCK_FRAMES))
            .unwrap()
            .into_endpoints();
        let mut ingress = CaptureIngress::new(producer, 48_000, 2);
        let mut input = Vec::with_capacity(BLOCK_FRAMES * 2);
        for index in 0..BLOCK_FRAMES {
            input.extend([
                index as f32 / BLOCK_FRAMES as f32,
                -(index as f32 / BLOCK_FRAMES as f32),
            ]);
        }
        ingress.push_interleaved(&input);
        let mut left = [0.0; BLOCK_FRAMES];
        let mut right = [0.0; BLOCK_FRAMES];
        let mut output: [&mut [f32]; 2] = [&mut left, &mut right];
        let info = consumer.try_pop_into(&mut output).unwrap().unwrap();
        assert_eq!(info.frames, BLOCK_FRAMES);
        assert_eq!(left[0], 0.0);
        assert!(right[BLOCK_FRAMES - 1] < -0.99);
    }
}
