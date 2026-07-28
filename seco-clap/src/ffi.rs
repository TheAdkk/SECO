//! Hand-written mirrors of the CLAP C ABI.
//!
//! Source of truth: `reference/clap` @ `195b42a` (tag 1.2.10). Every item
//! cites the header it mirrors (paths relative to `include/clap/`).
//!
//! Transcription rules:
//! - Enums are transcribed **complete and in declaration order**, never
//!   partially — omitting one member would silently shift every value after
//!   it. Same discipline for struct fields (order defines the ABI).
//! - Types get Rust names (`clap_plugin_entry` → `ClapPluginEntry`);
//!   constants keep their exact C names.
//! - `CLAP_ABI` is `__cdecl` on Windows and empty elsewhere
//!   (`private/macros.h:20-26`), which is exactly Rust's `extern "C"`.
//! - C `bool` is `_Bool`; Rust guarantees `bool` is ABI-compatible with it.

use std::ffi::{CStr, c_char, c_void};

// ------------------------------------------------------------- version.h

/// `clap_version_t` — `version.h:10-17`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ClapVersion {
    pub major: u32,
    pub minor: u32,
    pub revision: u32,
}

/// `CLAP_VERSION` — `version.h:23-36`. The header revision these mirrors
/// were transcribed from.
pub const CLAP_VERSION: ClapVersion = ClapVersion { major: 1, minor: 2, revision: 10 };

// ------------------------------------------------------------------ id.h

/// `clap_id` — `id.h:6`.
pub type ClapId = u32;

/// `CLAP_INVALID_ID` — `id.h:8`.
pub const CLAP_INVALID_ID: ClapId = u32::MAX;

// -------------------------------------------------------- string-sizes.h

/// String size constants — `string-sizes.h:7-17` (complete, in order).
pub const CLAP_NAME_SIZE: usize = 256;
/// See [`CLAP_NAME_SIZE`].
pub const CLAP_PATH_SIZE: usize = 1024;

// -------------------------------------------------------------- entry.h

/// `clap_plugin_entry_t` — `entry.h:61-129`.
///
/// The single symbol a CLAP dynamic library exports, named `clap_entry`
/// (`entry.h:132`).
#[repr(C)]
pub struct ClapPluginEntry {
    pub clap_version: ClapVersion,
    pub init: unsafe extern "C" fn(plugin_path: *const c_char) -> bool,
    pub deinit: unsafe extern "C" fn(),
    pub get_factory: unsafe extern "C" fn(factory_id: *const c_char) -> *const c_void,
}

// ---------------------------------------------- factory/plugin-factory.h

/// `CLAP_PLUGIN_FACTORY_ID` — `factory/plugin-factory.h:7`.
pub const CLAP_PLUGIN_FACTORY_ID: &CStr = c"clap.plugin-factory";

/// `clap_plugin_factory_t` — `factory/plugin-factory.h:18-39`.
/// All methods are `[thread-safe]`.
#[repr(C)]
pub struct ClapPluginFactory {
    pub get_plugin_count: unsafe extern "C" fn(factory: *const ClapPluginFactory) -> u32,
    pub get_plugin_descriptor: unsafe extern "C" fn(
        factory: *const ClapPluginFactory,
        index: u32,
    ) -> *const ClapPluginDescriptor,
    pub create_plugin: unsafe extern "C" fn(
        factory: *const ClapPluginFactory,
        host: *const ClapHost,
        plugin_id: *const c_char,
    ) -> *const ClapPlugin,
}

// --------------------------------------------------------------- host.h

/// `clap_host_t` — `host.h:9-47`. Phase 1 stores the pointer without calling
/// through it; the full mirror is here so the layout is right from day one.
#[repr(C)]
pub struct ClapHost {
    pub clap_version: ClapVersion,
    pub host_data: *mut c_void,
    pub name: *const c_char,
    pub vendor: *const c_char,
    pub url: *const c_char,
    pub version: *const c_char,
    pub get_extension:
        unsafe extern "C" fn(host: *const ClapHost, extension_id: *const c_char) -> *const c_void,
    pub request_restart: unsafe extern "C" fn(host: *const ClapHost),
    pub request_process: unsafe extern "C" fn(host: *const ClapHost),
    pub request_callback: unsafe extern "C" fn(host: *const ClapHost),
}

