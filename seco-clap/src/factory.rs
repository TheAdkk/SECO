//! `clap.plugin-factory` for a single plugin type.

use std::ffi::{CStr, CString, c_char};
use std::marker::PhantomData;
use std::ptr;
use std::sync::OnceLock;

use seco_core::Plugin;

use crate::ffi::{
    CLAP_PLUGIN_FEATURE_AUDIO_EFFECT, CLAP_PLUGIN_FEATURE_STEREO, CLAP_VERSION, ClapHost,
    ClapPlugin, ClapPluginDescriptor, ClapPluginFactory,
};
use crate::instance;

/// Assembles the factory for a concrete plugin type.
pub struct FactoryImpl<P>(PhantomData<P>);

impl<P: Plugin> FactoryImpl<P> {
    /// The factory vtable handed to the host.
    pub const FACTORY: ClapPluginFactory = ClapPluginFactory {
        get_plugin_count,
        get_plugin_descriptor: get_plugin_descriptor::<P>,
        create_plugin: create_plugin::<P>,
    };

    /// `&'static` handle to [`Self::FACTORY`], for returning as a raw pointer.
    pub const FACTORY_REF: &'static ClapPluginFactory = &Self::FACTORY;
}

/// Owns the descriptor plus the C strings it points into.
///
/// CLAP wants NUL-terminated C strings with static lifetime; `Plugin`'s
/// constants are plain Rust `&str` (seco-core must not know about C). The
/// strings are converted once, on first use, and kept alive here for the
/// lifetime of the DSO.
struct DescriptorStorage {
    _strings: Box<[CString]>,
    _features: Box<[*const c_char]>,
    descriptor: ClapPluginDescriptor,
}

// SAFETY: every pointer stored in `descriptor` and `_features` targets either
// a `'static` C literal or heap memory owned by `_strings`/`_features`, whose
// addresses are stable (boxed) and which live exactly as long as this struct.
// The storage is immutable after construction, so sharing across threads is
// sound.
unsafe impl Send for DescriptorStorage {}
// SAFETY: see the `Send` justification above; there is no interior mutability.
unsafe impl Sync for DescriptorStorage {}

// A single static slot instead of one per `P`: Rust has no generic statics,
// and a SECO binary exports exactly one plugin type (a second `seco_export!`
// would collide on the `clap_entry` symbol), so this cannot be shared by two
// different `P`.
static DESCRIPTOR: OnceLock<DescriptorStorage> = OnceLock::new();

pub(crate) fn descriptor_for<P: Plugin>() -> &'static ClapPluginDescriptor {
    let storage = DESCRIPTOR.get_or_init(|| {
        // An interior NUL in a plugin constant would be a plugin-author bug;
        // degrade to an empty string rather than panicking across FFI.
        let strings: Box<[CString]> = [P::ID, P::NAME, P::VENDOR, P::VERSION, P::DESCRIPTION]
            .into_iter()
            .map(|s| CString::new(s).unwrap_or_default())
            .collect();
        let features: Box<[*const c_char]> = Box::new([
            CLAP_PLUGIN_FEATURE_AUDIO_EFFECT.as_ptr(),
            CLAP_PLUGIN_FEATURE_STEREO.as_ptr(),
            ptr::null(),
        ]);
        // Optional URLs: blank is explicitly allowed (plugin.h:15-16).
        let blank = c"".as_ptr();
        let descriptor = ClapPluginDescriptor {
            clap_version: CLAP_VERSION,
            id: strings[0].as_ptr(),
            name: strings[1].as_ptr(),
            vendor: strings[2].as_ptr(),
            url: blank,
            manual_url: blank,
            support_url: blank,
            version: strings[3].as_ptr(),
            description: strings[4].as_ptr(),
            features: features.as_ptr(),
        };
        DescriptorStorage { _strings: strings, _features: features, descriptor }
    });
    &storage.descriptor
}

/// `factory/plugin-factory.h:19-21`. One binary, one plugin.
unsafe extern "C" fn get_plugin_count(_factory: *const ClapPluginFactory) -> u32 {
    1
}

/// `factory/plugin-factory.h:23-28`. NULL on bad index.
unsafe extern "C" fn get_plugin_descriptor<P: Plugin>(
    _factory: *const ClapPluginFactory,
    index: u32,
) -> *const ClapPluginDescriptor {
    if index == 0 { descriptor_for::<P>() } else { ptr::null() }
}

/// `factory/plugin-factory.h:30-38`. Host callbacks are forbidden in here;
/// host access belongs in `clap_plugin.init` (plugin.h:49-51).
unsafe extern "C" fn create_plugin<P: Plugin>(
    _factory: *const ClapPluginFactory,
    host: *const ClapHost,
    plugin_id: *const c_char,
) -> *const ClapPlugin {
    if host.is_null() || plugin_id.is_null() {
        return ptr::null();
    }
    // SAFETY: the host passes a valid NUL-terminated plugin id
    // (factory/plugin-factory.h:30).
    let id = unsafe { CStr::from_ptr(plugin_id) };
    if id.to_bytes() != P::ID.as_bytes() {
        return ptr::null();
    }
    instance::create::<P>(host)
}
