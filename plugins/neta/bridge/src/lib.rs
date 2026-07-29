//! Fixed-capacity audio bridge for Neta.
//!
//! [`AudioBlockRing`] builds one producer and one consumer for a bounded,
//! planar audio-block queue. It is an in-process core today; an OS
//! shared-memory mapping can use the same slot protocol later.
//!
//! The producer is intended for a plugin's audio thread. [`AudioBlockProducer::try_push`]
//! never allocates, locks, waits, or performs I/O. If every slot is busy it
//! returns [`PushError::Full`], so the producer can drop that block rather
//! than compromise real-time audio. The consumer supplies its own preallocated
//! planar destination to [`AudioBlockConsumer::try_pop_into`].
//!
//! Storage uses [`AtomicU32`] bit patterns rather than a non-atomic `f32`
//! buffer. This keeps concurrent transfer entirely safe Rust: producer writes
//! happen before a release publication of a slot; consumer reads happen after
//! an acquire observation. It costs one atomic load/store per sample. Neta's
//! desktop targets have native 32-bit and 64-bit atomics; a future carefully-audited
//! shared-memory backend may optimize the backing storage without changing
//! this API or its SPSC protocol.
//!
//! Build it with [`AudioBlockRing::new`] and consume it with
//! [`AudioBlockRing::into_endpoints`]. Exactly one [`AudioBlockProducer`] and
//! one [`AudioBlockConsumer`] result. Their transfer methods take `&mut self`,
//! and neither handle is clonable, preserving the single-producer/
//! single-consumer contract.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::cell::Cell;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Build-time configuration for an [`AudioBlockRing`].
///
/// All three limits are fixed after construction. Construction allocates all
/// backing storage, so transfer calls need no allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingConfig {
    /// Number of complete blocks the queue can hold concurrently.
    pub capacity: usize,
    /// Greatest number of planar channels in one block.
    pub max_channels: usize,
    /// Greatest number of frames in each channel of one block.
    pub max_frames: usize,
}

impl RingConfig {
    /// Creates configuration with fixed queue, channel, and frame limits.
    #[must_use]
    pub const fn new(capacity: usize, max_channels: usize, max_frames: usize) -> Self {
        Self {
            capacity,
            max_channels,
            max_frames,
        }
    }
}

/// Error returned when fixed backing storage cannot be configured.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RingConfigError {
    /// Queue capacity was zero.
    ZeroCapacity,
    /// Per-block channel limit was zero.
    ZeroMaxChannels,
    /// Per-channel frame limit was zero.
    ZeroMaxFrames,
    /// Backing-storage length overflowed `usize` or exceeded addressable allocation size.
    StorageSizeOverflow,
}

impl fmt::Display for RingConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroCapacity => {
                formatter.write_str("audio ring capacity must be greater than zero")
            }
            Self::ZeroMaxChannels => {
                formatter.write_str("audio ring maximum channel count must be greater than zero")
            }
            Self::ZeroMaxFrames => {
                formatter.write_str("audio ring maximum frame count must be greater than zero")
            }
            Self::StorageSizeOverflow => {
                formatter.write_str("audio ring backing storage size is not addressable")
            }
        }
    }
}

impl Error for RingConfigError {}

/// Stream metadata supplied with one audio block.
///
/// Channel and frame counts come from the planar source passed to
/// [`AudioBlockProducer::try_push`] and are returned in [`BlockInfo`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioBlockMeta {
    /// Absolute stream-frame position of first sample in this block.
    pub stream_frame: u64,
    /// Source sample rate in hertz. Must be nonzero.
    pub sample_rate_hz: u32,
}

impl AudioBlockMeta {
    /// Creates metadata for a block beginning at `stream_frame`.
    #[must_use]
    pub const fn new(stream_frame: u64, sample_rate_hz: u32) -> Self {
        Self {
            stream_frame,
            sample_rate_hz,
        }
    }
}