// ------------------------------------------------------- plugin-features.h

/// `plugin-features.h:19`. Feature strings are independent `#define`s (not an
/// enum — no ordering hazard), so only the ones SECO declares are mirrored.
pub const CLAP_PLUGIN_FEATURE_AUDIO_EFFECT: &CStr = c"audio-effect";
/// `plugin-features.h:77`.
pub const CLAP_PLUGIN_FEATURE_STEREO: &CStr = c"stereo";

// -------------------------------------------------------------- plugin.h

/// `clap_plugin_descriptor_t` — `plugin.h:12-39`. `id` and `name` are
/// mandatory; the other strings may be blank. `features` is a
/// null-terminated array of pointers.
#[repr(C)]
pub struct ClapPluginDescriptor {
    pub clap_version: ClapVersion,
    pub id: *const c_char,
    pub name: *const c_char,
    pub vendor: *const c_char,
    pub url: *const c_char,
    pub manual_url: *const c_char,
    pub support_url: *const c_char,
    pub version: *const c_char,
    pub description: *const c_char,
    pub features: *const *const c_char,
}

/// `clap_plugin_t` — `plugin.h:41-110`. Thread rules per method are quoted
/// where each callback is implemented (`instance.rs`).
#[repr(C)]
pub struct ClapPlugin {
    pub desc: *const ClapPluginDescriptor,
    pub plugin_data: *mut c_void,
    pub init: unsafe extern "C" fn(plugin: *const ClapPlugin) -> bool,
    pub destroy: unsafe extern "C" fn(plugin: *const ClapPlugin),
    pub activate: unsafe extern "C" fn(
        plugin: *const ClapPlugin,
        sample_rate: f64,
        min_frames_count: u32,
        max_frames_count: u32,
    ) -> bool,
    pub deactivate: unsafe extern "C" fn(plugin: *const ClapPlugin),
    pub start_processing: unsafe extern "C" fn(plugin: *const ClapPlugin) -> bool,
    pub stop_processing: unsafe extern "C" fn(plugin: *const ClapPlugin),
    pub reset: unsafe extern "C" fn(plugin: *const ClapPlugin),
    pub process: unsafe extern "C" fn(
        plugin: *const ClapPlugin,
        process: *const ClapProcess,
    ) -> ClapProcessStatus,
    pub get_extension:
        unsafe extern "C" fn(plugin: *const ClapPlugin, id: *const c_char) -> *const c_void,
    pub on_main_thread: unsafe extern "C" fn(plugin: *const ClapPlugin),
}

// ---------------------------------------------------------- fixedpoint.h

/// `clap_beattime` — `fixedpoint.h:15`. Q32.31 fixed point (see factor).
pub type ClapBeattime = i64;
/// `clap_sectime` — `fixedpoint.h:16`.
pub type ClapSectime = i64;

/// `CLAP_BEATTIME_FACTOR` — `fixedpoint.h:12`. "This will never change."
pub const CLAP_BEATTIME_FACTOR: i64 = 1 << 31;
/// `CLAP_SECTIME_FACTOR` — `fixedpoint.h:13`.
pub const CLAP_SECTIME_FACTOR: i64 = 1 << 31;

// -------------------------------------------------------------- events.h

/// `clap_event_header_t` — `events.h:18-24`. Prefix of every event.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ClapEventHeader {
    pub size: u32,
    pub time: u32,
    pub space_id: u16,
    /// `type` in C; renamed because `type` is a Rust keyword.
    pub type_: u16,
    pub flags: u32,
}

/// `CLAP_CORE_EVENT_SPACE_ID` — `events.h:27`.
pub const CLAP_CORE_EVENT_SPACE_ID: u16 = 0;

/// `clap_event_flags` — `events.h:29-39` (complete, in order). These live in
/// `ClapEventHeader::flags`, *not* in `ClapEventTransport::flags`.
pub const CLAP_EVENT_IS_LIVE: u32 = 1 << 0;
/// See [`CLAP_EVENT_IS_LIVE`].
pub const CLAP_EVENT_DONT_RECORD: u32 = 1 << 1;

