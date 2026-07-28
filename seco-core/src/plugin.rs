use crate::{AudioBuffer, EditorPage, ParamDesc, RtContext};

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
    /// Unique, stable identifier, reverse-URI style (e.g. `"dev.seco.zape"`).
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

    /// The plugin's editor, or `None` (the default) for no editor. The
    /// adapter only advertises `clap.gui` when this is `Some` — a plugin
    /// owns its own page; the framework owns the window, the lifecycle and
    /// the parameter plumbing. See [`EditorPage`].
    const EDITOR: Option<EditorPage> = None;

    /// Plugin-specific data for the editor, as JavaScript to evaluate in the
    /// page.
    ///
    /// Called on the main thread at the editor's refresh rate with the
    /// current plain parameter values (same order as [`Plugin::PARAMS`])
    /// and the current state block (the same bytes
    /// [`Plugin::apply_state`] receives, so the page and the audio draw
    /// from one source), never from `process()` — allocation is fine here.
    /// The adapter evaluates the snippet only when it differs from the last,
    /// so returning the same string every tick costs nothing on the wire.
    ///
    /// The parameter values themselves are already pushed by the adapter;
    /// this is for everything else the page needs to draw.
    fn editor_script(params: &[f64], state: &[u8]) -> Option<String> {
        let _ = (params, state);
        None
    }

    /// Applies a block of plugin-owned state — anything that does not fit
    /// in a parameter, such as a drawn curve.
    ///
    /// The adapter owns the block, not the plugin: both `clap.state`
    /// callbacks run on the main thread and may overlap `process()`, so
    /// asking a live plugin for its bytes there would be unsound. The block
    /// is therefore delivered *here*, on the audio thread, immediately
    /// before the `process()` call that follows a change — session load,
    /// preset change, or an editor edit.
    ///
    /// Same rules as [`Plugin::process`]: no allocation, no locks, no I/O.
    /// Parse into storage prepared in [`Plugin::activate`].
    ///
    /// An empty slice is a value, not "nothing happened": it means the
    /// session carried no block, and whatever was loaded before must be
    /// reset to defaults.
    ///
    /// Nothing calls this until a plugin has state to publish; the default
    /// ignores it.
    fn apply_state(&mut self, state: &[u8], rt: &RtContext) {
        let _ = (state, rt);
    }

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