/// Description of a block successfully read or discarded from queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockInfo {
    /// Original stream metadata.
    pub meta: AudioBlockMeta,
    /// Number of planar channels in this block.
    pub channels: usize,
    /// Number of frames in every channel of this block.
    pub frames: usize,
}

/// Error returned by [`AudioBlockProducer::try_push`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PushError {
    /// Every ring slot is occupied. No source samples were copied.
    Full,
    /// Source contained no channels.
    NoChannels,
    /// Source channels contained no frames.
    NoFrames,
    /// Source channel count exceeded configured maximum.
    TooManyChannels {
        /// Number of source channels.
        provided: usize,
        /// Configured maximum channel count.
        maximum: usize,
    },
    /// Source frame count exceeded configured maximum.
    TooManyFrames {
        /// Number of frames in every source channel.
        provided: usize,
        /// Configured maximum frames per channel.
        maximum: usize,
    },
    /// Source channels did not all contain the same number of frames.
    UnevenChannelFrames {
        /// Index of channel with unexpected frame count.
        channel: usize,
        /// Required frame count, taken from first channel.
        expected: usize,
        /// Actual frame count in this channel.
        actual: usize,
    },
    /// Block metadata had a zero sample rate.
    ZeroSampleRate,
}

impl fmt::Display for PushError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => formatter.write_str("audio ring is full"),
            Self::NoChannels => formatter.write_str("audio block has no channels"),
            Self::NoFrames => formatter.write_str("audio block has no frames"),
            Self::TooManyChannels { provided, maximum } => write!(
                formatter,
                "audio block has {provided} channels; ring maximum is {maximum}"
            ),
            Self::TooManyFrames { provided, maximum } => write!(
                formatter,
                "audio block has {provided} frames; ring maximum is {maximum}"
            ),
            Self::UnevenChannelFrames {
                channel,
                expected,
                actual,
            } => write!(
                formatter,
                "audio block channel {channel} has {actual} frames; expected {expected}"
            ),
            Self::ZeroSampleRate => {
                formatter.write_str("audio block sample rate must be greater than zero")
            }
        }
    }
}

impl Error for PushError {}

/// Error returned by [`AudioBlockConsumer::try_pop_into`].
///
/// Unlike [`PushError::Full`], these errors leave ready block in queue so
/// caller can retry with sufficiently large preallocated destination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PopError {
    /// Destination did not expose enough planar channels.
    TooFewDestinationChannels {
        /// Number of channels stored in pending block.
        required: usize,
        /// Number of destination channels supplied by caller.
        provided: usize,
    },
    /// One destination channel was too short for pending block.
    DestinationChannelTooShort {
        /// Index of short destination channel.
        channel: usize,
        /// Number of frames stored in pending block.
        required: usize,
        /// Available destination frames.
        provided: usize,
    },
}

impl fmt::Display for PopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooFewDestinationChannels { required, provided } => write!(
                formatter,
                "audio destination has {provided} channels; pending block needs {required}"
            ),
            Self::DestinationChannelTooShort {
                channel,
                required,
                provided,
            } => write!(
                formatter,
                "audio destination channel {channel} has {provided} frames; pending block needs {required}"
            ),
        }
    }
}

impl Error for PopError {}

/// Fixed-capacity ring awaiting split into its producer and consumer.
pub struct AudioBlockRing {
    inner: Arc<Inner>,
}

