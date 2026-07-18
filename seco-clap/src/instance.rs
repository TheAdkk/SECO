//! The bridge between one live `clap_plugin` and its `Plugin` state.

use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::slice;

use seco_core::{AudioBuffer, Plugin, with_rt_context};

use crate::ext::audio_ports;
use crate::factory;
use crate::ffi::{
    CLAP_EXT_AUDIO_PORTS, CLAP_PROCESS_CONTINUE, CLAP_PROCESS_ERROR, ClapAudioBuffer, ClapPlugin,
    ClapPluginAudioPorts, ClapProcess, ClapProcessStatus,
};

/// Channel capacity of the stack-allocated slice table in `plugin_process`.
/// SECO v1 declares stereo ports, so 2 is exact; if a host hands over more
/// channels anyway, the excess is passed through untouched rather than
/// allocating.
const MAX_CHANNELS: usize = 2;

/// Heap-allocated per-instance data. `clap_plugin.plugin_data` points back to
/// this, giving every callback access to both the vtable struct and the
/// user's plugin state.
struct Instance<P: Plugin> {
    clap_plugin: ClapPlugin,
    state: P,
}

pub(crate) fn create<P: Plugin>() -> *const ClapPlugin {
    let instance = Box::new(Instance {
        clap_plugin: ClapPlugin {
            desc: factory::descriptor_for::<P>(),
            plugin_data: ptr::null_mut(),
            init: plugin_init,
            destroy: plugin_destroy::<P>,
            activate: plugin_activate::<P>,
            deactivate: plugin_deactivate::<P>,
            start_processing: plugin_start_processing,
            stop_processing: plugin_stop_processing,
            reset: plugin_reset::<P>,
            process: plugin_process::<P>,
            get_extension: plugin_get_extension,
            on_main_thread: plugin_on_main_thread,
        },
        state: P::new(),
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

/// Recovers the instance from the pointer the host passes back.
///
/// SAFETY contract for callers: `plugin` must be a pointer obtained from
/// [`create`] and not yet destroyed. Handing out `&mut` here is sound because
/// CLAP guarantees the callbacks that reach it never run concurrently for one
/// instance: `[main-thread]` calls are serialized on the main thread, the
/// audio-thread role is held by at most one OS thread at a time
/// (thread-check.h:30-40), and the lifecycle state machine (plugin.h) keeps
/// the two groups from overlapping.
unsafe fn instance_mut<'a, P: Plugin>(plugin: *const ClapPlugin) -> &'a mut Instance<P> {
    // SAFETY: per the contract above, `plugin_data` was set in `create` and
    // points to the owning `Instance<P>`.
    unsafe { &mut *(*plugin).plugin_data.cast::<Instance<P>>() }
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
    // (plugin.h:55-58), so reclaiming the Box is sound and runs `P`'s Drop.
    let data = unsafe { (*plugin).plugin_data.cast::<Instance<P>>() };
    drop(unsafe { Box::from_raw(data) });
}

/// `plugin.h:60-71` `[main-thread & !active]`.
unsafe extern "C" fn plugin_activate<P: Plugin>(
    plugin: *const ClapPlugin,
    sample_rate: f64,
    _min_frames_count: u32,
    max_frames_count: u32,
) -> bool {
    // SAFETY: valid instance per `instance_mut`'s contract.
    let inst = unsafe { instance_mut::<P>(plugin) };
    inst.state.activate(sample_rate, max_frames_count);
    true
}

/// `plugin.h:72-73` `[main-thread & active]`.
unsafe extern "C" fn plugin_deactivate<P: Plugin>(plugin: *const ClapPlugin) {
    // SAFETY: valid instance per `instance_mut`'s contract.
    let inst = unsafe { instance_mut::<P>(plugin) };
    inst.state.deactivate();
}

/// `plugin.h:75-78` `[audio-thread & active & !processing]`.
unsafe extern "C" fn plugin_start_processing(_plugin: *const ClapPlugin) -> bool {
    true
}

/// `plugin.h:80-82` `[audio-thread & active & processing]`.
unsafe extern "C" fn plugin_stop_processing(_plugin: *const ClapPlugin) {}

/// `plugin.h:84-90` `[audio-thread & active]`.
unsafe extern "C" fn plugin_reset<P: Plugin>(plugin: *const ClapPlugin) {
    // SAFETY: valid instance per `instance_mut`'s contract.
    let inst = unsafe { instance_mut::<P>(plugin) };
    inst.state.reset();
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
    // SAFETY: valid instance per `instance_mut`'s contract; `process` and
    // everything it points to stay valid until this call returns
    // (plugin.h:92-94).
    let inst = unsafe { instance_mut::<P>(plugin) };
    let process = unsafe { &*process };

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
    with_rt_context(|rt| inst.state.process(&mut audio, rt));
    CLAP_PROCESS_CONTINUE
}

/// `plugin.h:99-104` `[thread-safe]`.
unsafe extern "C" fn plugin_get_extension(
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
    } else {
        ptr::null()
    }
}

/// `plugin.h:106-109` `[main-thread]`.
unsafe extern "C" fn plugin_on_main_thread(_plugin: *const ClapPlugin) {}

#[cfg(test)]
mod tests {
    use seco_core::RtContext;

    use super::*;

    /// The single plugin type used by every test in this crate: the
    /// descriptor storage is one static (one plugin per binary), so tests
    /// must not mix plugin types.
    struct HalfGain;

    impl Plugin for HalfGain {
        const ID: &'static str = "dev.seco.test.half-gain";
        const NAME: &'static str = "half-gain";
        const VENDOR: &'static str = "SECO tests";
        const VERSION: &'static str = "0.0.0";

        fn new() -> Self {
            HalfGain
        }

        fn process(&mut self, audio: &mut AudioBuffer, _rt: &RtContext) {
            for channel in audio.channels_mut() {
                for sample in channel {
                    *sample *= 0.5;
                }
            }
        }
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
    ) -> ClapProcess {
        ClapProcess {
            steady_time: -1,
            frames_count: frames,
            transport: ptr::null(),
            audio_inputs: inputs.unwrap_or(ptr::null()),
            audio_outputs: outputs,
            audio_inputs_count: u32::from(inputs.is_some()),
            audio_outputs_count: 1,
            in_events: ptr::null(),
            out_events: ptr::null(),
        }
    }

    #[test]
    fn out_of_place_copies_input_then_processes() {
        let plugin = create::<HalfGain>();
        let mut in_l = vec![1.0_f32; 64];
        let mut in_r = vec![-1.0_f32; 64];
        let mut out_l = vec![9.9_f32; 64];
        let mut out_r = vec![9.9_f32; 64];
        let mut in_ptrs = [in_l.as_mut_ptr(), in_r.as_mut_ptr()];
        let mut out_ptrs = [out_l.as_mut_ptr(), out_r.as_mut_ptr()];
        let in_buf = stereo_buffer(&mut in_ptrs);
        let mut out_buf = stereo_buffer(&mut out_ptrs);
        let process = process_struct(64, Some(&raw const in_buf), &raw mut out_buf);

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
        let plugin = create::<HalfGain>();
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
        let process = process_struct(32, Some(&raw const in_buf), &raw mut out_buf);

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
        let plugin = create::<HalfGain>();
        let mut l = vec![1.0_f32; 4];
        let mut r = vec![1.0_f32; 4];
        let mut out_ptrs = [l.as_mut_ptr(), r.as_mut_ptr()];
        let mut out_buf = stereo_buffer(&mut out_ptrs);
        let process = process_struct(0, None, &raw mut out_buf);

        // SAFETY: as in `out_of_place_copies_input_then_processes`.
        let status = unsafe { ((*plugin).process)(plugin, &raw const process) };

        assert_eq!(status, CLAP_PROCESS_CONTINUE);
        assert!(l.iter().all(|&s| s == 1.0));
        // SAFETY: created above; not used again after this call.
        unsafe { ((*plugin).destroy)(plugin) };
    }

    #[test]
    fn missing_input_yields_silence_not_garbage() {
        let plugin = create::<HalfGain>();
        let mut l = vec![7.0_f32; 16];
        let mut r = vec![7.0_f32; 16];
        let mut out_ptrs = [l.as_mut_ptr(), r.as_mut_ptr()];
        let mut out_buf = stereo_buffer(&mut out_ptrs);
        let process = process_struct(16, None, &raw mut out_buf);

        // SAFETY: as in `out_of_place_copies_input_then_processes`.
        let status = unsafe { ((*plugin).process)(plugin, &raw const process) };

        assert_eq!(status, CLAP_PROCESS_CONTINUE);
        assert!(l.iter().all(|&s| s == 0.0));
        assert!(r.iter().all(|&s| s == 0.0));
        // SAFETY: created above; not used again after this call.
        unsafe { ((*plugin).destroy)(plugin) };
    }
}
