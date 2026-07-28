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

/// Formats a continuous value for display: fixed decimals plus the unit.
/// Rounding is display-only — the stored value keeps full precision — so
/// `text_to_value` after this is a *round trip of the text*, not of the
/// value: "50%" reads back as exactly 50, which formats to "50%" again.
fn continuous_text(value: f64, unit: &str, decimals: u8) -> String {
    format!("{:.*}{unit}", decimals as usize, value)
}

/// Display formatting for the GUI. Deliberately a *copy* of the logic in
/// `value_to_text` rather than a shared helper: routing the host path
/// through a helper changed the no-gui binary, and the byte-identity
/// guarantee wins. The `display_matches_value_to_text` test pins the two
/// implementations together — if they ever format differently, it fails.
/// `None` only for a stepped value with no label.
#[cfg(any(test, all(feature = "gui", target_os = "macos")))]
pub(crate) fn plain_to_display(range: &ParamRange, value: f64) -> Option<String> {
    match range {
        ParamRange::Continuous { unit, decimals, .. } => {
            Some(continuous_text(value, unit, *decimals))
        }
        ParamRange::Stepped { labels, .. } => {
            labels.get(step_index(value, labels.len())).map(|label| (*label).to_string())
        }
        ParamRange::Toggle { .. } => {
            Some(if value >= 0.5 { "On" } else { "Off" }.to_string())
        }
    }
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
/// Continuous values are rounded to the parameter's declared decimals and
/// carry its unit, because this text is what hosts and the editor show —
/// `0.5046999999999999` is a correct number and a useless readout. The
/// round trip that matters is text -> value -> text, which is stable
/// (see `continuous_text`). Allocation is fine here: main thread.
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
        ParamRange::Continuous { unit, decimals, .. } => {
            continuous_text(value, unit, *decimals)
        }
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
        // The unit is optional on input: hosts round-trip our own text
        // ("50%"), users type "50".
        ParamRange::Continuous { unit, .. } => {
            text.strip_suffix(unit).unwrap_or(text).trim().parse::<f64>().ok()
        }
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
    // GUI edits reach the host here when the plugin is inactive (no
    // process() running to drain them).
    #[cfg(any(test, all(feature = "gui", target_os = "macos")))]
    // SAFETY: flush is the consumer when not processing
    // (ext/params.h:295-306); `_out` is valid for the call.
    unsafe {
        instance::drain_gui_params::<P>(inst, _out)
    };
}

#[cfg(test)]
mod tests {
    use seco_core::{AudioBuffer, ParamDesc, Plugin, RtContext};

    use super::*;

    /// Text-conversion-only plugin: `value_to_text` never dereferences the
    /// plugin pointer, so no instance (and no descriptor storage) is needed.
    struct TextOnly;

    impl Plugin for TextOnly {
        const ID: &'static str = "dev.seco.test.text-only";
        const NAME: &'static str = "text-only";
        const VENDOR: &'static str = "SECO tests";
        const VERSION: &'static str = "0.0.0";
        const PARAMS: &'static [ParamDesc] = &[
            ParamDesc {
                name: "Cont",
                range: ParamRange::Continuous {
                    min: 0.0,
                    max: 1.0,
                    default: 0.5,
                    unit: "",
                    decimals: 2,
                },
            },
            ParamDesc {
                name: "Pct",
                range: ParamRange::Continuous {
                    min: 0.0,
                    max: 100.0,
                    default: 100.0,
                    unit: "%",
                    decimals: 0,
                },
            },
            ParamDesc {
                name: "Step",
                range: ParamRange::Stepped { labels: &["1/1", "1/2", "1/4"], default: 1 },
            },
            ParamDesc { name: "Tog", range: ParamRange::Toggle { default: false, bypass: false } },
        ];