impl AudioBlockRing {
    /// Allocates one SPSC block ring.
    ///
    /// This is only allocation point in bridge core. Use a configuration based
    /// on host's maximum callback block size before entering audio processing.
    pub fn new(config: RingConfig) -> Result<Self, RingConfigError> {
        validate_config(config)?;

        let cells_per_block = config
            .max_channels
            .checked_mul(config.max_frames)
            .ok_or(RingConfigError::StorageSizeOverflow)?;
        let sample_cell_count = config
            .capacity
            .checked_mul(cells_per_block)
            .ok_or(RingConfigError::StorageSizeOverflow)?;
        let max_addressable_bytes = isize::MAX as usize;
        if config.capacity > max_addressable_bytes / std::mem::size_of::<Slot>()
            || sample_cell_count > max_addressable_bytes / std::mem::size_of::<AtomicU32>()
        {
            return Err(RingConfigError::StorageSizeOverflow);
        }

        let slots = (0..config.capacity).map(Slot::new).collect::<Box<[_]>>();
        let samples = (0..sample_cell_count)
            .map(|_| AtomicU32::new(0))
            .collect::<Box<[_]>>();
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                cells_per_block,
                slots,
                samples,
            }),
        })
    }

    /// Consumes ring factory and returns its one unique producer/consumer pair.
    #[must_use]
    pub fn into_endpoints(self) -> (AudioBlockProducer, AudioBlockConsumer) {
        (
            AudioBlockProducer {
                inner: Arc::clone(&self.inner),
                write_sequence: 0,
                // `Cell` makes endpoint `!Sync` but keeps it `Send`: one
                // handle can move to one audio thread, never be shared by two.
                _not_sync: std::marker::PhantomData,
            },
            AudioBlockConsumer {
                inner: self.inner,
                read_sequence: 0,
                _not_sync: std::marker::PhantomData,
            },
        )
    }
}

/// Unique SPSC producer endpoint, intended for plugin audio thread.
pub struct AudioBlockProducer {
    inner: Arc<Inner>,
    write_sequence: usize,
    _not_sync: std::marker::PhantomData<Cell<()>>,
}

impl AudioBlockProducer {
    /// Returns immutable queue limits selected during construction.
    #[must_use]
    pub fn config(&self) -> RingConfig {
        self.inner.config
    }

    /// Attempts to copy planar source block into next free slot.
    ///
    /// `source` is planar: every entry is one channel, and every channel must
    /// have equal nonzero length. Data are copied bit-for-bit, including NaN
    /// payloads. The method has no allocation, locking, waiting, or I/O.
    ///
    /// On [`PushError::Full`], no input is copied. Invalid source shape also
    /// leaves queue untouched.
    pub fn try_push(&mut self, meta: AudioBlockMeta, source: &[&[f32]]) -> Result<(), PushError> {
        let (channels, frames) = self.validate_source(meta, source)?;
        let slot_index = self.slot_index(self.write_sequence);
        let slot = &self.inner.slots[slot_index];

        if slot.sequence.load(Ordering::Acquire) != self.write_sequence {
            return Err(PushError::Full);
        }

        for (channel_index, channel) in source.iter().enumerate() {
            let start = self.sample_start(slot_index, channel_index);
            for (frame_index, sample) in channel.iter().enumerate() {
                self.inner.samples[start + frame_index].store(sample.to_bits(), Ordering::Relaxed);
            }
        }

        slot.stream_frame
            .store(meta.stream_frame, Ordering::Relaxed);
        slot.sample_rate_hz
            .store(meta.sample_rate_hz, Ordering::Relaxed);
        slot.channels.store(channels, Ordering::Relaxed);
        slot.frames.store(frames, Ordering::Relaxed);

        // Publish every relaxed payload/header write as one ready block.
        slot.sequence
            .store(self.write_sequence.wrapping_add(1), Ordering::Release);
        self.write_sequence = self.write_sequence.wrapping_add(1);
        Ok(())
    }

    fn validate_source(
        &self,
        meta: AudioBlockMeta,
        source: &[&[f32]],
    ) -> Result<(usize, usize), PushError> {
        if meta.sample_rate_hz == 0 {
            return Err(PushError::ZeroSampleRate);
        }
        if source.is_empty() {
            return Err(PushError::NoChannels);
        }
        if source.len() > self.inner.config.max_channels {
            return Err(PushError::TooManyChannels {
                provided: source.len(),
                maximum: self.inner.config.max_channels,
            });
        }

        let frames = source[0].len();
        if frames == 0 {
            return Err(PushError::NoFrames);
        }
        if frames > self.inner.config.max_frames {
            return Err(PushError::TooManyFrames {
                provided: frames,
                maximum: self.inner.config.max_frames,
            });
        }
        for (channel, samples) in source.iter().enumerate().skip(1) {
            if samples.len() != frames {
                return Err(PushError::UnevenChannelFrames {
                    channel,
                    expected: frames,
                    actual: samples.len(),
                });
            }
        }

        Ok((source.len(), frames))
    }

