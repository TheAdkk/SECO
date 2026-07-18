//! `clap.params`: Phase 2 scaffolding — one inert "Dummy" parameter.
//!
//! Its only job is proving the host↔plugin parameter plumbing (declaration,
//! automation events, text round-trip) before Phase 3 replaces it with the
//! real parameter system designed in seco-core. It is stored as an
//! `AtomicU64` on the instance because `get_value` runs on the main thread
//! while the audio thread applies events — see `instance.rs` on why the
//! instance is never handed out as `&mut`.

use std::ffi::{CStr, c_char};
use std::marker::PhantomData;
use std::ptr;
use std::sync::atomic::Ordering::Relaxed;

use seco_core::Plugin;

use crate::ffi::{
    CLAP_PARAM_IS_AUTOMATABLE, ClapId, ClapInputEvents, ClapOutputEvents, ClapParamInfo,
    ClapPlugin, ClapPluginParams,
};
use crate::instance;
use crate::util::fixed_cstr;

pub(crate) const DUMMY_PARAM_ID: ClapId = 0;
pub(crate) const DUMMY_PARAM_DEFAULT: f64 = 0.5;

/// Assembles the params vtable for a concrete plugin type.
pub(crate) struct ParamsImpl<P>(PhantomData<P>);

impl<P: Plugin> ParamsImpl<P> {
    pub(crate) const VTABLE: ClapPluginParams = ClapPluginParams {
        count,
        get_info,
        get_value: get_value::<P>,
        value_to_text,
        text_to_value,
        flush: flush::<P>,
    };
    pub(crate) const VTABLE_REF: &'static ClapPluginParams = &Self::VTABLE;
}

/// `ext/params.h:259-261` `[main-thread]`.
unsafe extern "C" fn count(_plugin: *const ClapPlugin) -> u32 {
    1
}

/// `ext/params.h:263-268` `[main-thread]`.
unsafe extern "C" fn get_info(
    _plugin: *const ClapPlugin,
    param_index: u32,
    param_info: *mut ClapParamInfo,
) -> bool {
    if param_index != 0 || param_info.is_null() {
        return false;
    }
    // SAFETY: the host provides `param_info` valid for one write
    // (ext/params.h:263-268); `ptr::write` because it may be uninitialized.
    unsafe {
        param_info.write(ClapParamInfo {
            id: DUMMY_PARAM_ID,
            flags: CLAP_PARAM_IS_AUTOMATABLE,
            cookie: ptr::null_mut(),
            name: fixed_cstr("Dummy"),
            module: fixed_cstr(""),
            min_value: 0.0,
            max_value: 1.0,
            default_value: DUMMY_PARAM_DEFAULT,
        });
    }
    true
}

/// `ext/params.h:270-273` `[main-thread]` — and the audio thread may be
/// inside `process()` at the same time, which is why the value lives in an
/// atomic and the instance is only borrowed shared.
unsafe extern "C" fn get_value<P: Plugin>(
    plugin: *const ClapPlugin,
    param_id: ClapId,
    out_value: *mut f64,
) -> bool {
    if plugin.is_null() || out_value.is_null() || param_id != DUMMY_PARAM_ID {
        return false;
    }
    // SAFETY: live instance per `instance::shared`'s contract.
    let inst = unsafe { instance::shared::<P>(plugin) };
    let value = f64::from_bits(inst.dummy_param_bits.load(Relaxed));
    // SAFETY: the host provides `out_value` valid for one f64 write.
    unsafe { out_value.write(value) };
    true
}

/// `ext/params.h:275-284` `[main-thread]`.
///
/// `format!("{value}")` is Rust's shortest round-trip representation, so
/// `text_to_value(value_to_text(v)) == v` holds exactly. Allocation is fine:
/// main thread.
unsafe extern "C" fn value_to_text(
    _plugin: *const ClapPlugin,
    param_id: ClapId,
    value: f64,
    out_buffer: *mut c_char,
    out_buffer_capacity: u32,
) -> bool {
    if param_id != DUMMY_PARAM_ID || out_buffer.is_null() || out_buffer_capacity == 0 {
        return false;
    }
    let text = format!("{value}");
    let n = text.len().min(out_buffer_capacity as usize - 1);
    // SAFETY: the host guarantees `out_buffer` holds `out_buffer_capacity`
    // bytes (ext/params.h:275-284); we write at most capacity-1 plus NUL.
    unsafe {
        for (i, byte) in text.as_bytes()[..n].iter().enumerate() {
            out_buffer.add(i).write(*byte as c_char);
        }
        out_buffer.add(n).write(0);
    }
    true
}

/// `ext/params.h:286-293` `[main-thread]`.
unsafe extern "C" fn text_to_value(
    _plugin: *const ClapPlugin,
    param_id: ClapId,
    param_value_text: *const c_char,
    out_value: *mut f64,
) -> bool {
    if param_id != DUMMY_PARAM_ID || param_value_text.is_null() || out_value.is_null() {
        return false;
    }
    // SAFETY: the host passes a NUL-terminated string (ext/params.h:286-293).
    let text = unsafe { CStr::from_ptr(param_value_text) };
    let Some(value) = text.to_str().ok().and_then(|s| s.trim().parse::<f64>().ok()) else {
        return false;
    };
    if !value.is_finite() {
        return false;
    }
    // SAFETY: the host provides `out_value` valid for one f64 write.
    unsafe { out_value.write(value.clamp(0.0, 1.0)) };
    true
}

/// `ext/params.h:295-306` `[active ? audio-thread : main-thread]`. Safe from
/// either: only atomics are touched.
unsafe extern "C" fn flush<P: Plugin>(
    plugin: *const ClapPlugin,
    in_: *const ClapInputEvents,
    _out: *const ClapOutputEvents,
) {
    if plugin.is_null() {
        return;
    }
    // SAFETY: live instance per `instance::shared`'s contract.
    let inst = unsafe { instance::shared::<P>(plugin) };
    // SAFETY: the event list is valid for the duration of this call.
    unsafe { instance::apply_input_events(inst, in_) };
}
