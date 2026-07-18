//! `clap.params`: bridges [`Plugin::PARAMS`] descriptors to the host.
//!
//! Parameter values are adapter-owned atomics (`Instance::param_bits`), not
//! plugin fields: `get_value` is `[main-thread]` and may run while the audio
//! thread is inside `process()`, so plugin-owned storage is off the table.
//! Plugins receive a per-block snapshot through `RtContext::param`.
//! A parameter's CLAP id is its index in `PARAMS` (append-only contract).

use std::ffi::{CStr, c_char};
use std::marker::PhantomData;
use std::ptr;
use std::sync::atomic::Ordering::Relaxed;

use seco_core::{ParamRange, Plugin};

use crate::ffi::{
    CLAP_PARAM_IS_AUTOMATABLE, CLAP_PARAM_IS_BYPASS, CLAP_PARAM_IS_ENUM, CLAP_PARAM_IS_STEPPED,
    ClapId, ClapInputEvents, ClapOutputEvents, ClapParamInfo, ClapPlugin, ClapPluginParams,
};
use crate::instance;
use crate::util::fixed_cstr;

/// Assembles the params vtable for a concrete plugin type.
pub(crate) struct ParamsImpl<P>(PhantomData<P>);

impl<P: Plugin> ParamsImpl<P> {
    pub(crate) const VTABLE: ClapPluginParams = ClapPluginParams {
        count: count::<P>,
        get_info: get_info::<P>,
        get_value: get_value::<P>,
        value_to_text: value_to_text::<P>,
        text_to_value: text_to_value::<P>,
        flush: flush::<P>,
    };
    pub(crate) const VTABLE_REF: &'static ClapPluginParams = &Self::VTABLE;
}

fn clap_flags(range: &ParamRange) -> u32 {
    match range {
        ParamRange::Continuous { .. } => CLAP_PARAM_IS_AUTOMATABLE,
        // IS_ENUM requires IS_STEPPED (ext/params.h:203-206).
        ParamRange::Stepped { .. } => {
            CLAP_PARAM_IS_AUTOMATABLE | CLAP_PARAM_IS_STEPPED | CLAP_PARAM_IS_ENUM
        }
        ParamRange::Toggle { bypass, .. } => {
            let base = CLAP_PARAM_IS_AUTOMATABLE | CLAP_PARAM_IS_STEPPED;
            if *bypass { base | CLAP_PARAM_IS_BYPASS } else { base }
        }
    }
}

/// Clamps a plain value to a valid step index.
fn step_index(value: f64, len: usize) -> usize {
    (value.round().max(0.0) as usize).min(len.saturating_sub(1))
}

/// Copies `text` into the host's capacity-limited out buffer, NUL-terminated.
///
/// SAFETY contract: `out` must be valid for `capacity` bytes, `capacity > 0`.
unsafe fn write_text(text: &str, out: *mut c_char, capacity: u32) {
    let n = text.len().min(capacity as usize - 1);
    // SAFETY: per fn contract; at most capacity-1 bytes plus the NUL.
    unsafe {
        for (i, byte) in text.as_bytes()[..n].iter().enumerate() {
            out.add(i).write(*byte as c_char);
        }
        out.add(n).write(0);
    }
}

/// `ext/params.h:259-261` `[main-thread]`.
unsafe extern "C" fn count<P: Plugin>(_plugin: *const ClapPlugin) -> u32 {
    P::PARAMS.len() as u32
}

/// `ext/params.h:263-268` `[main-thread]`.
unsafe extern "C" fn get_info<P: Plugin>(
    _plugin: *const ClapPlugin,
    param_index: u32,
    param_info: *mut ClapParamInfo,
) -> bool {
    let Some(desc) = P::PARAMS.get(param_index as usize) else {
        return false;
    };
    if param_info.is_null() {
        return false;
    }
    // SAFETY: the host provides `param_info` valid for one write
    // (ext/params.h:263-268); `ptr::write` because it may be uninitialized.
    unsafe {
        param_info.write(ClapParamInfo {
            id: param_index,
            flags: clap_flags(&desc.range),
            cookie: ptr::null_mut(),
            name: fixed_cstr(desc.name),
            module: fixed_cstr(""),
            min_value: desc.range.min(),
            max_value: desc.range.max(),
            default_value: desc.range.default_plain(),
        });
    }
    true
}

