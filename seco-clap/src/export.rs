//! The `seco_export!` macro.

/// Exports a [`Plugin`](seco_core::Plugin) type as this dynamic library's
/// CLAP entry point.
///
/// Expands to the `clap_entry` symbol CLAP hosts look up (`entry.h:131-132`).
/// Invoke it exactly once, at the crate root of a `cdylib` plugin crate.
///
/// On `unsafe(no_mangle)`: exporting an unmangled global is an FFI contract
/// the compiler cannot check — the name must be unique in the final binary
/// and the type must match what the loader expects. Both hold here: one
/// `seco_export!` per cdylib (a second one fails to link on the duplicate
/// symbol), and the type mirrors `entry.h`.
#[macro_export]
macro_rules! seco_export {
    ($plugin:ty) => {
        #[allow(non_upper_case_globals)]
        #[unsafe(no_mangle)]
        pub static clap_entry: $crate::ffi::ClapPluginEntry =
            $crate::entry::EntryImpl::<$plugin>::ENTRY;
    };
}