/// Core event types — `events.h:53-122` (complete, in order). Typed `u16` to
/// compare directly against `ClapEventHeader::type_`.
pub const CLAP_EVENT_NOTE_ON: u16 = 0;
/// See [`CLAP_EVENT_NOTE_ON`].
pub const CLAP_EVENT_NOTE_OFF: u16 = 1;
/// See [`CLAP_EVENT_NOTE_ON`].
pub const CLAP_EVENT_NOTE_CHOKE: u16 = 2;
/// See [`CLAP_EVENT_NOTE_ON`].
pub const CLAP_EVENT_NOTE_END: u16 = 3;
/// See [`CLAP_EVENT_NOTE_ON`].
pub const CLAP_EVENT_NOTE_EXPRESSION: u16 = 4;
/// See [`CLAP_EVENT_NOTE_ON`].
pub const CLAP_EVENT_PARAM_VALUE: u16 = 5;
/// See [`CLAP_EVENT_NOTE_ON`].
pub const CLAP_EVENT_PARAM_MOD: u16 = 6;
/// See [`CLAP_EVENT_NOTE_ON`].
pub const CLAP_EVENT_PARAM_GESTURE_BEGIN: u16 = 7;
/// See [`CLAP_EVENT_NOTE_ON`].
pub const CLAP_EVENT_PARAM_GESTURE_END: u16 = 8;
/// See [`CLAP_EVENT_NOTE_ON`].
pub const CLAP_EVENT_TRANSPORT: u16 = 9;
/// See [`CLAP_EVENT_NOTE_ON`].
pub const CLAP_EVENT_MIDI: u16 = 10;
/// See [`CLAP_EVENT_NOTE_ON`].
pub const CLAP_EVENT_MIDI_SYSEX: u16 = 11;
/// See [`CLAP_EVENT_NOTE_ON`].
pub const CLAP_EVENT_MIDI2: u16 = 12;

/// `clap_transport_flags` — `events.h:263-272` (complete, in order).
pub const CLAP_TRANSPORT_HAS_TEMPO: u32 = 1 << 0;
/// See [`CLAP_TRANSPORT_HAS_TEMPO`].
pub const CLAP_TRANSPORT_HAS_BEATS_TIMELINE: u32 = 1 << 1;
/// See [`CLAP_TRANSPORT_HAS_TEMPO`].
pub const CLAP_TRANSPORT_HAS_SECONDS_TIMELINE: u32 = 1 << 2;
/// See [`CLAP_TRANSPORT_HAS_TEMPO`].
pub const CLAP_TRANSPORT_HAS_TIME_SIGNATURE: u32 = 1 << 3;
/// See [`CLAP_TRANSPORT_HAS_TEMPO`].
pub const CLAP_TRANSPORT_IS_PLAYING: u32 = 1 << 4;
/// See [`CLAP_TRANSPORT_HAS_TEMPO`].
pub const CLAP_TRANSPORT_IS_RECORDING: u32 = 1 << 5;
/// See [`CLAP_TRANSPORT_HAS_TEMPO`].
pub const CLAP_TRANSPORT_IS_LOOP_ACTIVE: u32 = 1 << 6;
/// See [`CLAP_TRANSPORT_HAS_TEMPO`].
pub const CLAP_TRANSPORT_IS_WITHIN_PRE_ROLL: u32 = 1 << 7;

/// `clap_event_transport_t` — `events.h:280-302`. Delivered both via
/// `ClapProcess::transport` (state at sample 0) and as an in-event
/// (`CLAP_EVENT_TRANSPORT`) for mid-block changes.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ClapEventTransport {
    pub header: ClapEventHeader,
    pub flags: u32,
    pub song_pos_beats: ClapBeattime,
    pub song_pos_seconds: ClapSectime,
    pub tempo: f64,
    pub tempo_inc: f64,
    pub loop_start_beats: ClapBeattime,
    pub loop_end_beats: ClapBeattime,
    pub loop_start_seconds: ClapSectime,
    pub loop_end_seconds: ClapSectime,
    pub bar_start: ClapBeattime,
    pub bar_number: i32,
    pub tsig_num: u16,
    pub tsig_denom: u16,
}

