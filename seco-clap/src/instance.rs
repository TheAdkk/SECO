//! The bridge between one live `clap_plugin` and its `Plugin` state.
//!
//! # Aliasing model
//!
//! Phase 1 handed every callback a `&mut Instance`. That became unsound the
//! moment the params extension arrived: `clap_plugin_params.get_value` is
//! `[main-thread]` and may run *while the audio thread is inside
//! `process()`* — two live `&mut` (or a `&` plus a `&mut`) over one
//! `Instance` is undefined behavior regardless of which fields they touch.
//!
//! The rule now: an `Instance` is only ever borrowed **shared** (`&`).
//! Concurrently-visible values (the parameters, the trace slots) are
//! atomics, safe through `&`. The plugin state `P` lives in an `UnsafeCell`,
//! and only callbacks that CLAP serializes against each other (the
//! audio-thread group, and lifecycle calls that the host must not overlap
//! with processing) materialize `&mut P` from it, each with a `SAFETY:`
//! stating which guarantee makes it exclusive.

use std::cell::UnsafeCell;
use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use seco_core::__private::with_rt_context;
use seco_core::{AudioBuffer, Plugin, Transport};

use crate::MAX_PARAMS;
use crate::ext::audio_ports;
use crate::ext::params::ParamsImpl;
use crate::ext::state::StateImpl;
use crate::factory;
#[cfg(any(test, all(feature = "gui", target_os = "macos")))]
use crate::ffi::{
    CLAP_EVENT_IS_LIVE, CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END,
    ClapEventHeader, ClapEventParamGesture, ClapOutputEvents,
};
#[cfg(all(feature = "gui", target_os = "macos"))]
use crate::ffi::ClapHostParams;
use crate::ffi::{
    CLAP_BEATTIME_FACTOR, CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE, CLAP_EVENT_TRANSPORT,
    CLAP_EXT_AUDIO_PORTS, CLAP_EXT_PARAMS, CLAP_EXT_STATE, CLAP_PROCESS_CONTINUE,
    CLAP_PROCESS_ERROR, CLAP_SECTIME_FACTOR, CLAP_TRANSPORT_HAS_BEATS_TIMELINE,
    CLAP_TRANSPORT_HAS_SECONDS_TIMELINE, CLAP_TRANSPORT_HAS_TEMPO,
    CLAP_TRANSPORT_HAS_TIME_SIGNATURE, CLAP_TRANSPORT_IS_PLAYING, ClapAudioBuffer,
    ClapEventParamValue, ClapEventTransport, ClapHost, ClapInputEvents, ClapPlugin,
    ClapPluginAudioPorts, ClapPluginParams, ClapPluginState, ClapProcess, ClapProcessStatus,
};

/// Channel capacity of the stack-allocated slice table in `plugin_process`.
/// SECO v1 declares stereo ports, so 2 is exact; if a host hands over more
/// channels anyway, the excess is passed through untouched rather than
/// allocating.
const MAX_CHANNELS: usize = 2;

/// Heap-allocated per-instance data. `clap_plugin.plugin_data` points back to
/// this, giving every callback access to the vtable struct, the shared
/// atomics, and (where exclusivity is guaranteed) the plugin state.
pub(crate) struct Instance<P: Plugin> {
    clap_plugin: ClapPlugin,
    /// See the module docs: `&mut P` is only materialized inside callbacks
    /// whose exclusivity CLAP guarantees.
    state: UnsafeCell<P>,
    /// The plugin's own state block and its hand-off to the audio thread.
    /// Adapter-owned for the same reason as `param_bits`: `clap.state` runs
    /// on the main thread while `process()` may be running. See
    /// `plugin_state`.
    pub(crate) plugin_state: crate::plugin_state::PluginStateSlot,
    /// Plain parameter values as f64 bit patterns, indexed like
    /// `P::PARAMS`. Atomics because the main thread reads them (`get_value`,
    /// state save) while the audio thread applies events. Slots beyond
    /// `P::PARAMS.len()` are unused.
    pub(crate) param_bits: [AtomicU64; MAX_PARAMS],
    /// Editor slot; see `ext/gui.rs` for the main-thread-only exclusivity
    /// argument. Present only with the `gui` feature on macOS.
    #[cfg(all(feature = "gui", target_os = "macos"))]
    pub(crate) gui: crate::ext::gui::GuiSlot,
    /// Host handle, used by the GUI to register its refresh timer. Valid
    /// until after `destroy` (factory/plugin-factory.h:31). cfg-gated so
    /// the no-gui build stays byte-identical.
    #[cfg(all(feature = "gui", target_os = "macos"))]
    pub(crate) host: *const ClapHost,
    /// Host params extension, looked up in `plugin_init` (host callbacks
    /// are forbidden in create, factory/plugin-factory.h:33). Main-thread
    /// written-once/read-only field, like the gui slot.
    #[cfg(all(feature = "gui", target_os = "macos"))]
    pub(crate) host_params: UnsafeCell<*const ClapHostParams>,
    /// Host state extension, same lookup and same field discipline. Used to
    /// mark the session dirty when the editor writes a state block —
    /// parameter changes are implicitly dirty, a state block is not
    /// (ext/state.h:37-38).
    #[cfg(all(feature = "gui", target_os = "macos"))]
    pub(crate) host_state: UnsafeCell<*const crate::ffi::ClapHostState>,
    /// GUI -> host parameter changes, pending until the next process() or
    /// flush() drains them. One slot per param: value coalesces to the
    /// latest (a fast drag becomes one event per drain), gesture edges are
    /// sticky bits. Single producer (main-thread gui callbacks, serialized
    /// by the host) / single consumer (the audio-thread role) — the same
    /// crossing model as param_bits.
    #[cfg(any(test, all(feature = "gui", target_os = "macos")))]
    pub(crate) gui_pending: [PendingParam; MAX_PARAMS],
    #[cfg(debug_assertions)]
    pub(crate) trace: std::sync::Arc<crate::trace::TransportTrace>,
    #[cfg(debug_assertions)]
    trace_stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(debug_assertions)]
    trace_thread: Option<std::thread::JoinHandle<()>>,
}

pub(crate) fn create<P: Plugin>(host: *const ClapHost) -> *const ClapPlugin {
    // seco_export! asserts this at compile time; direct users of this crate
    // (tests) get the check here.
    assert!(P::PARAMS.len() <= MAX_PARAMS, "plugin declares more parameters than MAX_PARAMS");
    // The host pointer is only consulted by the debug-build trace so far.
    #[cfg(not(debug_assertions))]
    let _ = host;
    #[cfg(debug_assertions)]
    let trace = std::sync::Arc::new(crate::trace::TransportTrace::default());
    #[cfg(debug_assertions)]
    let trace_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // The logger thread only exists in debug builds and only under a real
    // host (tests pass a null host and stay thread- and file-free, which
    // keeps them runnable under miri). Reading `host.name` here touches data
    // fields, not host *callbacks*, which are what create_plugin forbids.
    #[cfg(debug_assertions)]
    let trace_thread = if host.is_null() {
        None
    } else {
        // SAFETY: non-null host stays valid until after destroy
        // (factory/plugin-factory.h:31); `name` is mandatory (host.h:14) but
        // checked anyway.
        let name_ptr = unsafe { (*host).name };
        let name = if name_ptr.is_null() {
            String::from("unknown-host")
        } else {
            // SAFETY: hosts provide NUL-terminated descriptor strings.
            unsafe { CStr::from_ptr(name_ptr) }.to_string_lossy().into_owned()
        };
        Some(crate::trace::spawn_logger(name, trace.clone(), trace_stop.clone()))
    };

    let instance = Box::new(Instance {
        clap_plugin: ClapPlugin {
            desc: factory::descriptor_for::<P>(),
            plugin_data: ptr::null_mut(),
            init: {
                #[cfg(not(all(feature = "gui", target_os = "macos")))]
                let f = plugin_init as unsafe extern "C" fn(*const ClapPlugin) -> bool;
                #[cfg(all(feature = "gui", target_os = "macos"))]
                let f = plugin_init::<P> as unsafe extern "C" fn(*const ClapPlugin) -> bool;
                f
            },
            destroy: plugin_destroy::<P>,
            activate: plugin_activate::<P>,
            deactivate: plugin_deactivate::<P>,
            start_processing: plugin_start_processing::<P>,
            stop_processing: plugin_stop_processing,
            reset: plugin_reset::<P>,
            process: plugin_process::<P>,
            get_extension: plugin_get_extension::<P>,
            on_main_thread: plugin_on_main_thread,
        },
        state: UnsafeCell::new(P::new()),
        plugin_state: crate::plugin_state::PluginStateSlot::new(),
        param_bits: std::array::from_fn(|index| {
            let default =
                P::PARAMS.get(index).map(|desc| desc.range.default_plain()).unwrap_or(0.0);
            AtomicU64::new(default.to_bits())
        }),
        #[cfg(all(feature = "gui", target_os = "macos"))]
        gui: crate::ext::gui::empty_slot(),
        #[cfg(all(feature = "gui", target_os = "macos"))]
        host,
        #[cfg(all(feature = "gui", target_os = "macos"))]
        host_params: UnsafeCell::new(ptr::null()),
        #[cfg(all(feature = "gui", target_os = "macos"))]
        host_state: UnsafeCell::new(ptr::null()),
        #[cfg(any(test, all(feature = "gui", target_os = "macos")))]
        gui_pending: std::array::from_fn(|_| PendingParam::default()),
        #[cfg(debug_assertions)]
        trace,
        #[cfg(debug_assertions)]
        trace_stop,
        #[cfg(debug_assertions)]
        trace_thread,
    });
    let raw = Box::into_raw(instance);
    // SAFETY: `raw` came from Box::into_raw just above, so it is valid and
    // uniquely owned; we complete the self-reference the host hands back to
    // every callback.
    unsafe { (*raw).clap_plugin.plugin_data = raw.cast::<c_void>() };
    // SAFETY: same validity as above; the host receives a pointer to the
    // `clap_plugin` field and must pass it back unchanged (plugin.h:41-44).
    unsafe { &raw const (*raw).clap_plugin }
}

