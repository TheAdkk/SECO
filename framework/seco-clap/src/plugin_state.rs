//! The plugin's own state block: adapter-owned bytes, handed to the audio
//! thread without locks.
//!
//! # Why the adapter owns it
//!
//! Some plugin state does not fit in parameters — a drawn curve, a sample
//! map, anything longer than a number. The obvious API would be "ask the
//! plugin for its bytes in `clap.state.save`", and it is unsound: both
//! state callbacks are `[main-thread]` (ext/state.h:25-33) and may run
//! *while the audio thread is inside `process()`*, exactly like
//! `params.get_value`. Materializing `&mut P` there would be two live
//! `&mut` over one instance.
//!
//! So the block lives here instead, mirroring how parameter values already
//! work: the adapter holds the master copy, the main thread reads and
//! writes it (session load, and later the editor), and the audio thread
//! receives it inside `process()` through
//! [`Plugin::apply_state`](seco_core::Plugin::apply_state). The plugin
//! never owns the copy that gets saved, so nothing has to be read back out
//! of it at an unsafe moment.
//!
//! # The hand-off
//!
//! A triple buffer: three fixed-size slots and an atomic index. The
//! producer always writes to a slot the consumer does not hold, the
//! consumer always reads a slot the producer will not touch, and neither
//! ever waits for the other. `publish` cannot block the audio thread
//! because the audio thread never takes a lock, and `take_with` cannot
//! allocate because every slot is preallocated.
//!
//! The invariant that makes it sound: `back`, `front` and `shared` always
//! hold a permutation of `{0, 1, 2}`. Both sides move an index by swapping
//! it *through* `shared`, one atomic exchange each, so no interleaving can
//! give both sides the same slot.

use std::cell::UnsafeCell;
use std::sync::atomic::{
    AtomicU8, AtomicU64,
    Ordering::{AcqRel, Relaxed},
};

/// Capacity of a plugin's state block. A block larger than this is refused
/// rather than truncated: silently losing the tail of a saved curve is
/// worse than failing the load.
///
/// Costs four slots' worth of memory per instance (master copy plus the
/// three buffers), which is the price of never allocating on the hand-off.
pub const MAX_PLUGIN_STATE: usize = 4096;

/// One slot: fixed bytes plus how many of them are live.
struct Chunk {
    bytes: [u8; MAX_PLUGIN_STATE],
    len: usize,
}

impl Chunk {
    const fn empty() -> Self {
        Self {
            bytes: [0; MAX_PLUGIN_STATE],
            len: 0,
        }
    }

