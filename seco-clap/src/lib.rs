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

mod export;
mod instance;