/// Recovers a shared borrow of the instance from the pointer the host passes
/// back.
///
/// SAFETY contract for callers: `plugin` must be a pointer obtained from
/// [`create`] and not yet destroyed. The returned `&Instance` may coexist
/// with others on other threads; anything reachable through it is either
/// atomic or behind the `state` UnsafeCell (see module docs).
pub(crate) unsafe fn shared<'a, P: Plugin>(plugin: *const ClapPlugin) -> &'a Instance<P> {
    // SAFETY: per the contract above, `plugin_data` was set in `create` and
    // points to the owning `Instance<P>`; no `&mut Instance` ever exists
    // after `create` returns.
    unsafe { &*(*plugin).plugin_data.cast::<Instance<P>>() }
}

/// Applies the events SECO understands from a host event list: parameter
/// values into the param atomics (and, in debug builds, counts mid-block
/// transport events). Shared by `process()` and `params.flush`.
///
/// SAFETY contract for callers: `list`, if non-null, must be valid for the
/// duration of the call, with host-provided vtable functions.
pub(crate) unsafe fn apply_input_events<P: Plugin>(
    inst: &Instance<P>,
    list: *const ClapInputEvents,
) {
    if list.is_null() {
        return;
    }
    // SAFETY: per fn contract the list and its callbacks are valid.
    let count = unsafe { ((*list).size)(list) };
    for index in 0..count {
        // SAFETY: `index < count`; `get` returns a pointer owned by the list
        // (events.h:351-352).
        let header = unsafe { ((*list).get)(list, index) };
        if header.is_null() {
            continue;
        }
        // SAFETY: the header pointer is valid for at least the header bytes.
        // `read_unaligned` because events are only specified as memcpy-able
        // blobs (events.h:14-17) — nothing promises the pointer is aligned,
        // and a reference to a misaligned event is UB even if never used.
        let head = unsafe { header.read_unaligned() };
        if head.space_id != CLAP_CORE_EVENT_SPACE_ID {
            continue;
        }
        match head.type_ {
            CLAP_EVENT_PARAM_VALUE if head.size as usize >= size_of::<ClapEventParamValue>() => {
                // SAFETY: size checked against the full event; unaligned for
                // the same reason as the header read above (the f64 fields
                // make this struct align-8, stricter than the header).
                let event = unsafe { header.cast::<ClapEventParamValue>().read_unaligned() };
                let index = event.param_id as usize;
                if index < P::PARAMS.len() {
                    inst.param_bits[index].store(event.value.to_bits(), Relaxed);
                }
            }
            #[cfg(debug_assertions)]
            CLAP_EVENT_TRANSPORT => {
                inst.trace.transport_events.fetch_add(1, Relaxed);
            }
            _ => {}
        }
    }
    // Release builds don't count transport events; silence the unused const.
    #[cfg(not(debug_assertions))]
    let _ = CLAP_EVENT_TRANSPORT;
}

#[cfg(any(test, all(feature = "gui", target_os = "macos")))]
pub(crate) mod gui_queue {
    //! The GUI->host parameter pipe. See `Instance::gui_pending`.

    use std::sync::atomic::{AtomicU8, AtomicU64, Ordering::{AcqRel, Relaxed, Release}};

    /// Sticky pending bits. VALUE coalesces; BEGIN/END are edges.
    pub(crate) const VALUE: u8 = 1 << 0;
    pub(crate) const BEGIN: u8 = 1 << 1;
    pub(crate) const END: u8 = 1 << 2;

    #[derive(Default)]
    pub(crate) struct PendingParam {
        /// Latest plain value from the GUI, as f64 bits.
        pub(crate) value_bits: AtomicU64,
        /// VALUE/BEGIN/END bits. The Release store here publishes
        /// `value_bits` to the draining thread's Acquire swap.
        pub(crate) flags: AtomicU8,
    }

    impl PendingParam {
        pub(crate) fn set_value(&self, value: f64) {
            // Value first (Relaxed), then the flag with Release: the
            // consumer's Acquire swap of `flags` makes the value visible.
            self.value_bits.store(value.to_bits(), Relaxed);
            self.flags.fetch_or(VALUE, Release);
        }

        pub(crate) fn mark(&self, bit: u8) {
            self.flags.fetch_or(bit, Release);
        }

        pub(crate) fn take(&self) -> (u8, f64) {
            let flags = self.flags.swap(0, AcqRel);
            (flags, f64::from_bits(self.value_bits.load(Relaxed)))
        }

        /// Puts unsent bits back after a failed try_push, to retry on the
        /// next drain.
        pub(crate) fn retry(&self, bits: u8) {
            self.flags.fetch_or(bits, Release);
        }
    }

    /// A parsed parameter message from the editor.
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum GuiMsg {
        GestureBegin(usize),
        Set(usize, f64),
        GestureEnd(usize),
    }

    /// What a page can send. Parameters take CLAP's own path; a state block
    /// goes to the plugin's state slot.
    #[derive(Debug)]
    pub(crate) enum EditorMsg<'a> {
        Param(GuiMsg),
        /// Opaque payload — the framework stores the bytes, the plugin
        /// decides what they mean.
        State(&'a str),
    }

    /// Wire format from JS, deliberately dumb: "begin <i>", "set <i>
    /// <plain>", "end <i>", and "state <payload>" where the payload is the
    /// rest of the message verbatim, spaces and all.
    ///
    /// Lives here rather than next to the WebKit plumbing because the
    /// protocol is not platform-specific — this way it is tested on every
    /// platform, not only where the editor compiles.
    pub(crate) fn parse_msg(text: &str) -> Option<EditorMsg<'_>> {
        if let Some(block) = text.strip_prefix("state ") {
            return Some(EditorMsg::State(block));
        }
        // "state" with no payload is a legal message: it clears the block.
        if text == "state" {
            return Some(EditorMsg::State(""));
        }
        let mut parts = text.split_ascii_whitespace();
        let verb = parts.next()?;
        let index: usize = parts.next()?.parse().ok()?;
        let msg = match verb {
            "begin" => GuiMsg::GestureBegin(index),
            "end" => GuiMsg::GestureEnd(index),
            "set" => {
                let value: f64 = parts.next()?.parse().ok()?;
                if !value.is_finite() {
                    return None;
                }
                GuiMsg::Set(index, value)
            }
            _ => return None,
        };
        Some(EditorMsg::Param(msg))
    }
}

#[cfg(any(test, all(feature = "gui", target_os = "macos")))]
pub(crate) use gui_queue::PendingParam;

/// Stores a state block written by the editor and tells the host the
/// session changed.
///
/// The editor runs on the main thread — WebKit delivers its messages there,
/// and every clap.gui callback is `[main-thread]` — which is exactly where
/// the state block may be written, so this needs no queue of its own: it
/// hands the bytes straight to the slot, and the triple buffer carries them
/// to the audio thread at the next process().
///
/// The bytes are opaque here. A page sends text and the plugin parses it in
/// `apply_state`; a page wanting binary encodes it (`btoa`) itself. The
/// framework's job is to store exactly what was sent and hand back exactly
/// that.
///
/// A block that does not fit is dropped rather than truncated, and the host
/// is not told the session changed — a half-written curve is not state
/// worth saving.
///
/// # Safety
///
/// `plugin` must be a live instance created by [`create`], and this must
/// run on the main thread (the gui/webview callbacks all do).
#[cfg(any(test, all(feature = "gui", target_os = "macos")))]
pub(crate) unsafe fn publish_editor_state<P: Plugin>(plugin: *const ClapPlugin, bytes: &[u8]) {
    // SAFETY: live instance per this function's contract.
    let inst = unsafe { shared::<P>(plugin) };
    // SAFETY: `[main-thread]` per this function's contract.
    let stored = unsafe { inst.plugin_state.publish(bytes) };

    // Parameter edits are implicitly dirty; a state block is not
    // (ext/state.h:37-38). Without this the host would let the user close
    // the session and lose the edit. A dropped block says nothing.
    #[cfg(all(feature = "gui", target_os = "macos"))]
    if stored {
        // SAFETY: main-thread-only field, written once in init.
        let host_state = unsafe { *inst.host_state.get() };
        if !host_state.is_null() && !inst.host.is_null() {
            // SAFETY: the extension pointer stays valid until destroy
            // (host.h:20-25); mark_dirty is [main-thread] (ext/state.h:39).
            unsafe { ((*host_state).mark_dirty)(inst.host) };
        }
    }
    // No editor build, no host to notify.
    #[cfg(not(all(feature = "gui", target_os = "macos")))]
    let _ = stored;
}

/// Queues a GUI-originated parameter change and asks the host to flush.
///
/// This is CLAP's scenario III (ext/params.h:54-60): the GUI never writes
/// the audio-visible value directly — the change is applied AND announced
/// to the host inside the next process()/flush() drain, keeping the two
/// perfectly consistent and giving the GUI the exact same latency as
/// host-sent events.
///
/// SAFETY contract: `plugin` per [`shared`]'s contract; call from the main
/// thread only (gui/webview callbacks).
#[cfg(any(test, all(feature = "gui", target_os = "macos")))]
pub(crate) unsafe fn queue_gui_param_change<P: Plugin>(
    plugin: *const ClapPlugin,
    msg: gui_queue::GuiMsg,
) {
    use gui_queue::{BEGIN, END, GuiMsg};
    // SAFETY: per fn contract.
    let inst = unsafe { shared::<P>(plugin) };
    let index = match msg {
        GuiMsg::GestureBegin(index) | GuiMsg::Set(index, _) | GuiMsg::GestureEnd(index) => index,
    };
    if index >= P::PARAMS.len() {
        return;
    }
    match msg {
        GuiMsg::GestureBegin(_) => inst.gui_pending[index].mark(BEGIN),
        GuiMsg::Set(_, value) => {
            let range = &P::PARAMS[index].range;
            inst.gui_pending[index].set_value(value.clamp(range.min(), range.max()));
        }
        GuiMsg::GestureEnd(_) => inst.gui_pending[index].mark(END),
    }
    // Ask the host to schedule process()/flush() so the drain runs soon.
    // [thread-safe, !audio-thread] (ext/params.h:377-381); we are on main.
    #[cfg(all(feature = "gui", target_os = "macos"))]
    {
        // SAFETY: main-thread-only field (written once in plugin_init).
        let host_params = unsafe { *inst.host_params.get() };
        if !host_params.is_null() && !inst.host.is_null() {
            // SAFETY: valid host vtable per host.h:20-25.
            unsafe { ((*host_params).request_flush)(inst.host) };
        }
    }
}