        fn new() -> Self {
            TextOnly
        }
        fn process(&mut self, _audio: &mut AudioBuffer, _rt: &RtContext) {}
    }

    /// Reads a parameter's display text through the host entry point.
    fn text_of(id: ClapId, value: f64) -> String {
        let mut buffer = [0 as c_char; 64];
        // SAFETY: value_to_text never touches the plugin pointer; the out
        // buffer is ours with the stated capacity.
        let ok = unsafe {
            (ParamsImpl::<TextOnly>::VTABLE.value_to_text)(
                std::ptr::null(),
                id,
                value,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
            )
        };
        assert!(ok, "value_to_text failed for param {id} value {value}");
        // SAFETY: value_to_text NUL-terminated the buffer.
        unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_str().unwrap().to_string()
    }

    /// Parses display text back through the host entry point.
    fn value_of(id: ClapId, text: &str) -> Option<f64> {
        let input = std::ffi::CString::new(text).unwrap();
        let mut value = f64::NAN;
        // SAFETY: text_to_value never touches the plugin pointer; both
        // pointers are ours and valid for the call.
        let ok = unsafe {
            (ParamsImpl::<TextOnly>::VTABLE.text_to_value)(
                std::ptr::null(),
                id,
                input.as_ptr(),
                &raw mut value,
            )
        };
        ok.then_some(value)
    }

    /// A parameter's readout is the plugin's job, not the host's: an
    /// automated percentage arrives as 50.469999999999999 and must read as
    /// "50%", in the DAW and in the editor alike.
    #[test]
    fn continuous_values_display_rounded_with_their_unit() {
        assert_eq!(text_of(1, 50.469_999_999_999_99), "50%");
        assert_eq!(text_of(1, 0.0), "0%");
        assert_eq!(text_of(1, 100.0), "100%");
        // Two decimals, no unit, on the other continuous parameter.
        assert_eq!(text_of(0, 0.123_456_789), "0.12");
    }

    /// Rounding the text must not make the parameter lossy in the host's
    /// hands: whatever we print, we must read back, and printing that again
    /// must not drift.
    #[test]
    fn display_text_round_trips_through_text_to_value() {
        for value in [0.0, 0.4, 12.5, 50.469_999_999_999_99, 99.6, 100.0] {
            let text = text_of(1, value);
            let parsed = value_of(1, &text).expect("our own text must parse");
            assert_eq!(text_of(1, parsed), text, "value {value} drifted on re-display");
        }
        // Typed by hand, without the unit, and out of range.
        assert_eq!(value_of(1, "50"), Some(50.0));
        assert_eq!(value_of(1, "  75 % "), Some(75.0));
        assert_eq!(value_of(1, "140"), Some(100.0));
        assert_eq!(value_of(1, "nope"), None);
    }

    /// `plain_to_display` (used by the GUI) is a deliberate copy of the
    /// `value_to_text` formatting so the no-gui binary stays byte-identical.
    /// This test is the leash: if the copies ever diverge, it fails.
    #[test]
    fn display_matches_value_to_text() {
        let cases: &[(ClapId, f64)] = &[
            (0, 0.0),
            (0, 0.3),
            (0, 0.123456789),
            (0, 1.0),
            (1, 0.0),
            (1, 50.469_999_999_999_99),
            (1, 100.0),
            (2, 0.0),
            (2, 1.0),
            (2, 1.4),
            (2, 2.0),
            (2, 7.0),
            (3, 0.0),
            (3, 0.49),
            (3, 0.5),
            (3, 1.0),
        ];
        for &(id, value) in cases {
            let mut buffer = [0 as c_char; 64];
            // SAFETY: value_to_text never touches the plugin pointer; the
            // out buffer is ours with the stated capacity.
            let ok = unsafe {
                (ParamsImpl::<TextOnly>::VTABLE.value_to_text)(
                    std::ptr::null(),
                    id,
                    value,
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                )
            };
            assert!(ok, "value_to_text failed for param {id} value {value}");
            // SAFETY: value_to_text NUL-terminated the buffer.
            let host_text =
                unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_str().unwrap().to_string();
            let gui_text = plain_to_display(&TextOnly::PARAMS[id as usize].range, value)
                .expect("display formatting failed");
            assert_eq!(host_text, gui_text, "param {id} value {value}: host vs GUI text");
        }
    }
}