/// `clap_event_param_value_t` — `events.h:222-237`. Sets a parameter's
/// value. `cookie` may be null (`ext/params.h:237-239`); the note-targeting
/// fields are `-1` for a global (non-polyphonic) change.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ClapEventParamValue {
    pub header: ClapEventHeader,
    pub param_id: ClapId,
    pub cookie: *mut c_void,
    pub note_id: i32,
    pub port_index: i16,
    pub channel: i16,
    pub key: i16,
    pub value: f64,
}

/// `clap_event_param_gesture_t` — `events.h:256-261`. Marks the beginning
/// or end of a user gesture on a parameter (types
/// `CLAP_EVENT_PARAM_GESTURE_BEGIN`/`_END`); improves host automation
/// touch/latch behavior (events.h:111-114).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ClapEventParamGesture {
    pub header: ClapEventHeader,
    pub param_id: ClapId,
}

/// `clap_input_events_t` — `events.h:344-353`. Host-sorted by sample time.
#[repr(C)]
pub struct ClapInputEvents {
    pub ctx: *mut c_void,
    pub size: unsafe extern "C" fn(list: *const ClapInputEvents) -> u32,
    pub get: unsafe extern "C" fn(
        list: *const ClapInputEvents,
        index: u32,
    ) -> *const ClapEventHeader,
}

/// `clap_output_events_t` — `events.h:355-363`. `try_push` copies the event.
#[repr(C)]
pub struct ClapOutputEvents {
    pub ctx: *mut c_void,
    pub try_push: unsafe extern "C" fn(
        list: *const ClapOutputEvents,
        event: *const ClapEventHeader,
    ) -> bool,
}

// -------------------------------------------------------- audio-buffer.h

/// `clap_audio_buffer_t` — `audio-buffer.h:26-33`. Exactly one of
/// `data32`/`data64` is set. `constant_mask` is a hint; the buffer is still
/// fully filled (`audio-buffer.h:19-25`).
#[repr(C)]
pub struct ClapAudioBuffer {
    pub data32: *mut *mut f32,
    pub data64: *mut *mut f64,
    pub channel_count: u32,
    pub latency: u32,
    pub constant_mask: u64,
}

// ------------------------------------------------------------- process.h

/// `clap_process_status` — `process.h:10-28` (complete, in order).
pub type ClapProcessStatus = i32;
/// Processing failed; output must be discarded.
pub const CLAP_PROCESS_ERROR: ClapProcessStatus = 0;
/// Keep processing.
pub const CLAP_PROCESS_CONTINUE: ClapProcessStatus = 1;
/// Keep processing while the output is not quiet.
pub const CLAP_PROCESS_CONTINUE_IF_NOT_QUIET: ClapProcessStatus = 2;
/// Use the tail extension to decide.
pub const CLAP_PROCESS_TAIL: ClapProcessStatus = 3;
/// No more processing needed until the next event or input change.
pub const CLAP_PROCESS_SLEEP: ClapProcessStatus = 4;

/// `clap_process_t` — `process.h:30-62`.
#[repr(C)]
pub struct ClapProcess {
    /// Steady sample counter; `-1` if unavailable (`process.h:31-38`).
    pub steady_time: i64,
    pub frames_count: u32,
    /// Transport at sample 0; NULL means free-running host
    /// (`process.h:43-45`).
    pub transport: *const ClapEventTransport,
    pub audio_inputs: *const ClapAudioBuffer,
    pub audio_outputs: *mut ClapAudioBuffer,
    pub audio_inputs_count: u32,
    pub audio_outputs_count: u32,
    pub in_events: *const ClapInputEvents,
    pub out_events: *const ClapOutputEvents,
}

// ---------------------------------------------------- ext/audio-ports.h