/// `ext/params.h:270-273` `[main-thread]` — possibly while the audio thread
/// is inside `process()`, hence atomics and a shared instance borrow.
unsafe extern "C" fn get_value<P: Plugin>(
    plugin: *const ClapPlugin,
    param_id: ClapId,
    out_value: *mut f64,
) -> bool {
    let index = param_id as usize;
    if plugin.is_null() || out_value.is_null() || index >= P::PARAMS.len() {
        return false;
    }
    // SAFETY: live instance per `instance::shared`'s contract.
    let inst = unsafe { instance::shared::<P>(plugin) };
    let value = f64::from_bits(inst.param_bits[index].load(Relaxed));
    // SAFETY: the host provides `out_value` valid for one f64 write.
    unsafe { out_value.write(value) };
    true
}

/// `ext/params.h:275-284` `[main-thread]`.
///
/// Continuous values use `format!("{value}")` — Rust's shortest round-trip
/// representation — so `text_to_value(value_to_text(v)) == v` exactly.
/// Allocation is fine here: main thread.
unsafe extern "C" fn value_to_text<P: Plugin>(
    _plugin: *const ClapPlugin,
    param_id: ClapId,
    value: f64,
    out_buffer: *mut c_char,
    out_buffer_capacity: u32,
) -> bool {
    let Some(desc) = P::PARAMS.get(param_id as usize) else {
        return false;
    };
    if out_buffer.is_null() || out_buffer_capacity == 0 {
        return false;
    }
    let text = match &desc.range {
        ParamRange::Continuous { .. } => format!("{value}"),
        ParamRange::Stepped { labels, .. } => {
            let Some(label) = labels.get(step_index(value, labels.len())) else {
                return false;
            };
            (*label).to_string()
        }
        ParamRange::Toggle { .. } => {
            if value >= 0.5 { "On" } else { "Off" }.to_string()
        }
    };
    // SAFETY: host guarantees `out_buffer` holds `out_buffer_capacity` bytes
    // (ext/params.h:275-284).
    unsafe { write_text(&text, out_buffer, out_buffer_capacity) };
    true
}

/// `ext/params.h:286-293` `[main-thread]`.
unsafe extern "C" fn text_to_value<P: Plugin>(
    _plugin: *const ClapPlugin,
    param_id: ClapId,
    param_value_text: *const c_char,
    out_value: *mut f64,
) -> bool {
    let Some(desc) = P::PARAMS.get(param_id as usize) else {
        return false;
    };
    if param_value_text.is_null() || out_value.is_null() {
        return false;
    }
    // SAFETY: the host passes a NUL-terminated string (ext/params.h:286-293).
    let Ok(text) = unsafe { CStr::from_ptr(param_value_text) }.to_str() else {
        return false;
    };
    let text = text.trim();
    let parsed = match &desc.range {
        ParamRange::Stepped { labels, .. } => labels
            .iter()
            .position(|label| label.eq_ignore_ascii_case(text))
            .map(|index| index as f64)
            .or_else(|| text.parse::<f64>().ok()),
        ParamRange::Toggle { .. } => match text {
            t if t.eq_ignore_ascii_case("on") => Some(1.0),
            t if t.eq_ignore_ascii_case("off") => Some(0.0),
            t => t.parse::<f64>().ok(),
        },
        ParamRange::Continuous { .. } => text.parse::<f64>().ok(),
    };
    let Some(value) = parsed.filter(|v| v.is_finite()) else {
        return false;
    };
    // SAFETY: the host provides `out_value` valid for one f64 write.
    unsafe { out_value.write(value.clamp(desc.range.min(), desc.range.max())) };
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
