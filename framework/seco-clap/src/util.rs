//! Small shared helpers for the FFI layer.

use std::ffi::c_char;

/// Builds a fixed-size, NUL-terminated C char array from a Rust string,
/// truncating if needed. For filling CLAP's fixed-size name/module fields.
pub(crate) fn fixed_cstr<const N: usize>(text: &str) -> [c_char; N] {
    let mut out = [0 as c_char; N];
    for (dst, src) in out.iter_mut().zip(text.bytes().take(N - 1)) {
        *dst = src as c_char;
    }
    out
}