/// `CLAP_EXT_AUDIO_PORTS` — `ext/audio-ports.h:16`.
pub const CLAP_EXT_AUDIO_PORTS: &CStr = c"clap.audio-ports";
/// `CLAP_PORT_MONO` — `ext/audio-ports.h:17`.
pub const CLAP_PORT_MONO: &CStr = c"mono";
/// `CLAP_PORT_STEREO` — `ext/audio-ports.h:18`.
pub const CLAP_PORT_STEREO: &CStr = c"stereo";

/// Audio port flags — `ext/audio-ports.h:24-40` (complete, in order).
pub const CLAP_AUDIO_PORT_IS_MAIN: u32 = 1 << 0;
/// See [`CLAP_AUDIO_PORT_IS_MAIN`].
pub const CLAP_AUDIO_PORT_SUPPORTS_64BITS: u32 = 1 << 1;
/// See [`CLAP_AUDIO_PORT_IS_MAIN`].
pub const CLAP_AUDIO_PORT_PREFERS_64BITS: u32 = 1 << 2;
/// See [`CLAP_AUDIO_PORT_IS_MAIN`].
pub const CLAP_AUDIO_PORT_REQUIRES_COMMON_SAMPLE_SIZE: u32 = 1 << 3;

/// `clap_audio_port_info_t` — `ext/audio-ports.h:42-65`.
#[repr(C)]
pub struct ClapAudioPortInfo {
    pub id: ClapId,
    pub name: [c_char; CLAP_NAME_SIZE],
    pub flags: u32,
    pub channel_count: u32,
    pub port_type: *const c_char,
    pub in_place_pair: ClapId,
}

/// `clap_plugin_audio_ports_t` — `ext/audio-ports.h:68-80`. Both methods are
/// `[main-thread]`. (The host-side struct and the rescan flags are not
/// mirrored yet: nothing in Phase 1 uses them.)
#[repr(C)]
pub struct ClapPluginAudioPorts {
    pub count: unsafe extern "C" fn(plugin: *const ClapPlugin, is_input: bool) -> u32,
    pub get: unsafe extern "C" fn(
        plugin: *const ClapPlugin,
        index: u32,
        is_input: bool,
        info: *mut ClapAudioPortInfo,
    ) -> bool,
}

// -------------------------------------------------------------- stream.h

/// `clap_istream_t` — `stream.h:22-27`. `read` returns bytes read, `0` at
/// EOF, `-1` on error — and may return *fewer bytes than requested*, so
/// consumers must loop (`stream.h:10-16`).
#[repr(C)]
pub struct ClapIStream {
    pub ctx: *mut c_void,
    pub read: unsafe extern "C" fn(
        stream: *const ClapIStream,
        buffer: *mut c_void,
        size: u64,
    ) -> i64,
}

/// `clap_ostream_t` — `stream.h:29-34`. `write` returns bytes written or
/// `-1`; partial writes possible, loop as with reads.
#[repr(C)]
pub struct ClapOStream {
    pub ctx: *mut c_void,
    pub write: unsafe extern "C" fn(
        stream: *const ClapOStream,
        buffer: *const c_void,
        size: u64,
    ) -> i64,
}

// ---------------------------------------------------------- ext/state.h

/// `CLAP_EXT_STATE` — `ext/state.h:18`. Without it, hosts should not
/// persist parameter values at all (`ext/params.h:101-107`).
pub const CLAP_EXT_STATE: &CStr = c"clap.state";

/// `clap_plugin_state_t` — `ext/state.h:24-34`. Both `[main-thread]`.
#[repr(C)]
pub struct ClapPluginState {
    pub save:
        unsafe extern "C" fn(plugin: *const ClapPlugin, stream: *const ClapOStream) -> bool,
    pub load:
        unsafe extern "C" fn(plugin: *const ClapPlugin, stream: *const ClapIStream) -> bool,
}

/// `clap_host_state_t` — `ext/state.h:36-41`. Looked up under the same
/// `clap.state` id as the plugin side.
#[repr(C)]
pub struct ClapHostState {
    /// `[main-thread]` — "tell the host that the plugin state has changed
    /// and should be saved again" (`ext/state.h:37`). Parameter changes are
    /// implicitly dirty (`ext/state.h:38`); everything else is not, which
    /// is why an editor-written state block has to say so.
    pub mark_dirty: unsafe extern "C" fn(host: *const ClapHost),
}

