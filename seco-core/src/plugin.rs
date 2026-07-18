use crate::{AudioBuffer, ParamDesc, RtContext};

/// A SECO audio plugin.
///
/// Implementations describe themselves through the associated constants and
/// process audio through [`Plugin::process`]. The trait knows nothing about
/// any host ABI.
///
/// `Send` is required because hosts may run consecutive audio callbacks on
/// different OS threads (the "audio thread" is a role, not a fixed thread).
/// `Sync` is *not* required: hosts guarantee an instance is never entered
/// from two threads at once.
pub trait Plugin: Send + 'static {
    /// Unique, stable identifier, reverse-URI style (e.g. `"dev.seco.patada"`).
    const ID: &'static str;
    /// Display name.
    const NAME: &'static str;
    /// Vendor string.
    const VENDOR: &'static str;
    /// Version string, e.g. `"0.1.0"`.
    const VERSION: &'static str;
    /// One-line description. May be empty.
    const DESCRIPTION: &'static str = "";
    /// The plugin's parameters. A parameter's identifier is its index here,
    /// so treat the slice as append-only (see [`ParamDesc`]). Current values
    /// arrive in `process()` via [`RtContext::param`].
    const PARAMS: &'static [ParamDesc] = &[];

    /// Creates an instance. Runs on the main thread; allocation is fine here.
    fn new() -> Self;

    /// Prepares for processing at `sample_rate`. Blocks passed to
    /// [`Plugin::process`] never exceed `max_frames` frames. Runs on the
    /// main thread; this is the place to pre-allocate.
    fn activate(&mut self, sample_rate: f64, max_frames: u32) {
        let _ = (sample_rate, max_frames);
    }

    /// Counterpart of [`Plugin::activate`].
    fn deactivate(&mut self) {}

    /// Clears all processing state (filters, envelopes, ...). Parameter
    /// values are unaffected.
    fn reset(&mut self) {}

    /// Processes one block of audio in place. Runs on the audio thread:
    /// no allocation, no locks, no I/O. `rt` witnesses that constraint and
    /// carries the block's context (e.g. `rt.transport()`) — see
    /// [`RtContext`].
    fn process(&mut self, audio: &mut AudioBuffer, rt: &RtContext);
}
