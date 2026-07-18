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
    /// Plain parameter values as f64 bit patterns, indexed like
    /// `P::PARAMS`. Atomics because the main thread reads them (`get_value`,
    /// state save) while the audio thread applies events. Slots beyond
    /// `P::PARAMS.len()` are unused.
    pub(crate) param_bits: [AtomicU64; MAX_PARAMS],
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
            init: plugin_init,
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
        param_bits: std::array::from_fn(|index| {
            let default =
                P::PARAMS.get(index).map(|desc| desc.range.default_plain()).unwrap_or(0.0);
            AtomicU64::new(default.to_bits())
        }),
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
        let head = unsafe { *header };
        if head.space_id != CLAP_CORE_EVENT_SPACE_ID {
            continue;
        }
        match head.type_ {
            CLAP_EVENT_PARAM_VALUE if head.size as usize >= size_of::<ClapEventParamValue>() => {
                // SAFETY: size checked against the full event; CLAP events
                // are contiguous blobs of `size` bytes (events.h:14-17).
                let event = unsafe { &*header.cast::<ClapEventParamValue>() };
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
/// there is nothing to do.
unsafe extern "C" fn plugin_init(_plugin: *const ClapPlugin) -> bool {
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
        ptr::null()
    }
}

/// `plugin.h:106-109` `[main-thread]`.
unsafe extern "C" fn plugin_on_main_thread(_plugin: *const ClapPlugin) {}

#[cfg(test)]
mod tests {
    use seco_core::{ParamDesc, ParamRange, RtContext};

    use super::*;

    /// The single plugin type used by every test in this crate: the
    /// descriptor storage is one static (one plugin per binary), so tests
    /// must not mix plugin types.
    struct HalfGain {
        last_transport: Option<Transport>,
    }

    impl Plugin for HalfGain {
        const ID: &'static str = "dev.seco.test.half-gain";
        const NAME: &'static str = "half-gain";
        const VENDOR: &'static str = "SECO tests";
        const VERSION: &'static str = "0.0.0";
        const PARAMS: &'static [ParamDesc] = &[ParamDesc {
            name: "Test",
            range: ParamRange::Continuous { min: 0.0, max: 1.0, default: 0.5 },
        }];

        fn new() -> Self {
            HalfGain { last_transport: None }
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
}