    #[inline]
    fn slot_index(&self, sequence: usize) -> usize {
        sequence % self.inner.config.capacity
    }

    #[inline]
    fn sample_start(&self, slot_index: usize, channel_index: usize) -> usize {
        slot_index * self.inner.cells_per_block + channel_index * self.inner.config.max_frames
    }
}

/// Unique SPSC consumer endpoint, intended for standalone app or editor.
pub struct AudioBlockConsumer {
    inner: Arc<Inner>,
    read_sequence: usize,
    _not_sync: std::marker::PhantomData<Cell<()>>,
}

impl AudioBlockConsumer {
    /// Returns immutable queue limits selected during construction.
    #[must_use]
    pub fn config(&self) -> RingConfig {
        self.inner.config
    }

    /// Returns metadata for next ready block without consuming it.
    ///
    /// Useful for sizing an app-owned destination. Returned data remain valid
    /// until this consumer pops or discards it; producer cannot overwrite a
    /// ready slot.
    #[must_use]
    pub fn peek_info(&self) -> Option<BlockInfo> {
        let slot = self.pending_slot()?;
        Some(block_info(slot))
    }

    /// Copies next ready block into caller-owned planar storage.
    ///
    /// `destination` may contain more channels or frames than needed; only
    /// first block-sized region is written. A destination-size error leaves
    /// pending block in ring for retry. `Ok(None)` means queue was empty.
    /// This method does not allocate, lock, wait, or perform I/O.
    pub fn try_pop_into(
        &mut self,
        destination: &mut [&mut [f32]],
    ) -> Result<Option<BlockInfo>, PopError> {
        let slot_index = self.slot_index(self.read_sequence);
        let slot = &self.inner.slots[slot_index];
        if slot.sequence.load(Ordering::Acquire) != self.read_sequence.wrapping_add(1) {
            return Ok(None);
        }

        let info = block_info(slot);
        validate_destination(info, destination)?;

        for (channel_index, output) in destination.iter_mut().take(info.channels).enumerate() {
            let start = self.sample_start(slot_index, channel_index);
            for (frame_index, sample) in output.iter_mut().take(info.frames).enumerate() {
                *sample =
                    f32::from_bits(self.inner.samples[start + frame_index].load(Ordering::Relaxed));
            }
        }

        self.release_slot(slot_index);
        Ok(Some(info))
    }

    /// Drops next ready block and returns its description.
    ///
    /// Use when consumer deliberately favors fresh real-time data over every
    /// historic block. `None` means queue was empty.
    pub fn try_discard(&mut self) -> Option<BlockInfo> {
        let slot_index = self.slot_index(self.read_sequence);
        let slot = &self.inner.slots[slot_index];
        if slot.sequence.load(Ordering::Acquire) != self.read_sequence.wrapping_add(1) {
            return None;
        }

        let info = block_info(slot);
        self.release_slot(slot_index);
        Some(info)
    }

    #[inline]
    fn pending_slot(&self) -> Option<&Slot> {
        let slot = &self.inner.slots[self.slot_index(self.read_sequence)];
        (slot.sequence.load(Ordering::Acquire) == self.read_sequence.wrapping_add(1))
            .then_some(slot)
    }

    #[inline]
    fn slot_index(&self, sequence: usize) -> usize {
        sequence % self.inner.config.capacity
    }