// ------------------------------------------------------------ ext/gui.h

/// `CLAP_EXT_GUI` — `ext/gui.h:47`.
pub const CLAP_EXT_GUI: &CStr = c"clap.gui";

/// Window API constants — `ext/gui.h:54-68` (complete set).
pub const CLAP_WINDOW_API_WIN32: &CStr = c"win32";
/// `ext/gui.h:57`. Cocoa uses logical size; do not call `set_scale()`.
pub const CLAP_WINDOW_API_COCOA: &CStr = c"cocoa";
/// See [`CLAP_WINDOW_API_WIN32`].
pub const CLAP_WINDOW_API_UIKIT: &CStr = c"uikit";
/// See [`CLAP_WINDOW_API_WIN32`].
pub const CLAP_WINDOW_API_X11: &CStr = c"x11";
/// See [`CLAP_WINDOW_API_WIN32`].
pub const CLAP_WINDOW_API_WAYLAND: &CStr = c"wayland";

/// The handle union inside `clap_window` — `ext/gui.h:82-88`. For
/// `"cocoa"` the payload is an `NSView *` (`clap_nsview`, `ext/gui.h:75`).
#[repr(C)]
#[derive(Clone, Copy)]
pub union ClapWindowHandle {
    pub cocoa: *mut c_void,
    pub uikit: *mut c_void,
    pub x11: std::ffi::c_ulong,
    pub win32: *mut c_void,
    pub ptr: *mut c_void,
}

/// `clap_window_t` — `ext/gui.h:79-89`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ClapWindow {
    pub api: *const c_char,
    pub handle: ClapWindowHandle,
}

/// `clap_gui_resize_hints_t` — `ext/gui.h:91-103`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ClapGuiResizeHints {
    pub can_resize_horizontally: bool,
    pub can_resize_vertically: bool,
    pub preserve_aspect_ratio: bool,
    pub aspect_ratio_width: u32,
    pub aspect_ratio_height: u32,
}

/// `clap_plugin_gui_t` — `ext/gui.h:107-212`. Every method is
/// `[main-thread]`; some additionally require embedded (`!floating`) or
/// floating mode, quoted at each implementation site.
#[repr(C)]
pub struct ClapPluginGui {
    pub is_api_supported: unsafe extern "C" fn(
        plugin: *const ClapPlugin,
        api: *const c_char,
        is_floating: bool,
    ) -> bool,
    pub get_preferred_api: unsafe extern "C" fn(
        plugin: *const ClapPlugin,
        api: *mut *const c_char,
        is_floating: *mut bool,
    ) -> bool,
    pub create: unsafe extern "C" fn(
        plugin: *const ClapPlugin,
        api: *const c_char,
        is_floating: bool,
    ) -> bool,
    pub destroy: unsafe extern "C" fn(plugin: *const ClapPlugin),
    pub set_scale: unsafe extern "C" fn(plugin: *const ClapPlugin, scale: f64) -> bool,
    pub get_size: unsafe extern "C" fn(
        plugin: *const ClapPlugin,
        width: *mut u32,
        height: *mut u32,
    ) -> bool,
    pub can_resize: unsafe extern "C" fn(plugin: *const ClapPlugin) -> bool,
    pub get_resize_hints: unsafe extern "C" fn(
        plugin: *const ClapPlugin,
        hints: *mut ClapGuiResizeHints,
    ) -> bool,
    pub adjust_size: unsafe extern "C" fn(
        plugin: *const ClapPlugin,
        width: *mut u32,
        height: *mut u32,
    ) -> bool,
    pub set_size:
        unsafe extern "C" fn(plugin: *const ClapPlugin, width: u32, height: u32) -> bool,
    pub set_parent:
        unsafe extern "C" fn(plugin: *const ClapPlugin, window: *const ClapWindow) -> bool,
    pub set_transient:
        unsafe extern "C" fn(plugin: *const ClapPlugin, window: *const ClapWindow) -> bool,
    pub suggest_title:
        unsafe extern "C" fn(plugin: *const ClapPlugin, title: *const c_char),
    pub show: unsafe extern "C" fn(plugin: *const ClapPlugin) -> bool,
    pub hide: unsafe extern "C" fn(plugin: *const ClapPlugin) -> bool,
}