/// Drains GUI-originated changes: writes the audio-visible atomic FIRST,
/// then announces the change to the host via `out_events`.
///
/// Order rationale: the audio truth must never lag the host's view. If the
/// host push fails (queue full), audio already plays the new value and the
/// notification retries next drain; the reverse order could record
/// automation the audio was not playing.
///
/// SAFETY contract: runs inside process() or params.flush() (the single
/// consumer); `out`, if non-null, valid for the call.
#[cfg(any(test, all(feature = "gui", target_os = "macos")))]
pub(crate) unsafe fn drain_gui_params<P: Plugin>(
    inst: &Instance<P>,
    out: *const ClapOutputEvents,
) {
    use gui_queue::{BEGIN, END, VALUE};
    for (index, pending) in inst.gui_pending.iter().enumerate().take(P::PARAMS.len()) {
        let (flags, value) = pending.take();
        if flags == 0 {
            continue;
        }
        // 1. The audio-visible atomic — same slot host events write.
        if flags & VALUE != 0 {
            inst.param_bits[index].store(value.to_bits(), Relaxed);
        }
        // 2. The host notification, in gesture order.
        if out.is_null() {
            continue; // host offered no event list; audio is correct, move on
        }
        let mut unsent = 0;
        if flags & BEGIN != 0 && !push_gesture(out, index as u32, CLAP_EVENT_PARAM_GESTURE_BEGIN) {
            unsent |= BEGIN;
        }
        if flags & VALUE != 0 && !push_value(out, index as u32, value) {
            unsent |= VALUE;
        }
        if flags & END != 0 && !push_gesture(out, index as u32, CLAP_EVENT_PARAM_GESTURE_END) {
            unsent |= END;
        }
        if unsent != 0 {
            pending.retry(unsent);
        }
    }
}

#[cfg(any(test, all(feature = "gui", target_os = "macos")))]
fn push_value(out: *const ClapOutputEvents, param_id: u32, value: f64) -> bool {
    let event = ClapEventParamValue {
        header: ClapEventHeader {
            size: size_of::<ClapEventParamValue>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_VALUE,
            // A user turning our on-screen knob is the definition of a
            // live event (events.h:30-32).
            flags: CLAP_EVENT_IS_LIVE,
        },
        param_id,
        cookie: std::ptr::null_mut(),
        note_id: -1,
        port_index: -1,
        channel: -1,
        key: -1,
        value,
    };
    // SAFETY: out is non-null (checked by caller) and valid for the call;
    // try_push copies the event (events.h:359-362).
    unsafe { ((*out).try_push)(out, (&raw const event).cast()) }
}

#[cfg(any(test, all(feature = "gui", target_os = "macos")))]
fn push_gesture(out: *const ClapOutputEvents, param_id: u32, type_: u16) -> bool {
    let event = ClapEventParamGesture {
        header: ClapEventHeader {
            size: size_of::<ClapEventParamGesture>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_,
            flags: CLAP_EVENT_IS_LIVE,
        },
        param_id,
    };
    // SAFETY: as in push_value.
    unsafe { ((*out).try_push)(out, (&raw const event).cast()) }
}

/// Converts a CLAP transport into core's host-agnostic [`Transport`],
/// honoring the validity flags (events.h:263-272).
fn convert_transport(tp: &ClapEventTransport) -> Transport {
    let has = |flag: u32| tp.flags & flag != 0;
    Transport {
        tempo_bpm: has(CLAP_TRANSPORT_HAS_TEMPO).then_some(tp.tempo),
        song_pos_beats: has(CLAP_TRANSPORT_HAS_BEATS_TIMELINE)
            .then(|| tp.song_pos_beats as f64 / CLAP_BEATTIME_FACTOR as f64),
        song_pos_seconds: has(CLAP_TRANSPORT_HAS_SECONDS_TIMELINE)
            .then(|| tp.song_pos_seconds as f64 / CLAP_SECTIME_FACTOR as f64),
        time_signature: has(CLAP_TRANSPORT_HAS_TIME_SIGNATURE)
            .then_some((tp.tsig_num, tp.tsig_denom)),
        playing: has(CLAP_TRANSPORT_IS_PLAYING),
    }
}

/// `plugin.h:46-53` `[main-thread]`. No host extensions are needed yet, so
/// there is nothing to do. (Non-generic on purpose: the gui build swaps in
/// a generic version below, and the no-gui binary must stay byte-identical.)
#[cfg(not(all(feature = "gui", target_os = "macos")))]
unsafe extern "C" fn plugin_init(_plugin: *const ClapPlugin) -> bool {
    true
}

/// `plugin.h:46-53` `[main-thread]`. Host extension lookups belong here —
/// they are forbidden in create_plugin (factory/plugin-factory.h:33).
#[cfg(all(feature = "gui", target_os = "macos"))]
unsafe extern "C" fn plugin_init<P: Plugin>(_plugin: *const ClapPlugin) -> bool {
    if !_plugin.is_null() {
        // SAFETY: live instance per `shared`'s contract.
        let inst = unsafe { shared::<P>(_plugin) };
        if !inst.host.is_null() {
            // SAFETY: get_extension is [thread-safe], callable from init on
            // (host.h:20-25).
            let ext = unsafe {
                ((*inst.host).get_extension)(inst.host, crate::ffi::CLAP_EXT_PARAMS.as_ptr())
            };
            // SAFETY: main-thread-only field, written in init before any
            // possible reader (the gui/webview callbacks all come later).
            unsafe { *inst.host_params.get() = ext.cast() };

            // SAFETY: as above.
            let ext = unsafe {
                ((*inst.host).get_extension)(inst.host, crate::ffi::CLAP_EXT_STATE.as_ptr())
            };
            // SAFETY: as above.
            unsafe { *inst.host_state.get() = ext.cast() };
        }
    }
    true
}

/// `plugin.h:55-58` `[main-thread & !active]`.
unsafe extern "C" fn plugin_destroy<P: Plugin>(plugin: *const ClapPlugin) {
    if plugin.is_null() {
        return;
    }
    // SAFETY: `plugin` was created by `create::<P>` (this is the destroy slot
    // of that very vtable); the host must not use the pointer after this call
    // (plugin.h:55-58) and makes no concurrent calls, so re-owning the Box is
    // sound and runs `P`'s Drop.
    let data = unsafe { (*plugin).plugin_data.cast::<Instance<P>>() };
    let mut instance = unsafe { Box::from_raw(data) };
    #[cfg(debug_assertions)]
    {
        instance.trace_stop.store(true, Relaxed);
        if let Some(thread) = instance.trace_thread.take() {
            let _ = thread.join();
        }
    }
    drop(instance);
}

/// `plugin.h:60-71` `[main-thread & !active]`.
unsafe extern "C" fn plugin_activate<P: Plugin>(
    plugin: *const ClapPlugin,
    sample_rate: f64,
    _min_frames_count: u32,
    max_frames_count: u32,
) -> bool {
    // SAFETY: live instance per `shared`'s contract.
    let inst = unsafe { shared::<P>(plugin) };
    #[cfg(debug_assertions)]
    inst.trace.activations.fetch_add(1, Relaxed);
    // SAFETY: `[main-thread & !active]` — the plugin is not processing and
    // no other lifecycle call runs concurrently, so state access is exclusive.
    let state = unsafe { &mut *inst.state.get() };
    state.activate(sample_rate, max_frames_count);
    true
}

/// `plugin.h:72-73` `[main-thread & active]`.
unsafe extern "C" fn plugin_deactivate<P: Plugin>(plugin: *const ClapPlugin) {
    // SAFETY: live instance per `shared`'s contract.
    let inst = unsafe { shared::<P>(plugin) };
    // SAFETY: hosts must stop processing before deactivating, so the audio
    // thread is out of `process()` and state access is exclusive.
    let state = unsafe { &mut *inst.state.get() };
    state.deactivate();
}

/// `plugin.h:75-78` `[audio-thread & active & !processing]`.
unsafe extern "C" fn plugin_start_processing<P: Plugin>(plugin: *const ClapPlugin) -> bool {
    #[cfg(debug_assertions)]
    if !plugin.is_null() {
        // SAFETY: live instance per `shared`'s contract; atomics only.
        let inst = unsafe { shared::<P>(plugin) };
        inst.trace.processing_starts.fetch_add(1, Relaxed);
    }
    #[cfg(not(debug_assertions))]
    let _ = plugin;
    true
}

/// `plugin.h:80-82` `[audio-thread & active & processing]`.
unsafe extern "C" fn plugin_stop_processing(_plugin: *const ClapPlugin) {}

