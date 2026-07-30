//! CLAP ABI adapter for SECO plugins.
//!
//! This is the only crate in the workspace that uses `unsafe`. All FFI types
//! are hand-written mirrors of the CLAP headers, pinned at
//! `reference/clap` commit `195b42a` (tag 1.2.10); every item cites the
//! header it mirrors, and every `unsafe` block carries a `SAFETY:` comment
//! stating the invariant it relies on.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod entry;
pub mod ext;
pub mod factory;
pub mod ffi;

mod alloc;
mod export;
mod instance;
mod plugin_state;
#[cfg(debug_assertions)]
mod trace;
mod util;

pub use alloc::RtAllocCheck;
pub use plugin_state::MAX_PLUGIN_STATE;

/// Capacity of the per-instance parameter storage. `seco_export!` rejects
/// plugins declaring more at compile time.
pub const MAX_PARAMS: usize = 32;

/// Legacy visualization bucket count.
///
/// Kept for existing plugins and their fixed layouts. New plugins should
/// declare [`Plugin::SCOPE_SLOTS`](seco_core::Plugin::SCOPE_SLOTS) and may
/// use up to [`MAX_SCOPE_BUCKETS`].
///
/// How many visualization buckets a plugin can publish
/// ([`RtContext::set_scope`](seco_core::RtContext::set_scope)).
///
/// What the buckets *mean* is the plugin's business: the framework only
/// promises the array's size and that writing to it is free. Zape uses two
/// halves — the signal in, and the signal out — which is why this is 256
/// rather than the 128 one waveform needs.
///
/// 128 across an editor a few hundred pixels wide is a couple of pixels per
/// bucket, past what the eye resolves in a waveform, and the whole array
/// still fits in about a kilobyte of JSON per refresh.
pub const SCOPE_BUCKETS: usize = 256;

/// Maximum per-instance visualization slots available to a plugin.
///
/// Storage stays fixed and lock-free so `RtContext::set_scope` remains safe
/// for audio callbacks. Each plugin receives only its declared
/// [`Plugin::SCOPE_SLOTS`](seco_core::Plugin::SCOPE_SLOTS) prefix.
pub const MAX_SCOPE_BUCKETS: usize = 1_024;

/// Not public API — re-exports for `seco_export!` expansions only.
#[doc(hidden)]
pub mod __reexport {
    pub use seco_core::Plugin;
}

#[cfg(test)]
mod vst3_wrapper_tests {
    const MACOS_TICKS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../plugins/neta/vendor/clap-wrapper/external/clap-wrapper/src/detail/os/macos.mm"
    ));
    const LINUX_TICKS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../plugins/neta/vendor/clap-wrapper/external/clap-wrapper/src/detail/os/linux.cpp"
    ));

    #[test]
    fn vendored_vst3_timers_use_monotonic_milliseconds() {
        for source in [MACOS_TICKS, LINUX_TICKS] {
            assert!(source.contains("std::chrono::steady_clock"));
            assert!(source.contains("std::chrono::milliseconds"));
        }
        assert!(!MACOS_TICKS.contains("::clock()"));
        assert!(!LINUX_TICKS.contains("return clock()"));
    }
}