    fn copy_from(&mut self, source: &[u8]) {
        self.bytes[..source.len()].copy_from_slice(source);
        self.len = source.len();
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// Index bits of `shared`.
const INDEX_MASK: u8 = 0b011;
/// Set by the producer, cleared by the consumer: "there is something new".
const UNREAD: u8 = 0b100;

/// The state block plus its hand-off to the audio thread.
pub(crate) struct PluginStateSlot {
    /// Master copy. Main-thread only — the same exclusivity class as the
    /// gui slot — and the one `clap.state.save` writes out.
    latest: UnsafeCell<Chunk>,
    /// Hand-off slots. Each is only ever touched through the index that
    /// currently names it (`back` for the producer, `front` for the
    /// consumer).
    buffers: [UnsafeCell<Chunk>; 3],
    /// Published index, plus [`UNREAD`].
    shared: AtomicU8,
    /// The slot the producer writes into. Producer-only; atomic because it
    /// lives behind a shared reference, not because it is contended.
    back: AtomicU8,
    /// The slot the consumer reads from. Consumer-only, same reasoning.
    front: AtomicU8,
    /// Main-thread revision of `latest`. GUI code uses this to skip parsing
    /// an unchanged editor state block every refresh tick.
    revision: AtomicU64,
}

impl PluginStateSlot {
    pub(crate) fn new() -> Self {
        Self {
            latest: UnsafeCell::new(Chunk::empty()),
            buffers: [
                UnsafeCell::new(Chunk::empty()),
                UnsafeCell::new(Chunk::empty()),
                UnsafeCell::new(Chunk::empty()),
            ],
            shared: AtomicU8::new(2),
            back: AtomicU8::new(0),
            front: AtomicU8::new(1),
            revision: AtomicU64::new(0),
        }
    }

    /// Stores `bytes` and publishes them to the audio thread. Returns false
    /// if the block is too large, having changed nothing.
    ///
    /// # Safety
    ///
    /// Main thread only: `latest` and the `back` slot are borrowed
    /// exclusively, and CLAP serializes every `[main-thread]` callback.
    pub(crate) unsafe fn publish(&self, bytes: &[u8]) -> bool {
        if bytes.len() > MAX_PLUGIN_STATE {
            return false;
        }
        // SAFETY: main-thread exclusivity per this function's contract.
        unsafe { (*self.latest.get()).copy_from(bytes) };

        let back = self.back.load(Relaxed) as usize & INDEX_MASK as usize;
        // SAFETY: the producer owns `back`; the permutation invariant means
        // the consumer cannot be holding the same slot.
        unsafe { (*self.buffers[back].get()).copy_from(bytes) };

        // AcqRel, and both halves are load-bearing — miri caught this as a
        // real data race when it was Release only. Release publishes the
        // writes above to the consumer. *Acquire* is for the other
        // direction: `previous` is the slot the consumer handed back, and
        // the next publish writes into it, so the consumer's last read of
        // it must happen-before that write.
        let previous = self.shared.swap(back as u8 | UNREAD, AcqRel);
        self.back.store(previous & INDEX_MASK, Relaxed);
        self.revision.fetch_add(1, Relaxed);
        true
    }

    /// Runs `f` on the master copy.
    ///
    /// # Safety
    ///
    /// Main thread only, as for [`publish`](Self::publish).
    pub(crate) unsafe fn with_latest<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        // SAFETY: main-thread exclusivity per this function's contract.
        f(unsafe { (*self.latest.get()).as_slice() })
    }

    /// Revision of the main-thread master copy. It is only a cache key; the
    /// state hand-off itself remains governed by `shared` above.
    #[cfg(any(test, all(feature = "gui", target_os = "macos")))]
    pub(crate) fn latest_revision(&self) -> u64 {
        self.revision.load(Relaxed)
    }

    /// Runs `f` on the newly published block, if there is one. Returns
    /// `None` when nothing changed since the last call — the common case,
    /// costing one relaxed load.
    ///
    /// The block is borrowed for the closure only: the next call may swap
    /// the slot out from under it, which the closure form prevents.
    ///
    /// # Safety
    ///
    /// Audio thread only (one such thread exists at a time,
    /// thread-check.h:30-40): `front` is borrowed exclusively.
    pub(crate) unsafe fn take_with<R>(&self, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
        if self.shared.load(Relaxed) & UNREAD == 0 {
            return None;
        }
        let front = self.front.load(Relaxed) & INDEX_MASK;
        // AcqRel for the mirror-image reason: Acquire makes the producer's
        // writes to the slot we are taking visible, Release publishes the
        // slot we are giving back so the producer may write into it.
        let published = self.shared.swap(front, AcqRel);
        self.front.store(published & INDEX_MASK, Relaxed);

        let index = (published & INDEX_MASK) as usize;
        // SAFETY: the exchange above moved `index` out of `shared`, so the
        // producer can no longer be writing to it; the consumer owns it
        // until its next take.
        Some(f(unsafe { (*self.buffers[index].get()).as_slice() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_taken_before_anything_is_published() {
        let slot = PluginStateSlot::new();
        // SAFETY: single-threaded test.
        assert!(unsafe { slot.take_with(|_| ()) }.is_none());
        assert_eq!(
            unsafe { slot.with_latest(<[u8]>::to_vec) },
            Vec::<u8>::new()
        );
    }

    #[test]
    fn a_published_block_arrives_once() {
        let slot = PluginStateSlot::new();
        // SAFETY: single-threaded test.
        unsafe {
            assert!(slot.publish(b"curve"));
            assert_eq!(slot.take_with(<[u8]>::to_vec), Some(b"curve".to_vec()));
            // Taken already: no second delivery, so a plugin is not asked to
            // re-apply the same state every block.
            assert!(slot.take_with(|_| ()).is_none());
            assert_eq!(slot.with_latest(<[u8]>::to_vec), b"curve".to_vec());
        }
    }

    #[test]
    fn revision_changes_only_for_accepted_publishes() {
        let slot = PluginStateSlot::new();
        assert_eq!(slot.latest_revision(), 0);
        // SAFETY: single-threaded test.
        unsafe {
            assert!(slot.publish(b"first"));
            assert_eq!(slot.latest_revision(), 1);
            assert!(!slot.publish(&[0_u8; MAX_PLUGIN_STATE + 1]));
            assert_eq!(slot.latest_revision(), 1);
            assert!(slot.publish(b"second"));
        }
        assert_eq!(slot.latest_revision(), 2);
    }

    /// A consumer that misses a block must still end up with the newest
    /// one, not a stale one.
    #[test]
    fn overwriting_before_a_take_delivers_the_newest() {
        let slot = PluginStateSlot::new();
        // SAFETY: single-threaded test.
        unsafe {
            slot.publish(b"first");
            slot.publish(b"second");
            slot.publish(b"third");
            assert_eq!(slot.take_with(<[u8]>::to_vec), Some(b"third".to_vec()));
        }
    }

    #[test]
    fn an_empty_block_is_a_value_not_a_no_op() {
        let slot = PluginStateSlot::new();
        // SAFETY: single-threaded test.
        unsafe {
            slot.publish(b"curve");
            slot.take_with(|_| ());
            // A session with no block clears the previous one; the plugin
            // must hear about that, or a preset without a curve would keep
            // whatever was loaded before it.
            slot.publish(b"");
            assert_eq!(slot.take_with(<[u8]>::to_vec), Some(Vec::new()));
        }
    }

    #[test]
    fn an_oversized_block_is_refused_and_changes_nothing() {
        let slot = PluginStateSlot::new();
        // SAFETY: single-threaded test.
        unsafe {
            slot.publish(b"keep me");
            slot.take_with(|_| ());
            assert!(!slot.publish(&[0_u8; MAX_PLUGIN_STATE + 1]));
            assert!(
                slot.take_with(|_| ()).is_none(),
                "a refused publish must not signal"
            );
            assert_eq!(slot.with_latest(<[u8]>::to_vec), b"keep me".to_vec());
        }
    }

    /// The indices must stay a permutation of {0,1,2} however the two sides
    /// interleave — that is what keeps the producer and consumer off the
    /// same slot.
    #[test]
    fn indices_stay_disjoint_across_interleavings() {
        let slot = PluginStateSlot::new();
        let seen = |slot: &PluginStateSlot| {
            let mut indices = [
                slot.back.load(Relaxed) & INDEX_MASK,
                slot.front.load(Relaxed) & INDEX_MASK,
                slot.shared.load(Relaxed) & INDEX_MASK,
            ];
            indices.sort_unstable();
            indices
        };
        assert_eq!(seen(&slot), [0, 1, 2]);
        // SAFETY: single-threaded test.
        unsafe {
            for round in 0..8_u8 {
                slot.publish(&[round]);
                assert_eq!(seen(&slot), [0, 1, 2], "after publish {round}");
                if round % 3 != 0 {
                    slot.take_with(|_| ());
                    assert_eq!(seen(&slot), [0, 1, 2], "after take {round}");
                }
            }
        }
    }

    /// The real crossing, under miri's data-race detector: the main thread
    /// publishing while the audio-thread role takes. Every block observed
    /// must be one that was actually written — never a torn mix of two.
    #[test]
    fn publish_races_take_without_ub() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        struct Shared(PluginStateSlot);
        // SAFETY: the slot's whole purpose is this crossing; the type is
        // only shared here the way an Instance is shared between the main
        // and audio threads.
        unsafe impl Sync for Shared {}
        unsafe impl Send for Shared {}

        let shared = Arc::new(Shared(PluginStateSlot::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let producer = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                for round in 0..64_u8 {
                    // A block whose every byte is the round number: any
                    // tear shows up as two different bytes.
                    let block = [round; 64];
                    // SAFETY: this thread stands in for the main thread and
                    // is the only producer.
                    unsafe { shared.0.publish(&block) };
                }
                stop.store(true, Relaxed);
            })
        };

        let mut delivered = 0;
        while !stop.load(Relaxed) {
            // SAFETY: this thread stands in for the audio thread and is the
            // only consumer.
            if let Some(ok) = unsafe {
                shared.0.take_with(|bytes| {
                    bytes.len() == 64 && bytes.iter().all(|byte| *byte == bytes[0])
                })
            } {
                assert!(ok, "observed a torn block");
                delivered += 1;
            }
        }
        producer.join().unwrap();
        let _ = delivered;
    }
}
