//! `clap.audio-ports`: one stereo input, one stereo output, in-place capable.
//!
//! Fixed for now — SECO v1 plugins are stereo effects. When a plugin needs to
//! choose its own layout, this becomes part of the `Plugin` trait.

use crate::ffi::{
    CLAP_AUDIO_PORT_IS_MAIN, CLAP_PORT_STEREO, ClapAudioPortInfo, ClapPlugin, ClapPluginAudioPorts,
};
use crate::util::fixed_cstr;

pub(crate) const VTABLE: ClapPluginAudioPorts = ClapPluginAudioPorts { count, get };
/// `&'static` handle to [`VTABLE`], for returning from `get_extension`.
pub(crate) const VTABLE_REF: &ClapPluginAudioPorts = &VTABLE;

/// `ext/audio-ports.h:69-71` `[main-thread]`.
unsafe extern "C" fn count(_plugin: *const ClapPlugin, _is_input: bool) -> u32 {
    1
}

/// `ext/audio-ports.h:73-79` `[main-thread]`.
unsafe extern "C" fn get(
    _plugin: *const ClapPlugin,
    index: u32,
    _is_input: bool,
    info: *mut ClapAudioPortInfo,
) -> bool {
    if index != 0 || info.is_null() {
        return false;
    }
    // Written via `ptr::write`, not `&mut *info`: the host's out-param may be
    // uninitialized memory, and forming a reference to it would already be UB.
    //
    // SAFETY: the host provides `info` valid for writing one
    // `clap_audio_port_info` (ext/audio-ports.h:73-79).
    unsafe {
        info.write(ClapAudioPortInfo {
            id: 0,
            name: fixed_cstr("main"),
            flags: CLAP_AUDIO_PORT_IS_MAIN,
            channel_count: 2,
            port_type: CLAP_PORT_STEREO.as_ptr(),
            // Same id on the other side: in-place processing is supported
            // (ext/audio-ports.h:61-64), and the adapter handles it.
            in_place_pair: 0,
        });
    }
    true
}
