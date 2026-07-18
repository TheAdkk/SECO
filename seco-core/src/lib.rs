//! SECO core: the plugin traits and audio types.
//!
//! This crate is deliberately free of FFI, `unsafe`, and dependencies. A
//! [`Plugin`] implementation compiles without knowing that CLAP exists;
//! `seco-clap` adapts it to the CLAP ABI.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod buffer;
mod plugin;
mod rt;
mod transport;

pub use buffer::AudioBuffer;
pub use plugin::Plugin;
pub use rt::RtContext;
pub use transport::Transport;

/// Not public API — adapter-only entry points, exempt from semver.
///
/// The `__private` path is the fence and `#[doc(hidden)]` is the sign
/// (serde/tokio convention): signaling, not a guarantee. See [`RtContext`]
/// for what breaks if you climb over it.
#[doc(hidden)]
pub mod __private {
    pub use crate::rt::with_rt_context;
}
