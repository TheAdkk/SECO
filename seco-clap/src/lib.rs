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

/// Not public API — re-exports for `seco_export!` expansions only.
#[doc(hidden)]
pub mod __reexport {
    pub use seco_core::Plugin;
}