/// `clap_host_gui_t` — `ext/gui.h:214-245`. Unused so far (Phase 6.1 has a
/// fixed-size window); mirrored complete for later resize support.
#[repr(C)]
pub struct ClapHostGui {
    pub resize_hints_changed: unsafe extern "C" fn(host: *const ClapHost),
    pub request_resize:
        unsafe extern "C" fn(host: *const ClapHost, width: u32, height: u32) -> bool,
    pub request_show: unsafe extern "C" fn(host: *const ClapHost) -> bool,
    pub request_hide: unsafe extern "C" fn(host: *const ClapHost) -> bool,
    pub closed: unsafe extern "C" fn(host: *const ClapHost, was_destroyed: bool),
}

// --------------------------------------------------- ext/timer-support.h

/// `CLAP_EXT_TIMER_SUPPORT` — `ext/timer-support.h:5`.
pub const CLAP_EXT_TIMER_SUPPORT: &CStr = c"clap.timer-support";

/// `clap_plugin_timer_support_t` — `ext/timer-support.h:11-14`.
/// `on_timer` is `[main-thread]`.
#[repr(C)]
pub struct ClapPluginTimerSupport {
    pub on_timer: unsafe extern "C" fn(plugin: *const ClapPlugin, timer_id: ClapId),
}

/// `clap_host_timer_support_t` — `ext/timer-support.h:16-27`. Both
/// `[main-thread]`; "30 Hz should be allowed" (`:19`).
#[repr(C)]
pub struct ClapHostTimerSupport {
    pub register_timer: unsafe extern "C" fn(
        host: *const ClapHost,
        period_ms: u32,
        timer_id: *mut ClapId,
    ) -> bool,
    pub unregister_timer: unsafe extern "C" fn(host: *const ClapHost, timer_id: ClapId) -> bool,
}

// --------------------------------------------------------- ext/params.h

/// `CLAP_EXT_PARAMS` — `ext/params.h:127`.
pub const CLAP_EXT_PARAMS: &CStr = c"clap.params";

/// `clap_param_info_flags` — `ext/params.h:133-207` (complete, in order).
pub const CLAP_PARAM_IS_STEPPED: u32 = 1 << 0;
/// See [`CLAP_PARAM_IS_STEPPED`].
pub const CLAP_PARAM_IS_PERIODIC: u32 = 1 << 1;
/// See [`CLAP_PARAM_IS_STEPPED`].
pub const CLAP_PARAM_IS_HIDDEN: u32 = 1 << 2;
/// See [`CLAP_PARAM_IS_STEPPED`].
pub const CLAP_PARAM_IS_READONLY: u32 = 1 << 3;
/// See [`CLAP_PARAM_IS_STEPPED`].
pub const CLAP_PARAM_IS_BYPASS: u32 = 1 << 4;
/// See [`CLAP_PARAM_IS_STEPPED`].
pub const CLAP_PARAM_IS_AUTOMATABLE: u32 = 1 << 5;
/// See [`CLAP_PARAM_IS_STEPPED`].
pub const CLAP_PARAM_IS_AUTOMATABLE_PER_NOTE_ID: u32 = 1 << 6;
/// See [`CLAP_PARAM_IS_STEPPED`].
pub const CLAP_PARAM_IS_AUTOMATABLE_PER_KEY: u32 = 1 << 7;
/// See [`CLAP_PARAM_IS_STEPPED`].
pub const CLAP_PARAM_IS_AUTOMATABLE_PER_CHANNEL: u32 = 1 << 8;
/// See [`CLAP_PARAM_IS_STEPPED`].
pub const CLAP_PARAM_IS_AUTOMATABLE_PER_PORT: u32 = 1 << 9;
/// See [`CLAP_PARAM_IS_STEPPED`].
pub const CLAP_PARAM_IS_MODULATABLE: u32 = 1 << 10;
/// See [`CLAP_PARAM_IS_STEPPED`].
pub const CLAP_PARAM_IS_MODULATABLE_PER_NOTE_ID: u32 = 1 << 11;
/// See [`CLAP_PARAM_IS_STEPPED`].
pub const CLAP_PARAM_IS_MODULATABLE_PER_KEY: u32 = 1 << 12;
/// See [`CLAP_PARAM_IS_STEPPED`].
pub const CLAP_PARAM_IS_MODULATABLE_PER_CHANNEL: u32 = 1 << 13;
/// See [`CLAP_PARAM_IS_STEPPED`].
pub const CLAP_PARAM_IS_MODULATABLE_PER_PORT: u32 = 1 << 14;
/// See [`CLAP_PARAM_IS_STEPPED`].
pub const CLAP_PARAM_REQUIRES_PROCESS: u32 = 1 << 15;
/// See [`CLAP_PARAM_IS_STEPPED`].
pub const CLAP_PARAM_IS_ENUM: u32 = 1 << 16;

