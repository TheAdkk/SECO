/// A borrowed block of channel-major audio, processed in place.
///
/// What you read from a channel is the input; what you leave behind is the
/// output. The adapter layer guarantees every channel slice holds exactly
/// [`frames`](AudioBuffer::frames) samples.
///
/// The type only borrows: it cannot allocate, grow, or outlive the audio
/// callback that created it.
pub struct AudioBuffer<'a> {
    channels: &'a mut [&'a mut [f32]],
    frames: usize,
}

impl<'a> AudioBuffer<'a> {
    /// Wraps borrowed channel slices.
    ///
    /// `frames()` becomes the length of the shortest slice; well-behaved
    /// callers pass slices of equal length.
    pub fn new(channels: &'a mut [&'a mut [f32]]) -> Self {
        let frames = channels.iter().map(|ch| ch.len()).min().unwrap_or(0);
        debug_assert!(
            channels.iter().all(|ch| ch.len() == frames),
            "channel slices must have equal length"
        );
        Self { channels, frames }
    }

    /// Number of frames (samples per channel) in this block.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Number of channels.
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// Iterates over the channels as read-only sample slices — for a plugin
    /// that needs to look at the block before changing it (metering, a
    /// picture of the input for its editor).
    pub fn channels(&self) -> impl Clone + ExactSizeIterator<Item = &[f32]> {
        self.channels.iter().map(|channel| &**channel)
    }

    /// Iterates over the channels as mutable sample slices.
    pub fn channels_mut(&mut self) -> impl Iterator<Item = &mut [f32]> {
        self.channels.iter_mut().map(|ch| &mut **ch)
    }
}