    #[inline]
    fn sample_start(&self, slot_index: usize, channel_index: usize) -> usize {
        slot_index * self.inner.cells_per_block + channel_index * self.inner.config.max_frames
    }

    #[inline]
    fn release_slot(&mut self, slot_index: usize) {
        // Let producer reuse slot only after all sample reads above completed.
        self.inner.slots[slot_index].sequence.store(
            self.read_sequence.wrapping_add(self.inner.config.capacity),
            Ordering::Release,
        );
        self.read_sequence = self.read_sequence.wrapping_add(1);
    }
}

struct Inner {
    config: RingConfig,
    cells_per_block: usize,
    slots: Box<[Slot]>,
    samples: Box<[AtomicU32]>,
}

// Keep hot publication state isolated from neighboring slots where practical.
#[repr(align(64))]
struct Slot {
    sequence: AtomicUsize,
    stream_frame: AtomicU64,
    sample_rate_hz: AtomicU32,
    channels: AtomicUsize,
    frames: AtomicUsize,
}

impl Slot {
    fn new(sequence: usize) -> Self {
        Self {
            sequence: AtomicUsize::new(sequence),
            stream_frame: AtomicU64::new(0),
            sample_rate_hz: AtomicU32::new(0),
            channels: AtomicUsize::new(0),
            frames: AtomicUsize::new(0),
        }
    }
}

fn validate_config(config: RingConfig) -> Result<(), RingConfigError> {
    if config.capacity == 0 {
        return Err(RingConfigError::ZeroCapacity);
    }
    if config.max_channels == 0 {
        return Err(RingConfigError::ZeroMaxChannels);
    }
    if config.max_frames == 0 {
        return Err(RingConfigError::ZeroMaxFrames);
    }
    Ok(())
}

#[inline]
fn block_info(slot: &Slot) -> BlockInfo {
    BlockInfo {
        meta: AudioBlockMeta {
            stream_frame: slot.stream_frame.load(Ordering::Relaxed),
            sample_rate_hz: slot.sample_rate_hz.load(Ordering::Relaxed),
        },
        channels: slot.channels.load(Ordering::Relaxed),
        frames: slot.frames.load(Ordering::Relaxed),
    }
}