/// `clap_param_info_t` — `ext/params.h:211-256`.
#[repr(C)]
pub struct ClapParamInfo {
    pub id: ClapId,
    pub flags: u32,
    pub cookie: *mut c_void,
    pub name: [c_char; CLAP_NAME_SIZE],
    pub module: [c_char; CLAP_PATH_SIZE],
    pub min_value: f64,
    pub max_value: f64,
    pub default_value: f64,
}

/// `clap_plugin_params_t` — `ext/params.h:258-307`. All `[main-thread]`
/// except `flush`, which is `[active ? audio-thread : main-thread]`.
/// (The host-side struct and the rescan/clear flag enums are not mirrored
/// yet: nothing in Phase 2 uses them.)
#[repr(C)]
pub struct ClapPluginParams {
    pub count: unsafe extern "C" fn(plugin: *const ClapPlugin) -> u32,
    pub get_info: unsafe extern "C" fn(
        plugin: *const ClapPlugin,
        param_index: u32,
        param_info: *mut ClapParamInfo,
    ) -> bool,
    pub get_value: unsafe extern "C" fn(
        plugin: *const ClapPlugin,
        param_id: ClapId,
        out_value: *mut f64,
    ) -> bool,
    pub value_to_text: unsafe extern "C" fn(
        plugin: *const ClapPlugin,
        param_id: ClapId,
        value: f64,
        out_buffer: *mut c_char,
        out_buffer_capacity: u32,
    ) -> bool,
    pub text_to_value: unsafe extern "C" fn(
        plugin: *const ClapPlugin,
        param_id: ClapId,
        param_value_text: *const c_char,
        out_value: *mut f64,
    ) -> bool,
    pub flush: unsafe extern "C" fn(
        plugin: *const ClapPlugin,
        in_: *const ClapInputEvents,
        out: *const ClapOutputEvents,
    ),
}

/// `clap_param_rescan_flags` — `ext/params.h:348`. Typedef mirrored for
/// signature fidelity; the flag constants are not mirrored (nothing uses
/// them yet — a typedef carries no member-omission hazard).
pub type ClapParamRescanFlags = u32;
/// `clap_param_clear_flags` — `ext/params.h:360`. See
/// [`ClapParamRescanFlags`].
pub type ClapParamClearFlags = u32;

/// `clap_host_params_t` — `ext/params.h:362-382`. `request_flush` is
/// `[thread-safe, !audio-thread]` (`:377-381`): after it, the host
/// schedules a call to `process()` or `params.flush()`, where the plugin
/// can emit its outgoing parameter events.
#[repr(C)]
pub struct ClapHostParams {
    pub rescan: unsafe extern "C" fn(host: *const ClapHost, flags: ClapParamRescanFlags),
    pub clear: unsafe extern "C" fn(
        host: *const ClapHost,
        param_id: ClapId,
        flags: ClapParamClearFlags,
    ),
    pub request_flush: unsafe extern "C" fn(host: *const ClapHost),
}
