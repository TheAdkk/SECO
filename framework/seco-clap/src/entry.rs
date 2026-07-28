//! Builds the value exported as the `clap_entry` symbol.

use std::ffi::{CStr, c_char, c_void};
use std::marker::PhantomData;
use std::ptr;

use seco_core::Plugin;

use crate::factory::FactoryImpl;
use crate::ffi::{CLAP_PLUGIN_FACTORY_ID, CLAP_VERSION, ClapPluginEntry, ClapPluginFactory};

/// Assembles the `clap_plugin_entry` for a concrete plugin type. Used by
/// [`seco_export!`](crate::seco_export); not meant to be called directly.
pub struct EntryImpl<P>(PhantomData<P>);

impl<P: Plugin> EntryImpl<P> {
    /// The value the plugin crate exports as `clap_entry` (`entry.h:131-132`).
    pub const ENTRY: ClapPluginEntry = ClapPluginEntry {
        clap_version: CLAP_VERSION,
        init,
        deinit,
        get_factory: get_factory::<P>,
    };
}

/// `entry.h:100`. May be called more than once (`entry.h:34-36`), but the
/// counter/mutex defense is only required for "non trivial non idempotent
/// actions" — this does nothing, so none is needed.
unsafe extern "C" fn init(_plugin_path: *const c_char) -> bool {
    true
}

/// `entry.h:118`. Nothing to free; see [`init`].
unsafe extern "C" fn deinit() {}

/// `entry.h:120-128`. `[thread-safe]`; returns NULL for unknown factories.
unsafe extern "C" fn get_factory<P: Plugin>(factory_id: *const c_char) -> *const c_void {
    if factory_id.is_null() {
        return ptr::null();
    }
    // SAFETY: the host passes a valid NUL-terminated factory id string
    // (entry.h:128).
    let id = unsafe { CStr::from_ptr(factory_id) };
    if id == CLAP_PLUGIN_FACTORY_ID {
        (FactoryImpl::<P>::FACTORY_REF as *const ClapPluginFactory).cast()
    } else {
        ptr::null()
    }
}