/// `plugin.h:84-90` `[audio-thread & active]`.
unsafe extern "C" fn plugin_reset<P: Plugin>(plugin: *const ClapPlugin) {
    // SAFETY: live instance per `shared`'s contract.
    let inst = unsafe { shared::<P>(plugin) };
    #[cfg(debug_assertions)]
    inst.trace.resets.fetch_add(1, Relaxed);
    // SAFETY: `[audio-thread]` — at most one audio thread exists per
    // instance (thread-check.h:30-40), so state access is exclusive.
    let state = unsafe { &mut *inst.state.get() };
    state.reset();
}

/// `plugin.h:92-97` `[audio-thread & active & processing]`.
///
/// The adapter copies input to output once (unless the host already processes
/// in place) and then hands the plugin *only* mutable output slices. Exposing
/// input and output separately would instantiate a `&[f32]` and a
/// `&mut [f32]` over the same memory whenever the host runs in place — UB in
/// Rust — so the in-place model is not just simpler, it is the sound one.
unsafe extern "C" fn plugin_process<P: Plugin>(
    plugin: *const ClapPlugin,
    process: *const ClapProcess,
) -> ClapProcessStatus {
    if plugin.is_null() || process.is_null() {
        return CLAP_PROCESS_ERROR;
    }
    // SAFETY: live instance per `shared`'s contract; `process` and everything
    // it points to stay valid until this call returns (plugin.h:92-94).
    let inst = unsafe { shared::<P>(plugin) };
    let process = unsafe { &*process };

    let transport = if process.transport.is_null() {
        None
    } else {
        // SAFETY: non-null transport is valid for this call (process.h:43-45).
        Some(unsafe { &*process.transport })
    };
    #[cfg(debug_assertions)]
    inst.trace.record(process, transport);

    // Events first, audio second: block-start application. Splitting the
    // block at event offsets is a known Phase 3+ refinement.
    // SAFETY: `in_events` is valid for this call.
    unsafe { apply_input_events(inst, process.in_events) };
    // GUI edits drain after host events: within one block a live touch on
    // the on-screen control wins; hosts pause automation during our
    // gesture anyway (that is what the gesture events are for).
    #[cfg(any(test, all(feature = "gui", target_os = "macos")))]
    // SAFETY: we are the single consumer (audio-thread role); out_events
    // valid for the call.
    unsafe {
        drain_gui_params::<P>(inst, process.out_events)
    };

    let frames = process.frames_count as usize;
    if frames == 0 {
        // Hosts issue zero-frame calls (e.g. flush-shaped); nothing to do.
        return CLAP_PROCESS_CONTINUE;
    }
    if process.audio_outputs_count == 0 || process.audio_outputs.is_null() {
        return CLAP_PROCESS_CONTINUE;
    }
    // SAFETY: `audio_outputs` has `audio_outputs_count` (>= 1) entries, so
    // index 0 is readable (process.h:47-52).
    let out: &ClapAudioBuffer = unsafe { &*process.audio_outputs };
    if out.data32.is_null() {
        // We declared 32-bit-only ports; a missing data32 means a host bug
        // or a 64-bit-only call. Skip rather than crash.
        return CLAP_PROCESS_CONTINUE;
    }
    let input: Option<&ClapAudioBuffer> =
        if process.audio_inputs_count > 0 && !process.audio_inputs.is_null() {
            // SAFETY: `audio_inputs` has `audio_inputs_count` (>= 1) entries.
            Some(unsafe { &*process.audio_inputs })
        } else {
            None
        };

    // Stack table of borrowed channel views. `&mut []` is a valid empty
    // slice, so the unfilled tail is harmless. No heap allocation here.
    let mut channels: [&mut [f32]; MAX_CHANNELS] = [&mut [], &mut []];
    let mut used = 0;
    for ch in 0..(out.channel_count as usize).min(MAX_CHANNELS) {
        // SAFETY: `data32` holds `channel_count` pointers (audio-buffer.h:26-33).
        let out_ptr = unsafe { *out.data32.add(ch) };
        if out_ptr.is_null() {
            break;
        }
        // Some hosts mirror one buffer into several channel slots. A second
        // `&mut` over the same memory is aliasing UB (and would double-apply
        // the gain), so the shared buffer is processed exactly once.
        if channels[..used].iter().any(|existing| ptr::eq(existing.as_ptr(), out_ptr)) {
            break;
        }
        let in_ptr = input
            .filter(|i| (ch as u32) < i.channel_count && !i.data32.is_null())
            // SAFETY: bounds just checked against the input's channel_count.
            .map(|i| unsafe { *i.data32.add(ch) })
            .filter(|p| !p.is_null());
        match in_ptr {
            Some(in_ptr) if !ptr::eq(in_ptr, out_ptr) => {
                // SAFETY: both buffers hold `frames` f32s valid for this call;
                // distinct host buffers do not overlap (the in-place case is
                // exact aliasing, excluded by the `ptr::eq` check above).
                unsafe { ptr::copy_nonoverlapping(in_ptr, out_ptr, frames) };
            }
            Some(_) => {} // already in place
            None => {
                // No matching input channel: feed silence, not garbage.
                // SAFETY: `out_ptr` is valid for `frames` writes.
                unsafe { ptr::write_bytes(out_ptr, 0, frames) };
            }
        }
        // SAFETY: `out_ptr` is valid for `frames` f32s and, per CLAP's
        // threading rules, this thread has exclusive access during process().
        channels[used] = unsafe { slice::from_raw_parts_mut(out_ptr, frames) };
        used += 1;
    }

    let mut audio = AudioBuffer::new(&mut channels[..used]);
    let transport = transport.map(convert_transport).unwrap_or_default();
    // Block-start snapshot: the plugin sees one coherent value per param for
    // the whole block (events above landed first). Stack array, no alloc.
    let mut params_snapshot = [0.0_f64; MAX_PARAMS];
    for (slot, bits) in params_snapshot.iter_mut().zip(&inst.param_bits).take(P::PARAMS.len()) {
        *slot = f64::from_bits(bits.load(Relaxed));
    }
    // SAFETY (state): `[audio-thread]` — at most one audio thread exists per
    // instance (thread-check.h:30-40); main-thread param callbacks touch only
    // atomics, never `state`. Exclusive.
    let state = unsafe { &mut *inst.state.get() };
    with_rt_context(transport, &params_snapshot[..P::PARAMS.len()], |rt| {
        // A state block published since the last block lands here, on the
        // audio thread, before the plugin processes with it — see
        // `plugin_state` for why the main thread cannot deliver it itself.
        // SAFETY: `[audio-thread]`, the single consumer.
        unsafe { inst.plugin_state.take_with(|bytes| state.apply_state(bytes, rt)) };
        state.process(&mut audio, rt)
    });
    CLAP_PROCESS_CONTINUE
}

/// `plugin.h:99-104` `[thread-safe]`.
unsafe extern "C" fn plugin_get_extension<P: Plugin>(
    _plugin: *const ClapPlugin,
    id: *const c_char,
) -> *const c_void {
    if id.is_null() {
        return ptr::null();
    }
    // SAFETY: the host passes a valid NUL-terminated extension id
    // (plugin.h:99-104).
    let id = unsafe { CStr::from_ptr(id) };
    if id == CLAP_EXT_AUDIO_PORTS {
        (audio_ports::VTABLE_REF as *const ClapPluginAudioPorts).cast()
    } else if id == CLAP_EXT_PARAMS {
        (ParamsImpl::<P>::VTABLE_REF as *const ClapPluginParams).cast()
    } else if id == CLAP_EXT_STATE {
        (StateImpl::<P>::VTABLE_REF as *const ClapPluginState).cast()
    } else {
        // A plugin without `Plugin::EDITOR` has no page to show: stay silent
        // about clap.gui rather than hand the host a vtable that refuses
        // every call.
        #[cfg(all(feature = "gui", target_os = "macos"))]
        if id == crate::ffi::CLAP_EXT_GUI && P::EDITOR.is_some() {
            return (crate::ext::gui::GuiImpl::<P>::VTABLE_REF as *const crate::ffi::ClapPluginGui)
                .cast();
        }
        #[cfg(all(feature = "gui", target_os = "macos"))]
        if id == crate::ffi::CLAP_EXT_TIMER_SUPPORT {
            return (crate::ext::gui::TimerImpl::<P>::VTABLE_REF
                as *const crate::ffi::ClapPluginTimerSupport)
                .cast();
        }
        ptr::null()
    }
}

/// `plugin.h:106-109` `[main-thread]`.
unsafe extern "C" fn plugin_on_main_thread(_plugin: *const ClapPlugin) {}

#[cfg(test)]
mod tests {
    use seco_core::{ParamDesc, ParamRange, RtContext};

    use super::*;
    use crate::ffi::{ClapIStream, ClapOStream};

    /// The single plugin type used by every test in this crate: the
    /// descriptor storage is one static (one plugin per binary), so tests
    /// must not mix plugin types.
    struct HalfGain {
        last_transport: Option<Transport>,
        /// Whatever `apply_state` last delivered, and how many times.
        applied: Option<Vec<u8>>,
        applications: usize,
    }

