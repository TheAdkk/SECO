//! Reads a built plugin's identity out of the plugin itself.
//!
//! The bundle metadata (identifier, display name, version) has exactly one
//! source of truth: the `Plugin` constants the binary already exposes
//! through `clap_plugin_descriptor`. Rather than restate them in a script or
//! in `Cargo.toml`, xtask does what a host does — dlopen the artifact, look
//! up `clap_entry`, ask the factory — so a bundle can never describe a
//! plugin the binary does not.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;

use seco_clap::ffi::{CLAP_PLUGIN_FACTORY_ID, ClapPluginEntry, ClapPluginFactory};

// dlopen/dlsym live in libSystem (macOS) and libdl/libc (Linux); both are
// linked into every Rust binary already.
unsafe extern "C" {
    fn dlopen(path: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
}

/// `RTLD_NOW` — 2 on both macOS (`dlfcn.h`) and glibc.
const RTLD_NOW: c_int = 2;

/// What a bundle needs to describe the plugin it wraps.
pub struct Descriptor {
    pub id: String,
    pub name: String,
    pub version: String,
}

/// Loads `dylib`, drives the CLAP entry point exactly as a host would, and
/// copies the descriptor strings out before unloading.
pub fn read(dylib: &Path) -> Result<Descriptor, String> {
    let path = CString::new(dylib.as_os_str().as_encoded_bytes())
        .map_err(|_| format!("path is not a C string: {}", dylib.display()))?;

    // SAFETY: `path` is a valid NUL-terminated string for the call.
    let handle = unsafe { dlopen(path.as_ptr(), RTLD_NOW) };
    if handle.is_null() {
        return Err(format!("dlopen failed: {}", dylib.display()));
    }
    let result = read_loaded(handle, &path);
    // SAFETY: `handle` came from a successful dlopen and is not used again.
    unsafe { dlclose(handle) };
    result
}

/// The lookup, factored out so the `dlclose` above always runs.
fn read_loaded(handle: *mut c_void, path: &CStr) -> Result<Descriptor, String> {
    // SAFETY: `handle` is a live dlopen handle; the symbol name is a literal.
    let symbol = unsafe { dlsym(handle, c"clap_entry".as_ptr()) };
    if symbol.is_null() {
        return Err("no clap_entry symbol — is this a CLAP plugin?".into());
    }
    // SAFETY: a CLAP binary's `clap_entry` is a `clap_plugin_entry`
    // (entry.h:131-132); the pointer stays valid until dlclose.
    let entry = unsafe { &*symbol.cast::<ClapPluginEntry>() };

    // entry.h:34-46: init() before anything else, deinit() at the end.
    // SAFETY: the entry vtable above is well-formed; `path` outlives the call.
    if !unsafe { (entry.init)(path.as_ptr()) } {
        return Err("clap_entry.init() returned false".into());
    }
    let descriptor = read_factory(entry);
    // SAFETY: paired with the successful init above.
    unsafe { (entry.deinit)() };
    descriptor
}

fn read_factory(entry: &ClapPluginEntry) -> Result<Descriptor, String> {
    // SAFETY: `get_factory` is [thread-safe] and takes a NUL-terminated id
    // (entry.h:120-128); a NULL return means "factory not supported".
    let factory = unsafe { (entry.get_factory)(CLAP_PLUGIN_FACTORY_ID.as_ptr()) };
    if factory.is_null() {
        return Err("plugin exposes no clap.plugin-factory".into());
    }
    let factory = factory.cast::<ClapPluginFactory>();
    // SAFETY: the pointer above is a `clap_plugin_factory` by the id we
    // asked for; index 0 exists whenever get_plugin_count() > 0.
    let descriptor = unsafe {
        if ((*factory).get_plugin_count)(factory) == 0 {
            return Err("factory reports zero plugins".into());
        }
        ((*factory).get_plugin_descriptor)(factory, 0)
    };
    if descriptor.is_null() {
        return Err("factory returned a null descriptor for index 0".into());
    }
    // SAFETY: non-null descriptor from the factory; its string fields are
    // NUL-terminated and live as long as the loaded library
    // (plugin-factory.h:23-28). Copied to owned Strings before unloading.
    unsafe {
        let descriptor = &*descriptor;
        Ok(Descriptor {
            id: cstr(descriptor.id)?,
            name: cstr(descriptor.name)?,
            version: cstr(descriptor.version)?,
        })
    }
}

/// Copies one descriptor field. Rejects NULL: `id`, `name` and `version` are
/// mandatory (plugin.h:11-14), and a bundle built from a blank one would be
/// silently unloadable.
///
/// # Safety
///
/// `ptr` must be NULL or a valid NUL-terminated string.
unsafe fn cstr(ptr: *const c_char) -> Result<String, String> {
    if ptr.is_null() {
        return Err("descriptor field is null".into());
    }
    // SAFETY: per this function's contract.
    Ok(unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned())
}
