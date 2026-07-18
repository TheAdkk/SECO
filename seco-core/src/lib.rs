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

pub use buffer::AudioBuffer;
pub use plugin::Plugin;
pub use rt::{RtContext, with_rt_context};