    impl Plugin for HalfGain {
        const ID: &'static str = "dev.seco.test.half-gain";
        const NAME: &'static str = "half-gain";
        const VENDOR: &'static str = "SECO tests";
        const VERSION: &'static str = "0.0.0";
        const PARAMS: &'static [ParamDesc] = &[ParamDesc {
            name: "Test",
            range: ParamRange::Continuous { min: 0.0, max: 1.0, default: 0.5, unit: "", decimals: 2 },
        }];

        fn new() -> Self {
            HalfGain { last_transport: None, applied: None, applications: 0 }
        }

        fn apply_state(&mut self, state: &[u8], _rt: &RtContext) {
            // A real plugin parses into storage from activate(); a Vec here
            // is fine because these tests never arm the allocation detector.
            self.applied = Some(state.to_vec());
            self.applications += 1;
        }

        fn process(&mut self, audio: &mut AudioBuffer, rt: &RtContext) {
            self.last_transport = Some(*rt.transport());
            for channel in audio.channels_mut() {
                for sample in channel {
                    *sample *= 0.5;
                }
            }
        }
    }

    /// Reads back plugin state after a vtable call.
    ///
    /// SAFETY-wise this mirrors the audio callbacks: the test is
    /// single-threaded and no other borrow of `state` is live.
    unsafe fn state_of(plugin: *const ClapPlugin) -> &'static HalfGain {
        unsafe { &*shared::<HalfGain>(plugin).state.get() }
    }

    fn stereo_buffer(ptrs: &mut [*mut f32; 2]) -> ClapAudioBuffer {
        ClapAudioBuffer {
            data32: ptrs.as_mut_ptr(),
            data64: ptr::null_mut(),
            channel_count: 2,
            latency: 0,
            constant_mask: 0,
        }
    }

    fn process_struct(
        frames: u32,
        inputs: Option<*const ClapAudioBuffer>,
        outputs: *mut ClapAudioBuffer,
        transport: *const ClapEventTransport,
        in_events: *const ClapInputEvents,
    ) -> ClapProcess {
        ClapProcess {
            steady_time: -1,
            frames_count: frames,
            transport,
            audio_inputs: inputs.unwrap_or(ptr::null()),
            audio_outputs: outputs,
            audio_inputs_count: u32::from(inputs.is_some()),
            audio_outputs_count: 1,
            in_events,
            out_events: ptr::null(),
        }
    }

    /// A `clap_ostream` collecting into a Vec, and a `clap_istream`
    /// handing bytes back a few at a time — hosts are allowed to do that
    /// (stream.h:10-16) and clap-validator does.
    struct SaveSink(Vec<u8>);

    unsafe extern "C" fn sink_write(
        stream: *const ClapOStream,
        buffer: *const std::ffi::c_void,
        size: u64,
    ) -> i64 {
        // SAFETY: ctx is the SaveSink we installed; buffer holds `size`
        // bytes for the call.
        unsafe {
            let sink = &mut *(*stream).ctx.cast::<SaveSink>();
            let bytes = std::slice::from_raw_parts(buffer.cast::<u8>(), size as usize);
            sink.0.extend_from_slice(bytes);
        }
        size as i64
    }

    struct LoadSource {
        bytes: Vec<u8>,
        offset: usize,
    }

    unsafe extern "C" fn source_read(
        stream: *const ClapIStream,
        buffer: *mut std::ffi::c_void,
        size: u64,
    ) -> i64 {
        // SAFETY: ctx is the LoadSource we installed; buffer is valid for
        // `size` bytes.
        unsafe {
            let source = &mut *(*stream).ctx.cast::<LoadSource>();
            // Deliberately dribble: 7 bytes at a time exercises the loop.
            let n = (source.bytes.len() - source.offset).min(size as usize).min(7);
            std::ptr::copy_nonoverlapping(
                source.bytes.as_ptr().add(source.offset),
                buffer.cast::<u8>(),
                n,
            );
            source.offset += n;
            n as i64
        }
    }

    fn save_state(plugin: *const ClapPlugin) -> Vec<u8> {
        let mut sink = SaveSink(Vec::new());
        let stream =
            ClapOStream { ctx: (&raw mut sink).cast(), write: sink_write };
        // SAFETY: live instance; the stream outlives the call.
        let ok = unsafe {
            (StateImpl::<HalfGain>::VTABLE.save)(plugin, &raw const stream)
        };
        assert!(ok, "save failed");
        sink.0
    }

    fn load_state(plugin: *const ClapPlugin, bytes: Vec<u8>) -> bool {
        let mut source = LoadSource { bytes, offset: 0 };
        let stream =
            ClapIStream { ctx: (&raw mut source).cast(), read: source_read };
        // SAFETY: live instance; the stream outlives the call.
        unsafe { (StateImpl::<HalfGain>::VTABLE.load)(plugin, &raw const stream) }
    }

    /// Runs one silent block, which is where a published state block is
    /// delivered to the plugin.
    fn run_one_block(plugin: *const ClapPlugin) {
        let mut left = vec![0.0_f32; 8];
        let mut right = vec![0.0_f32; 8];
        let mut ptrs = [left.as_mut_ptr(), right.as_mut_ptr()];
        let mut out = stereo_buffer(&mut ptrs);
        let process = process_struct(8, None, &raw mut out, ptr::null(), ptr::null());
        // SAFETY: live instance; everything referenced outlives the call.
        unsafe { ((*plugin).process)(plugin, &raw const process) };
    }

    /// The point of the whole mechanism: a block stored on one instance
    /// comes back on another, and reaches the plugin on the audio thread
    /// rather than through a main-thread `&mut`.
    #[test]
    fn plugin_state_survives_save_and_load() {
        let saver = create::<HalfGain>(ptr::null());
        let block = b"curve:0.1,0.2,0.3".to_vec();
        // SAFETY: [main-thread] role, single-threaded test.
        assert!(unsafe { shared::<HalfGain>(saver).plugin_state.publish(&block) });
        let blob = save_state(saver);
        // SAFETY: created above, not used again.
        unsafe { ((*saver).destroy)(saver) };

        let loader = create::<HalfGain>(ptr::null());
        assert!(load_state(loader, blob));
        // Nothing reaches the plugin until it processes: the main thread
        // must not touch plugin state.
        // SAFETY: single-threaded test, no other borrow live.
        assert!(unsafe { state_of(loader) }.applied.is_none());

        run_one_block(loader);
        // SAFETY: as above.
        let state = unsafe { state_of(loader) };
        assert_eq!(state.applied.as_deref(), Some(block.as_slice()));
        assert_eq!(state.applications, 1);

        // And it is delivered once, not on every block.
        run_one_block(loader);
        // SAFETY: as above.
        assert_eq!(unsafe { state_of(loader) }.applications, 1);
        // SAFETY: created above, not used again.
        unsafe { ((*loader).destroy)(loader) };
    }

    /// The editor's wire protocol, tested here rather than next to the
    /// WebKit plumbing so it runs on every platform.
    #[test]
    fn the_editor_protocol_parses_state_blocks() {
        use gui_queue::{EditorMsg, GuiMsg, parse_msg};

        // A block is taken verbatim: spaces, commas, whatever the page
        // chose. The framework does not read it.
        assert!(matches!(
            parse_msg("state 0.1,0.2, 0.3"),
            Some(EditorMsg::State("0.1,0.2, 0.3"))
        ));
        // Clearing the block is a message, not a missing one.
        assert!(matches!(parse_msg("state"), Some(EditorMsg::State(""))));
        assert!(matches!(parse_msg("state "), Some(EditorMsg::State(""))));

        // Parameters still take their own path.
        assert!(matches!(
            parse_msg("set 2 0.5"),
            Some(EditorMsg::Param(GuiMsg::Set(2, value))) if value == 0.5
        ));
        assert!(matches!(
            parse_msg("begin 1"),
            Some(EditorMsg::Param(GuiMsg::GestureBegin(1)))
        ));

        // Junk is dropped, not guessed at: this input comes from a webview.
        assert!(parse_msg("").is_none());
        assert!(parse_msg("stateful 1").is_none());
        assert!(parse_msg("set 2 nan").is_none());
        assert!(parse_msg("set two 0.5").is_none());
        assert!(parse_msg("set 2").is_none());
    }

    /// The editor writing state is the point of the whole channel: the
    /// bytes must reach the plugin on the audio thread, unchanged.
    #[test]
    fn an_editor_state_block_reaches_the_plugin() {
        let plugin = create::<HalfGain>(ptr::null());
        // SAFETY: live instance; [main-thread] role in a single-threaded
        // test, which is where webview messages arrive.
        unsafe { publish_editor_state::<HalfGain>(plugin, b"0.1,0.2,0.3") };
        run_one_block(plugin);

        // SAFETY: single-threaded test, no other borrow live.
        assert_eq!(unsafe { state_of(plugin) }.applied.as_deref(), Some(&b"0.1,0.2,0.3"[..]));

        // And it is what `clap.state` saves: an editor edit survives the
        // session even though the plugin never handed anything back.
        let blob = save_state(plugin);
        assert!(
            blob.windows(11).any(|window| window == b"0.1,0.2,0.3"),
            "the edited block must be in the saved state"
        );
        // SAFETY: created above, not used again.
        unsafe { ((*plugin).destroy)(plugin) };
    }

    /// A page can always send more than the slot holds. Dropping the block
    /// keeps the previous one; truncating would hand the plugin a curve
    /// that is silently half a curve.
    #[test]
    fn an_oversized_editor_block_is_dropped_not_truncated() {
        let plugin = create::<HalfGain>(ptr::null());
        // SAFETY: as in the test above.
        unsafe { publish_editor_state::<HalfGain>(plugin, b"good") };
        run_one_block(plugin);

        let too_big = vec![b'9'; crate::MAX_PLUGIN_STATE + 1];
        // SAFETY: as above.
        unsafe { publish_editor_state::<HalfGain>(plugin, &too_big) };
        run_one_block(plugin);

        // SAFETY: single-threaded test, no other borrow live.
        let state = unsafe { state_of(plugin) };
        assert_eq!(state.applied.as_deref(), Some(&b"good"[..]));
        assert_eq!(state.applications, 1, "nothing new should have been delivered");
        // SAFETY: created above, not used again.
        unsafe { ((*plugin).destroy)(plugin) };
    }

    /// A state block is not implicitly dirty the way a parameter change is
    /// (ext/state.h:37-38): without `mark_dirty` the host lets the user
    /// close the session and the edit is gone. Needs the editor build —
    /// `cargo test -p seco-clap --features gui` — since that is where the
    /// host lookup lives.
    #[cfg(all(feature = "gui", target_os = "macos"))]
    #[test]
    fn an_editor_state_block_marks_the_session_dirty() {
        use std::sync::atomic::AtomicUsize;

        static DIRTY: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn mark_dirty(_host: *const ClapHost) {
            DIRTY.fetch_add(1, Relaxed);
        }
        unsafe extern "C" fn get_extension(
            _host: *const ClapHost,
            id: *const c_char,
        ) -> *const std::ffi::c_void {
            static HOST_STATE: crate::ffi::ClapHostState =
                crate::ffi::ClapHostState { mark_dirty };
            // SAFETY: the plugin passes a NUL-terminated extension id.
            if unsafe { CStr::from_ptr(id) } == crate::ffi::CLAP_EXT_STATE {
                (&raw const HOST_STATE).cast()
            } else {
                ptr::null()
            }
        }
        unsafe extern "C" fn noop(_host: *const ClapHost) {}

        let host = ClapHost {
            clap_version: crate::ffi::CLAP_VERSION,
            host_data: ptr::null_mut(),
            name: c"seco-tests".as_ptr(),
            vendor: c"".as_ptr(),
            url: c"".as_ptr(),
            version: c"0.0.0".as_ptr(),
            get_extension,
            request_restart: noop,
            request_process: noop,
            request_callback: noop,
        };

        let plugin = create::<HalfGain>(&raw const host);
        // SAFETY: live instance; init is where host extensions are resolved.
        assert!(unsafe { ((*plugin).init)(plugin) });

        // SAFETY: [main-thread] role in a single-threaded test.
        unsafe { publish_editor_state::<HalfGain>(plugin, b"edited") };
        assert_eq!(DIRTY.load(Relaxed), 1, "the host was never told to save");

        // A block that does not fit is not state worth saving.
        let too_big = vec![b'9'; crate::MAX_PLUGIN_STATE + 1];
        // SAFETY: as above.
        unsafe { publish_editor_state::<HalfGain>(plugin, &too_big) };
        assert_eq!(DIRTY.load(Relaxed), 1, "a dropped block must not mark the session dirty");

        // SAFETY: created above, not used again.
        unsafe { ((*plugin).destroy)(plugin) };
    }

    /// A session saved before the plugin had a state block must *clear* it,
    /// not leave the previous one in place — otherwise loading an old
    /// preset inherits whatever the last one drew.
    #[test]
    fn a_version_1_state_clears_the_plugin_block() {
        let plugin = create::<HalfGain>(ptr::null());
        // SAFETY: [main-thread] role, single-threaded test.
        assert!(unsafe { shared::<HalfGain>(plugin).plugin_state.publish(b"stale") });
        run_one_block(plugin);

        // A hand-built v1 blob: magic, version 1, one parameter, no block.
        let mut blob = b"SECO".to_vec();
        blob.extend_from_slice(&1_u16.to_le_bytes());
        blob.extend_from_slice(&1_u16.to_le_bytes());
        blob.extend_from_slice(&0_u32.to_le_bytes());
        blob.extend_from_slice(&0.25_f64.to_bits().to_le_bytes());
        assert!(load_state(plugin, blob));
        run_one_block(plugin);

        // SAFETY: single-threaded test, no other borrow live.
        let state = unsafe { state_of(plugin) };
        assert_eq!(state.applied.as_deref(), Some(&[][..]), "the old block must be cleared");
        assert_eq!(state.applications, 2);
        // SAFETY: created above, not used again.
        unsafe { ((*plugin).destroy)(plugin) };
    }

    /// A corrupt or hostile length must fail the load, not truncate the
    /// block or read past the blob.
    #[test]
    fn a_lying_block_length_fails_the_load() {
        let plugin = create::<HalfGain>(ptr::null());
        let mut header = b"SECO".to_vec();
        header.extend_from_slice(&2_u16.to_le_bytes());
        header.extend_from_slice(&0_u16.to_le_bytes());

        // Longer than the blob actually carries.
        let mut truncated = header.clone();
        truncated.extend_from_slice(&64_u32.to_le_bytes());
        truncated.extend_from_slice(b"short");
        assert!(!load_state(plugin, truncated));

        // Longer than the slot can ever hold.
        let mut oversized = header.clone();
        oversized.extend_from_slice(&(crate::MAX_PLUGIN_STATE as u32 + 1).to_le_bytes());
        assert!(!load_state(plugin, oversized));

        // Missing the length word entirely.
        assert!(!load_state(plugin, header));

        run_one_block(plugin);
        // SAFETY: single-threaded test, no other borrow live.
        assert_eq!(unsafe { state_of(plugin) }.applications, 0, "a failed load must publish nothing");
        // SAFETY: created above, not used again.
        unsafe { ((*plugin).destroy)(plugin) };
    }

    #[test]
    fn out_of_place_copies_input_then_processes() {
        let plugin = create::<HalfGain>(ptr::null());
        let mut in_l = vec![1.0_f32; 64];
        let mut in_r = vec![-1.0_f32; 64];
        let mut out_l = vec![9.9_f32; 64];
        let mut out_r = vec![9.9_f32; 64];
        let mut in_ptrs = [in_l.as_mut_ptr(), in_r.as_mut_ptr()];
        let mut out_ptrs = [out_l.as_mut_ptr(), out_r.as_mut_ptr()];
        let in_buf = stereo_buffer(&mut in_ptrs);
        let mut out_buf = stereo_buffer(&mut out_ptrs);
        let process =
            process_struct(64, Some(&raw const in_buf), &raw mut out_buf, ptr::null(), ptr::null());

        // SAFETY: `plugin` is a live instance from `create`; `process` and
        // everything it references outlive the call.
        let status = unsafe { ((*plugin).process)(plugin, &raw const process) };

        assert_eq!(status, CLAP_PROCESS_CONTINUE);
        assert!(out_l.iter().all(|&s| s == 0.5));
        assert!(out_r.iter().all(|&s| s == -0.5));
        assert!(in_l.iter().all(|&s| s == 1.0), "input must stay untouched");
        // SAFETY: created above; not used again after this call.
        unsafe { ((*plugin).destroy)(plugin) };
    }

    /// The path clap-validator's out-of-place test cannot reach: the host
    /// hands over the *same* sample memory as input and output
    /// (`in_place_pair`). The adapter must skip the self-copy — a
    /// `copy_nonoverlapping` onto itself would be UB — and process once.
    #[test]
    fn in_place_skips_self_copy_and_processes_once() {
        let plugin = create::<HalfGain>(ptr::null());
        let mut l = vec![0.8_f32; 32];
        let mut r = vec![0.4_f32; 32];
        // One raw pointer per channel, shared by both tables, exactly like a
        // host reusing the buffer with two `clap_audio_buffer` structs.
        let lp = l.as_mut_ptr();
        let rp = r.as_mut_ptr();
        let mut in_ptrs = [lp, rp];
        let mut out_ptrs = [lp, rp];
        let in_buf = stereo_buffer(&mut in_ptrs);
        let mut out_buf = stereo_buffer(&mut out_ptrs);
        let process =
            process_struct(32, Some(&raw const in_buf), &raw mut out_buf, ptr::null(), ptr::null());

        // SAFETY: as in `out_of_place_copies_input_then_processes`.
        let status = unsafe { ((*plugin).process)(plugin, &raw const process) };

        assert_eq!(status, CLAP_PROCESS_CONTINUE);
        assert!(l.iter().all(|&s| s == 0.4), "halved exactly once");
        assert!(r.iter().all(|&s| s == 0.2), "halved exactly once");
        // SAFETY: created above; not used again after this call.
        unsafe { ((*plugin).destroy)(plugin) };
    }

    #[test]
    fn zero_frames_is_a_no_op() {
        let plugin = create::<HalfGain>(ptr::null());
        let mut l = vec![1.0_f32; 4];
        let mut r = vec![1.0_f32; 4];
        let mut out_ptrs = [l.as_mut_ptr(), r.as_mut_ptr()];
        let mut out_buf = stereo_buffer(&mut out_ptrs);
        let process = process_struct(0, None, &raw mut out_buf, ptr::null(), ptr::null());

        // SAFETY: as in `out_of_place_copies_input_then_processes`.
        let status = unsafe { ((*plugin).process)(plugin, &raw const process) };

        assert_eq!(status, CLAP_PROCESS_CONTINUE);
        assert!(l.iter().all(|&s| s == 1.0));
        // SAFETY: created above; not used again after this call.
        unsafe { ((*plugin).destroy)(plugin) };
    }

    #[test]
    fn missing_input_yields_silence_not_garbage() {
        let plugin = create::<HalfGain>(ptr::null());
        let mut l = vec![7.0_f32; 16];
        let mut r = vec![7.0_f32; 16];
        let mut out_ptrs = [l.as_mut_ptr(), r.as_mut_ptr()];
        let mut out_buf = stereo_buffer(&mut out_ptrs);
        let process = process_struct(16, None, &raw mut out_buf, ptr::null(), ptr::null());

        // SAFETY: as in `out_of_place_copies_input_then_processes`.
        let status = unsafe { ((*plugin).process)(plugin, &raw const process) };

        assert_eq!(status, CLAP_PROCESS_CONTINUE);
        assert!(l.iter().all(|&s| s == 0.0));
        assert!(r.iter().all(|&s| s == 0.0));
        // SAFETY: created above; not used again after this call.
        unsafe { ((*plugin).destroy)(plugin) };
    }

    #[test]
    fn transport_converts_fixed_point_and_flags() {
        let plugin = create::<HalfGain>(ptr::null());
        let mut l = vec![0.0_f32; 8];
        let mut r = vec![0.0_f32; 8];
        let mut out_ptrs = [l.as_mut_ptr(), r.as_mut_ptr()];
        let mut out_buf = stereo_buffer(&mut out_ptrs);
        let tp = ClapEventTransport {
            header: crate::ffi::ClapEventHeader {
                size: size_of::<ClapEventTransport>() as u32,
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_TRANSPORT,
                flags: 0,
            },
            flags: CLAP_TRANSPORT_HAS_TEMPO
                | CLAP_TRANSPORT_HAS_BEATS_TIMELINE
                | CLAP_TRANSPORT_HAS_SECONDS_TIMELINE
                | CLAP_TRANSPORT_HAS_TIME_SIGNATURE
                | CLAP_TRANSPORT_IS_PLAYING,
            song_pos_beats: 5 * CLAP_BEATTIME_FACTOR / 2, // 2.5 beats
            song_pos_seconds: 5 * CLAP_SECTIME_FACTOR / 4, // 1.25 s
            tempo: 120.0,
            tempo_inc: 0.0,
            loop_start_beats: 0,
            loop_end_beats: 0,
            loop_start_seconds: 0,
            loop_end_seconds: 0,
            bar_start: 0,
            bar_number: 0,
            tsig_num: 6,
            tsig_denom: 8,
        };
        let process =
            process_struct(8, None, &raw mut out_buf, &raw const tp, ptr::null());

        // SAFETY: as in `out_of_place_copies_input_then_processes`.
        let status = unsafe { ((*plugin).process)(plugin, &raw const process) };
        assert_eq!(status, CLAP_PROCESS_CONTINUE);

        // SAFETY: single-threaded test, no live borrow of state.
        let seen = unsafe { state_of(plugin) }.last_transport;
        assert_eq!(
            seen,
            Some(Transport {
                tempo_bpm: Some(120.0),
                song_pos_beats: Some(2.5),
                song_pos_seconds: Some(1.25),
                time_signature: Some((6, 8)),
                playing: true,
            })
        );
        // SAFETY: created above; not used again after this call.
        unsafe { ((*plugin).destroy)(plugin) };
    }

    #[test]
    fn null_transport_reads_as_default() {
        let plugin = create::<HalfGain>(ptr::null());
        let mut l = vec![0.0_f32; 8];
        let mut r = vec![0.0_f32; 8];
        let mut out_ptrs = [l.as_mut_ptr(), r.as_mut_ptr()];
        let mut out_buf = stereo_buffer(&mut out_ptrs);
        let process = process_struct(8, None, &raw mut out_buf, ptr::null(), ptr::null());

        // SAFETY: as in `out_of_place_copies_input_then_processes`.
        unsafe { ((*plugin).process)(plugin, &raw const process) };

        // SAFETY: single-threaded test, no live borrow of state.
        let seen = unsafe { state_of(plugin) }.last_transport;
        assert_eq!(seen, Some(Transport::default()));
        // SAFETY: created above; not used again after this call.
        unsafe { ((*plugin).destroy)(plugin) };
    }

    /// A minimal host-side event list: `ctx` points at the pointer array.
    unsafe extern "C" fn list_size(list: *const ClapInputEvents) -> u32 {
        // SAFETY: `ctx` points to the Vec set up by the test.
        let events = unsafe { &*(*list).ctx.cast::<Vec<*const crate::ffi::ClapEventHeader>>() };
        events.len() as u32
    }

    unsafe extern "C" fn list_get(
        list: *const ClapInputEvents,
        index: u32,
    ) -> *const crate::ffi::ClapEventHeader {
        // SAFETY: as in `list_size`; the caller-side contract is
        // `index < size()`, and the test upholds it.
        let events = unsafe { &*(*list).ctx.cast::<Vec<*const crate::ffi::ClapEventHeader>>() };
        events[index as usize]
    }

    /// The spec promises events are contiguous memcpy-able blobs
    /// (events.h:14-17) — it never promises *alignment*. A host with a
    /// packed event queue can legally hand a param event at an address not
    /// aligned for its f64 fields; reading it through a reference is UB
    /// (miri flags it), `read_unaligned` is not.
    #[test]
    fn param_event_at_unaligned_address_is_read_safely() {
        let plugin = create::<HalfGain>(ptr::null());
        // Backing aligned to 8; the event is planted at +4, so its f64
        // lands misaligned. 4 + 56 (event size) fits in 64 bytes.
        let mut backing = [0_u64; 8];
        let event = ClapEventParamValue {
            header: crate::ffi::ClapEventHeader {
                size: size_of::<ClapEventParamValue>() as u32,
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_VALUE,
                flags: 0,
            },
            param_id: 0,
            cookie: ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value: 0.25,
        };
        let misaligned = unsafe { backing.as_mut_ptr().cast::<u8>().add(4) }
            .cast::<ClapEventParamValue>();
        // SAFETY: offset 4 + size 56 <= 64 bytes of backing; unaligned write
        // is explicitly fine.
        unsafe { misaligned.write_unaligned(event) };
        let mut ptrs: Vec<*const crate::ffi::ClapEventHeader> = vec![misaligned.cast()];
        let in_events = ClapInputEvents {
            ctx: (&raw mut ptrs).cast(),
            size: list_size,
            get: list_get,
        };

        // SAFETY: live instance; list valid for the call.
        let inst = unsafe { shared::<HalfGain>(plugin) };
        unsafe { apply_input_events(inst, &raw const in_events) };

        assert_eq!(f64::from_bits(inst.param_bits[0].load(Relaxed)), 0.25);
        // SAFETY: created above; not used again after this call.
        unsafe { ((*plugin).destroy)(plugin) };
    }

    /// Hosts sometimes mirror one buffer into both channel slots
    /// (`data32[0] == data32[1]`). Building two `&mut [f32]` over that
    /// memory is aliasing UB before a single sample moves, and processing
    /// it twice doubles the gain. The adapter must collapse duplicates and
    /// process the shared buffer exactly once.
    #[test]
    fn duplicate_channel_pointers_process_once() {
        let plugin = create::<HalfGain>(ptr::null());
        let mut mono = vec![1.0_f32; 32];
        let mp = mono.as_mut_ptr();
        let mut in_ptrs = [mp, mp];
        let mut out_ptrs = [mp, mp];
        let in_buf = stereo_buffer(&mut in_ptrs);
        let mut out_buf = stereo_buffer(&mut out_ptrs);
        let process =
            process_struct(32, Some(&raw const in_buf), &raw mut out_buf, ptr::null(), ptr::null());

        // SAFETY: as in `out_of_place_copies_input_then_processes`.
        let status = unsafe { ((*plugin).process)(plugin, &raw const process) };

        assert_eq!(status, CLAP_PROCESS_CONTINUE);
        assert!(
            mono.iter().all(|&s| s == 0.5),
            "shared buffer must be halved exactly once, got {}",
            mono[0]
        );
        // SAFETY: created above; not used again after this call.
        unsafe { ((*plugin).destroy)(plugin) };
    }

    #[test]
    fn param_value_event_lands_in_the_atomic() {
        let plugin = create::<HalfGain>(ptr::null());
        let mut l = vec![0.0_f32; 8];
        let mut r = vec![0.0_f32; 8];
        let mut out_ptrs = [l.as_mut_ptr(), r.as_mut_ptr()];
        let mut out_buf = stereo_buffer(&mut out_ptrs);

        let event = ClapEventParamValue {
            header: crate::ffi::ClapEventHeader {
                size: size_of::<ClapEventParamValue>() as u32,
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_VALUE,
                flags: 0,
            },
            param_id: 0,
            cookie: ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value: 0.75,
        };
        let mut ptrs: Vec<*const crate::ffi::ClapEventHeader> =
            vec![(&raw const event).cast()];
        let in_events = ClapInputEvents {
            ctx: (&raw mut ptrs).cast(),
            size: list_size,
            get: list_get,
        };
        let process =
            process_struct(8, None, &raw mut out_buf, ptr::null(), &raw const in_events);

        // SAFETY: as in `out_of_place_copies_input_then_processes`.
        unsafe { ((*plugin).process)(plugin, &raw const process) };

        // SAFETY: live instance; atomics are shared-safe.
        let inst = unsafe { shared::<HalfGain>(plugin) };
        assert_eq!(f64::from_bits(inst.param_bits[0].load(Relaxed)), 0.75);

        // And the params vtable must report the same through get_value.
        let mut read_back = 0.0_f64;
        // SAFETY: live instance; out pointer valid for one write.
        let ok = unsafe {
            (ParamsImpl::<HalfGain>::VTABLE.get_value)(
                plugin,
                0,
                &raw mut read_back,
            )
        };
        assert!(ok);
        assert_eq!(read_back, 0.75);
        // SAFETY: created above; not used again after this call.
        unsafe { ((*plugin).destroy)(plugin) };
    }

    /// The hazard that forced the shared-borrow redesign, actually exercised:
    /// the host reads the parameter from the main thread (`get_value`,
    /// `[main-thread]`) *while* the audio thread is inside `process()`. CLAP
    /// explicitly permits this pair to overlap. Under miri this runs with the
    /// data-race detector: the old `&mut Instance` model fails here, the
    /// shared-borrow + atomic + UnsafeCell model must pass, and reads must
    /// never be torn (only whole written values observable).
    ///
    /// `params.flush` is deliberately NOT exercised concurrently — the spec
    /// forbids it running at the same time as `process()`
    /// (ext/params.h:295-296), so that overlap is outside the host contract.
    #[test]
    fn get_value_races_process_without_ub() {
        const ITERS: usize = if cfg!(miri) { 64 } else { 2000 };

        struct SendPlugin(*const ClapPlugin);
        // SAFETY: the instance outlives both threads (destroy happens after
        // join), and the two threads invoke exactly the callback pair CLAP
        // permits to run concurrently for one instance.
        unsafe impl Send for SendPlugin {}

        let plugin = create::<HalfGain>(ptr::null());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let reader = {
            let plugin = SendPlugin(plugin);
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                // Move the whole wrapper, not just its field: closures
                // capture disjoint fields since Rust 2021, and capturing the
                // bare `*const` would sidestep SendPlugin's `Send` impl.
                let plugin = plugin;
                let SendPlugin(plugin) = plugin;
                barrier.wait();
                for _ in 0..ITERS {
                    let mut value = f64::NAN;
                    // SAFETY: live instance; concurrent with `process()` by
                    // design, which the spec allows for `get_value`.
                    let ok = unsafe {
                        (ParamsImpl::<HalfGain>::VTABLE.get_value)(
                            plugin,
                            0,
                            &raw mut value,
                        )
                    };
                    assert!(ok);
                    assert!(
                        value == 0.5 || value == 0.25 || value == 0.75,
                        "torn or corrupt read: {value}"
                    );
                }
            })
        };

        let mut l = vec![0.0_f32; 16];
        let mut r = vec![0.0_f32; 16];
        barrier.wait();
        for i in 0..ITERS {
            let event = ClapEventParamValue {
                header: crate::ffi::ClapEventHeader {
                    size: size_of::<ClapEventParamValue>() as u32,
                    time: 0,
                    space_id: CLAP_CORE_EVENT_SPACE_ID,
                    type_: CLAP_EVENT_PARAM_VALUE,
                    flags: 0,
                },
                param_id: 0,
                cookie: ptr::null_mut(),
                note_id: -1,
                port_index: -1,
                channel: -1,
                key: -1,
                value: if i % 2 == 0 { 0.25 } else { 0.75 },
            };
            let mut ptrs: Vec<*const crate::ffi::ClapEventHeader> =
                vec![(&raw const event).cast()];
            let in_events = ClapInputEvents {
                ctx: (&raw mut ptrs).cast(),
                size: list_size,
                get: list_get,
            };
            let mut out_ptrs = [l.as_mut_ptr(), r.as_mut_ptr()];
            let mut out_buf = stereo_buffer(&mut out_ptrs);
            let process =
                process_struct(16, None, &raw mut out_buf, ptr::null(), &raw const in_events);
            // SAFETY: live instance; audio-thread role held by this thread
            // only, per the test's structure.
            let status = unsafe { ((*plugin).process)(plugin, &raw const process) };
            assert_eq!(status, CLAP_PROCESS_CONTINUE);
        }

        reader.join().unwrap();
        // SAFETY: created above, both threads joined; not used again.
        unsafe { ((*plugin).destroy)(plugin) };
    }
    /// Out-events sink that records every pushed event.
    #[derive(Default)]
    struct Collected {
        events: Vec<(u16, u32, f64, u32)>, // (type, param_id, value, header.flags)
        reject: bool,
    }

    unsafe extern "C" fn collect_push(
        list: *const ClapOutputEvents,
        event: *const crate::ffi::ClapEventHeader,
    ) -> bool {
        // SAFETY: ctx points to the test's Collected; event valid per contract.
        let sink = unsafe { &mut *(*list).ctx.cast::<Collected>() };
        if sink.reject {
            return false;
        }
        let head = unsafe { event.read_unaligned() };
        let (param_id, value) = match head.type_ {
            CLAP_EVENT_PARAM_VALUE => {
                let ev = unsafe { event.cast::<ClapEventParamValue>().read_unaligned() };
                (ev.param_id, ev.value)
            }
            _ => {
                let ev = unsafe { event.cast::<ClapEventParamGesture>().read_unaligned() };
                (ev.param_id, f64::NAN)
            }
        };
        sink.events.push((head.type_, param_id, value, head.flags));
        true
    }

    fn out_list(sink: &mut Collected) -> ClapOutputEvents {
        ClapOutputEvents { ctx: (sink as *mut Collected).cast(), try_push: collect_push }
    }

    fn run_silent_block(plugin: *const ClapPlugin, out: *const ClapOutputEvents) {
        let mut l = vec![0.0_f32; 8];
        let mut r = vec![0.0_f32; 8];
        let mut out_ptrs = [l.as_mut_ptr(), r.as_mut_ptr()];
        let mut out_buf = stereo_buffer(&mut out_ptrs);
        let mut process = process_struct(8, None, &raw mut out_buf, ptr::null(), ptr::null());
        process.out_events = out;
        // SAFETY: live instance; process struct valid for the call.
        unsafe { ((*plugin).process)(plugin, &raw const process) };
    }

    /// The GUI drain writes the audio-visible atomic FIRST, then announces
    /// begin/value/end to the host, all tagged IS_LIVE.
    #[test]
    fn gui_edit_updates_atomic_and_notifies_host_in_order() {
        use gui_queue::GuiMsg;
        let plugin = create::<HalfGain>(ptr::null());
        // SAFETY: main-thread contract holds (single-threaded test).
        unsafe {
            queue_gui_param_change::<HalfGain>(plugin, GuiMsg::GestureBegin(0));
            queue_gui_param_change::<HalfGain>(plugin, GuiMsg::Set(0, 0.75));
            queue_gui_param_change::<HalfGain>(plugin, GuiMsg::GestureEnd(0));
        }
        let mut sink = Collected::default();
        let out = out_list(&mut sink);
        run_silent_block(plugin, &raw const out);

        // SAFETY: live instance; atomics shared-safe.
        let inst = unsafe { shared::<HalfGain>(plugin) };
        assert_eq!(f64::from_bits(inst.param_bits[0].load(Relaxed)), 0.75);
        let types: Vec<u16> = sink.events.iter().map(|e| e.0).collect();
        assert_eq!(
            types,
            vec![
                crate::ffi::CLAP_EVENT_PARAM_GESTURE_BEGIN,
                CLAP_EVENT_PARAM_VALUE,
                crate::ffi::CLAP_EVENT_PARAM_GESTURE_END
            ]
        );
        assert_eq!(sink.events[1].2, 0.75);
        assert!(
            sink.events.iter().all(|e| e.3 & crate::ffi::CLAP_EVENT_IS_LIVE != 0),
            "GUI edits are live user events"
        );
        // SAFETY: created above; not used again after this call.
        unsafe { ((*plugin).destroy)(plugin) };
    }

    /// A fast drag (many set messages between drains) coalesces to ONE
    /// value event carrying the latest value.
    #[test]
    fn gui_fast_drag_coalesces_to_latest() {
        use gui_queue::GuiMsg;
        let plugin = create::<HalfGain>(ptr::null());
        // SAFETY: as above.
        unsafe {
            queue_gui_param_change::<HalfGain>(plugin, GuiMsg::GestureBegin(0));
            for step in 0..=100 {
                queue_gui_param_change::<HalfGain>(plugin, GuiMsg::Set(0, step as f64 / 100.0));
            }
            queue_gui_param_change::<HalfGain>(plugin, GuiMsg::GestureEnd(0));
        }
        let mut sink = Collected::default();
        let out = out_list(&mut sink);
        run_silent_block(plugin, &raw const out);

        let values: Vec<f64> =
            sink.events.iter().filter(|e| e.0 == CLAP_EVENT_PARAM_VALUE).map(|e| e.2).collect();
        assert_eq!(values, vec![1.0], "must coalesce to a single latest-value event");
        // SAFETY: created above; not used again after this call.
        unsafe { ((*plugin).destroy)(plugin) };
    }

    /// If the host's event queue rejects the push, the audio atomic is
    /// already correct and the notification retries on the next drain.
    #[test]
    fn gui_notification_retries_after_full_host_queue() {
        use gui_queue::GuiMsg;
        let plugin = create::<HalfGain>(ptr::null());
        // SAFETY: as above.
        unsafe { queue_gui_param_change::<HalfGain>(plugin, GuiMsg::Set(0, 0.25)) };

        let mut sink = Collected { reject: true, ..Default::default() };
        let out = out_list(&mut sink);
        run_silent_block(plugin, &raw const out);
        // SAFETY: live instance.
        let inst = unsafe { shared::<HalfGain>(plugin) };
        assert_eq!(f64::from_bits(inst.param_bits[0].load(Relaxed)), 0.25, "audio first");
        assert!(sink.events.is_empty());

        sink.reject = false;
        let out = out_list(&mut sink);
        run_silent_block(plugin, &raw const out);
        let values: Vec<f64> =
            sink.events.iter().filter(|e| e.0 == CLAP_EVENT_PARAM_VALUE).map(|e| e.2).collect();
        assert_eq!(values, vec![0.25], "retried on the next drain");
        // SAFETY: created above; not used again after this call.
        unsafe { ((*plugin).destroy)(plugin) };
    }

    /// The GUI producer races the audio-thread drain under miri's race
    /// detector: the Release/Acquire pair on the pending flags must publish
    /// the value bits, and no read may tear.
    #[test]
    fn gui_queue_races_drain_without_ub() {
        use gui_queue::GuiMsg;
        const ITERS: usize = if cfg!(miri) { 48 } else { 1500 };

        struct SendPlugin(*const ClapPlugin);
        // SAFETY: instance outlives both threads (joined before destroy);
        // producer = one thread (main-thread role), consumer = this thread
        // (audio role) — the exact pair the design permits concurrently.
        unsafe impl Send for SendPlugin {}

        let plugin = create::<HalfGain>(ptr::null());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let producer = {
            let plugin = SendPlugin(plugin);
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let plugin = plugin;
                let SendPlugin(plugin) = plugin;
                barrier.wait();
                for step in 0..ITERS {
                    // SAFETY: serialized single producer, live instance.
                    unsafe {
                        queue_gui_param_change::<HalfGain>(
                            plugin,
                            GuiMsg::Set(0, (step % 100) as f64 / 100.0),
                        );
                    }
                }
            })
        };

        let mut sink = Collected::default();
        barrier.wait();
        for _ in 0..ITERS {
            let out = out_list(&mut sink);
            run_silent_block(plugin, &raw const out);
        }
        producer.join().unwrap();
        for (type_, _, value, _) in &sink.events {
            if *type_ == CLAP_EVENT_PARAM_VALUE {
                assert!((0.0..=1.0).contains(value), "torn or corrupt value: {value}");
            }
        }
        // SAFETY: created above, threads joined; not used again.
        unsafe { ((*plugin).destroy)(plugin) };
    }
}