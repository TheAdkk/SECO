//! `clap.state`: persists parameter values.
//!
//! Required, not optional: hosts should refuse to save/restore parameters
//! for plugins without this extension (ext/params.h:101-107).
//!
//! Format, little-endian: magic `"SECO"`, `u16` version (= 1), `u16` entry
//! count, then per entry a `u32` parameter index and the `f64` plain value
//! as bits. Unknown indices are skipped on load (a newer state in an older
//! plugin); absent ones keep their current value.

use std::marker::PhantomData;
use std::sync::atomic::Ordering::Relaxed;

use seco_core::Plugin;

use crate::ffi::{ClapIStream, ClapOStream, ClapPlugin, ClapPluginState};
use crate::instance;

/// Assembles the state vtable for a concrete plugin type.
pub(crate) struct StateImpl<P>(PhantomData<P>);

impl<P: Plugin> StateImpl<P> {
    pub(crate) const VTABLE: ClapPluginState =
        ClapPluginState { save: save::<P>, load: load::<P> };
    pub(crate) const VTABLE_REF: &'static ClapPluginState = &Self::VTABLE;
}

const MAGIC: [u8; 4] = *b"SECO";
const VERSION: u16 = 1;
const ENTRY_BYTES: usize = 4 + 8;
/// Sanity ceiling for incoming state blobs; ours are tens of bytes.
const MAX_STATE_BYTES: usize = 1 << 20;

/// `ext/state.h:25-28` `[main-thread]`.
unsafe extern "C" fn save<P: Plugin>(
    plugin: *const ClapPlugin,
    stream: *const ClapOStream,
) -> bool {
    if plugin.is_null() || stream.is_null() {
        return false;
    }
    // SAFETY: live instance per `instance::shared`'s contract.
    let inst = unsafe { instance::shared::<P>(plugin) };

    let mut bytes = Vec::with_capacity(8 + P::PARAMS.len() * ENTRY_BYTES);
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&(P::PARAMS.len() as u16).to_le_bytes());
    for index in 0..P::PARAMS.len() {
        bytes.extend_from_slice(&(index as u32).to_le_bytes());
        bytes.extend_from_slice(&inst.param_bits[index].load(Relaxed).to_le_bytes());
    }

    // Streams may accept fewer bytes than offered: loop (stream.h:10-16).
    let mut written = 0;
    while written < bytes.len() {
        let remaining = &bytes[written..];
        // SAFETY: stream and its callback are valid for this call; the
        // buffer is ours and lives across it.
        let n = unsafe {
            ((*stream).write)(stream, remaining.as_ptr().cast(), remaining.len() as u64)
        };
        // -1 is an error; 0 would loop forever, treat it as one too.
        if n <= 0 {
            return false;
        }
        written += n as usize;
    }
    true
}

/// `ext/state.h:30-33` `[main-thread]`. Loading applies straight to the
/// param atomics; the audio thread picks the values up next block.
unsafe extern "C" fn load<P: Plugin>(
    plugin: *const ClapPlugin,
    stream: *const ClapIStream,
) -> bool {
    if plugin.is_null() || stream.is_null() {
        return false;
    }
    // SAFETY: live instance per `instance::shared`'s contract.
    let inst = unsafe { instance::shared::<P>(plugin) };

    // Drain the stream first; hosts may hand out a few bytes per read
    // (stream.h:10-16 — clap-validator's buffered-streams test uses a small
    // prime). 0 is EOF, negative is an error.
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 512];
    loop {
        // SAFETY: stream and callback valid for this call; chunk is ours.
        let n = unsafe {
            ((*stream).read)(stream, chunk.as_mut_ptr().cast(), chunk.len() as u64)
        };
        if n < 0 {
            return false;
        }
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..n as usize]);
        if bytes.len() > MAX_STATE_BYTES {
            return false;
        }
    }

    if bytes.len() < 8 || bytes[0..4] != MAGIC {
        return false;
    }
    if u16::from_le_bytes([bytes[4], bytes[5]]) != VERSION {
        return false;
    }
    let count = u16::from_le_bytes([bytes[6], bytes[7]]) as usize;
    if bytes.len() < 8 + count * ENTRY_BYTES {
        return false;
    }
    for entry in 0..count {
        let offset = 8 + entry * ENTRY_BYTES;
        let mut index_bytes = [0_u8; 4];
        index_bytes.copy_from_slice(&bytes[offset..offset + 4]);
        let mut value_bytes = [0_u8; 8];
        value_bytes.copy_from_slice(&bytes[offset + 4..offset + ENTRY_BYTES]);
        let index = u32::from_le_bytes(index_bytes) as usize;
        if index < P::PARAMS.len() {
            inst.param_bits[index].store(u64::from_le_bytes(value_bytes), Relaxed);
        }
    }
    true
}