fn validate_destination(info: BlockInfo, destination: &[&mut [f32]]) -> Result<(), PopError> {
    if destination.len() < info.channels {
        return Err(PopError::TooFewDestinationChannels {
            required: info.channels,
            provided: destination.len(),
        });
    }
    for (channel, output) in destination.iter().take(info.channels).enumerate() {
        if output.len() < info.frames {
            return Err(PopError::DestinationChannelTooShort {
                channel,
                required: info.frames,
                provided: output.len(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    const CONFIG: RingConfig = RingConfig::new(2, 2, 8);

    fn endpoints() -> (AudioBlockProducer, AudioBlockConsumer) {
        AudioBlockRing::new(CONFIG)
            .expect("valid ring config")
            .into_endpoints()
    }

    #[test]
    fn config_rejects_zero_limits_and_overflow() {
        assert!(matches!(
            AudioBlockRing::new(RingConfig::new(0, 1, 1)),
            Err(RingConfigError::ZeroCapacity)
        ));
        assert!(matches!(
            AudioBlockRing::new(RingConfig::new(1, 0, 1)),
            Err(RingConfigError::ZeroMaxChannels)
        ));
        assert!(matches!(
            AudioBlockRing::new(RingConfig::new(1, 1, 0)),
            Err(RingConfigError::ZeroMaxFrames)
        ));
        assert!(matches!(
            AudioBlockRing::new(RingConfig::new(usize::MAX, 2, 1)),
            Err(RingConfigError::StorageSizeOverflow)
        ));
        assert!(matches!(
            AudioBlockRing::new(RingConfig::new(usize::MAX, 1, 1)),
            Err(RingConfigError::StorageSizeOverflow)
        ));
    }

    #[test]
    fn transfers_planar_block_in_fifo_order() {
        let (mut producer, mut consumer) = endpoints();
        let left = [0.25, -0.5, 1.0, -1.0];
        let right = [-0.25, 0.5, -1.0, 1.0];
        let source: [&[f32]; 2] = [&left, &right];
        let meta = AudioBlockMeta::new(128, 48_000);

        producer.try_push(meta, &source).unwrap();
        assert_eq!(
            consumer.peek_info(),
            Some(BlockInfo {
                meta,
                channels: 2,
                frames: 4,
            })
        );

        let mut output_left = [99.0; 8];
        let mut output_right = [99.0; 8];
        let mut destination: [&mut [f32]; 2] = [&mut output_left, &mut output_right];
        let info = consumer.try_pop_into(&mut destination).unwrap();

        assert_eq!(info.unwrap().meta, meta);
        assert_eq!(&output_left[..4], left);
        assert_eq!(&output_right[..4], right);
        assert_eq!(&output_left[4..], &[99.0; 4]);
        assert_eq!(&output_right[4..], &[99.0; 4]);
        assert_eq!(consumer.peek_info(), None);
    }

    #[test]
    fn full_queue_never_overwrites_oldest_block() {
        let (mut producer, mut consumer) = endpoints();
        let first = [1.0, 1.0];
        let second = [2.0, 2.0];
        let third = [3.0, 3.0];

        producer
            .try_push(AudioBlockMeta::new(0, 48_000), &[&first])
            .unwrap();
        producer
            .try_push(AudioBlockMeta::new(2, 48_000), &[&second])
            .unwrap();
        assert_eq!(
            producer.try_push(AudioBlockMeta::new(4, 48_000), &[&third]),
            Err(PushError::Full)
        );

        let mut output = [0.0; 8];
        let first_info = {
            let mut destination: [&mut [f32]; 1] = [&mut output];
            consumer.try_pop_into(&mut destination).unwrap().unwrap()
        };
        assert_eq!(first_info.meta.stream_frame, 0);
        assert_eq!(&output[..2], first);

        producer
            .try_push(AudioBlockMeta::new(4, 48_000), &[&third])
            .unwrap();
        let second_info = {
            let mut destination: [&mut [f32]; 1] = [&mut output];
            consumer.try_pop_into(&mut destination).unwrap().unwrap()
        };
        assert_eq!(second_info.meta.stream_frame, 2);
        assert_eq!(&output[..2], second);
        let third_info = {
            let mut destination: [&mut [f32]; 1] = [&mut output];
            consumer.try_pop_into(&mut destination).unwrap().unwrap()
        };
        assert_eq!(third_info.meta.stream_frame, 4);
        assert_eq!(&output[..2], third);
    }

    #[test]
    fn input_validation_leaves_queue_untouched() {
        let (mut producer, consumer) = endpoints();
        let too_many_channels = [[0.0; 2]; 3];
        let uneven_left = [0.0; 2];
        let uneven_right = [0.0; 3];

        assert_eq!(
            producer.try_push(AudioBlockMeta::new(0, 0), &[&[0.0]]),
            Err(PushError::ZeroSampleRate)
        );
        assert_eq!(
            producer.try_push(AudioBlockMeta::new(0, 48_000), &[]),
            Err(PushError::NoChannels)
        );
        assert_eq!(
            producer.try_push(AudioBlockMeta::new(0, 48_000), &[&[]]),
            Err(PushError::NoFrames)
        );
        assert_eq!(
            producer.try_push(
                AudioBlockMeta::new(0, 48_000),
                &[
                    &too_many_channels[0],
                    &too_many_channels[1],
                    &too_many_channels[2]
                ],
            ),
            Err(PushError::TooManyChannels {
                provided: 3,
                maximum: 2,
            })
        );
        assert_eq!(
            producer.try_push(
                AudioBlockMeta::new(0, 48_000),
                &[&uneven_left, &uneven_right],
            ),
            Err(PushError::UnevenChannelFrames {
                channel: 1,
                expected: 2,
                actual: 3,
            })
        );
        assert_eq!(consumer.peek_info(), None);
    }

    #[test]
    fn destination_error_keeps_pending_block_for_retry() {
        let (mut producer, mut consumer) = endpoints();
        let left = [0.25, 0.5];
        let right = [0.75, 1.0];
        let meta = AudioBlockMeta::new(64, 44_100);
        producer.try_push(meta, &[&left, &right]).unwrap();

        let mut only_channel = [0.0; 2];
        let mut too_small_destination: [&mut [f32]; 1] = [&mut only_channel];
        assert_eq!(
            consumer.try_pop_into(&mut too_small_destination),
            Err(PopError::TooFewDestinationChannels {
                required: 2,
                provided: 1,
            })
        );
        assert_eq!(consumer.peek_info().unwrap().meta, meta);

        let mut output_left = [0.0; 2];
        let mut output_right = [0.0; 2];
        let mut valid_destination: [&mut [f32]; 2] = [&mut output_left, &mut output_right];
        assert_eq!(
            consumer
                .try_pop_into(&mut valid_destination)
                .unwrap()
                .unwrap()
                .meta,
            meta
        );
        assert_eq!(output_left, left);
        assert_eq!(output_right, right);
    }

    #[test]
    fn preserves_all_f32_bits() {
        let (mut producer, mut consumer) = endpoints();
        let source = [
            f32::from_bits(0x7f80_0001),
            -0.0,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ];
        producer
            .try_push(AudioBlockMeta::new(0, 48_000), &[&source])
            .unwrap();

        let mut output = [0.0; 4];
        let mut destination: [&mut [f32]; 1] = [&mut output];
        consumer.try_pop_into(&mut destination).unwrap();
        for (actual, expected) in output.into_iter().zip(source) {
            assert_eq!(actual.to_bits(), expected.to_bits());
        }
    }

    #[test]
    fn concurrent_stress_keeps_every_block_in_order() {
        const BLOCKS: usize = 20_000;
        const FRAMES: usize = 16;
        let (mut producer, mut consumer) = AudioBlockRing::new(RingConfig::new(8, 2, FRAMES))
            .unwrap()
            .into_endpoints();

        let producer_thread = thread::spawn(move || {
            let mut left = [0.0_f32; FRAMES];
            let mut right = [0.0_f32; FRAMES];
            for block in 0..BLOCKS {
                for frame in 0..FRAMES {
                    left[frame] = block as f32 + frame as f32 / 100.0;
                    right[frame] = -left[frame];
                }
                loop {
                    match producer.try_push(
                        AudioBlockMeta::new((block * FRAMES) as u64, 48_000),
                        &[&left, &right],
                    ) {
                        Ok(()) => break,
                        Err(PushError::Full) => thread::yield_now(),
                        Err(error) => panic!("unexpected push error: {error}"),
                    }
                }
            }
        });

        let consumer_thread = thread::spawn(move || {
            let mut left = [0.0_f32; FRAMES];
            let mut right = [0.0_f32; FRAMES];
            for block in 0..BLOCKS {
                let info = loop {
                    let result = {
                        let mut destination: [&mut [f32]; 2] = [&mut left, &mut right];
                        consumer.try_pop_into(&mut destination)
                    };
                    match result {
                        Ok(Some(info)) => break info,
                        Ok(None) => thread::yield_now(),
                        Err(error) => panic!("unexpected pop error: {error}"),
                    }
                };
                assert_eq!(info.meta.stream_frame, (block * FRAMES) as u64);
                assert_eq!(info.meta.sample_rate_hz, 48_000);
                for frame in 0..FRAMES {
                    let expected = block as f32 + frame as f32 / 100.0;
                    assert_eq!(left[frame], expected);
                    assert_eq!(right[frame], -expected);
                }
            }
        });

        producer_thread.join().unwrap();
        consumer_thread.join().unwrap();
    }
}
