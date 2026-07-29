#[cfg(debug_assertions)]
use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

use crate::Transport;

#[cfg(debug_assertions)]
thread_local! {
    /// How many real-time scopes are live on this thread. A nesting counter,
    /// not a bool: with nested runners a bool would be cleared by the
    /// innermost exit while the outer callback is still real-time. Scoped
    /// per callback, never latched per thread — CLAP's audio thread is a
    /// role that may hop between OS threads.
    static RT_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// RAII guard for the thread-local real-time depth counter (debug builds).
#[cfg(debug_assertions)]
struct DepthGuard;

#[cfg(debug_assertions)]
impl DepthGuard {
    fn enter() -> Self {
        RT_DEPTH.with(|d| d.set(d.get() + 1));
        DepthGuard
    }
}

#[cfg(debug_assertions)]
impl Drop for DepthGuard {
    fn drop(&mut self) {
        RT_DEPTH.with(|d| d.set(d.get() - 1));
    }
}

/// True while the current thread is inside a real-time scope (debug builds).
/// The allocation detector in the adapter layer consults this on every
/// allocator call.
#[cfg(debug_assertions)]
pub fn rt_depth() -> u32 {
    RT_DEPTH.with(Cell::get)
}

/// Witness and per-callback context for the real-time audio thread.
///
/// Carries everything a plugin may consult during one `process()` call —
/// the [`Transport`] and the parameter snapshot — so
/// [`Plugin::process`](crate::Plugin::process) keeps one stable context
/// parameter as the framework grows, instead of sprouting a new argument per
/// phase.
///
/// What the type system actually enforces: the type has no public
/// constructor, is `!Send`/`!Sync` (the borrow cannot leave its thread), and
/// is only lent inside a closure (the borrow cannot escape it). What is
/// convention, not enforcement: that the closure runner is only invoked by an
/// adapter inside a real audio callback. The runner lives in
/// `seco_core::__private` — a serde/tokio-style fence, deliberately a
/// signpost rather than a wall. Rust cannot make this unforgeable; code that
/// reaches into `__private` gets a witness that lies, and owns the
/// consequences.
///
/// In debug builds, entering the runner marks the thread real-time for its
/// duration (nesting counter, see `RT_DEPTH`); the adapter's allocation
/// detector turns any heap use inside it into an abort. Release builds
/// compile the marking to nothing.
pub struct RtContext<'a> {
    transport: Transport,
    params: &'a [f64],
    scope: &'a [AtomicU32],
    // Raw pointers are neither `Send` nor `Sync`, so this marker makes the
    // whole type `!Send + !Sync` with zero cost and zero `unsafe`.
    _not_send_not_sync: PhantomData<*const ()>,
}

impl RtContext<'_> {
    /// Musical time information for the current block, as of its first
    /// sample.
    pub fn transport(&self) -> &Transport {
        &self.transport
    }

    /// The plain value of the parameter at `index` (its position in
    /// [`Plugin::PARAMS`](crate::Plugin::PARAMS)), snapshotted at the start
    /// of the block. Out-of-range indices read as `0.0` — total, so the
    /// audio thread never panics on a bad index.
    pub fn param(&self, index: usize) -> f64 {
        self.params.get(index).copied().unwrap_or(0.0)
    }

    /// How many scope buckets the adapter provides. Zero when there is no
    /// editor to draw them.
    pub fn scope_len(&self) -> usize {
        self.scope.len()
    }

    /// Publishes one bucket of visualization data for the editor —
    /// a meter, an envelope, whatever the plugin wants drawn.
    ///
    /// This is the audio thread talking to the editor, the opposite
    /// direction from [`Plugin::apply_state`](crate::Plugin::apply_state),
    /// and it is deliberately the weakest channel in the framework: plain
    /// relaxed stores into a fixed array of atomics. Nothing blocks,
    /// nothing allocates, and nothing is coordinated — a reader can see
    /// bucket 3 from this block next to bucket 4 from the previous one.
    ///
    /// That is the right trade for a picture and the wrong one for
    /// anything else: never send state a decision depends on through here.
    /// Out-of-range indices and non-finite values are ignored, so the audio
    /// thread cannot panic on a bad one.
    pub fn set_scope(&self, index: usize, value: f32) {
        if !value.is_finite() {
            return;
        }
        if let Some(slot) = self.scope.get(index) {
            slot.store(value.to_bits(), Relaxed);
        }
    }
}

/// Runs `f` with a real-time context. Adapter crates call this once per
/// audio callback, wrapping the call into
/// [`Plugin::process`](crate::Plugin::process).
///
/// Exposed only through `seco_core::__private`: see [`RtContext`] for what
/// that fence does and does not guarantee.
pub fn with_rt_context<R>(
    transport: Transport,
    params: &[f64],
    scope: &[AtomicU32],
    f: impl FnOnce(&RtContext) -> R,
) -> R {
    #[cfg(debug_assertions)]
    let _guard = DepthGuard::enter();
    f(&RtContext {
        transport,
        params,
        scope,
        _not_send_not_sync: PhantomData,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(debug_assertions)]
    #[test]
    fn depth_counts_nested_scopes() {
        assert_eq!(rt_depth(), 0);
        with_rt_context(Transport::default(), &[], &[], |_| {
            assert_eq!(rt_depth(), 1);
            with_rt_context(Transport::default(), &[], &[], |_| {
                assert_eq!(rt_depth(), 2);
            });
            // A bool flag would already be cleared here; the counter is not.
            assert_eq!(rt_depth(), 1);
        });
        assert_eq!(rt_depth(), 0);
    }

    /// The scope is a picture, not a protocol: a plugin must be able to
    /// write to it without checking anything, including when there is no
    /// editor and the array is empty.
    #[test]
    fn scope_writes_are_total() {
        let scope: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
        with_rt_context(Transport::default(), &[], &scope, |rt| {
            assert_eq!(rt.scope_len(), 2);
            rt.set_scope(0, 0.5);
            rt.set_scope(9, 1.0); // out of range
            rt.set_scope(1, f32::NAN); // never publish a NaN to a canvas
        });
        assert_eq!(f32::from_bits(scope[0].load(Relaxed)), 0.5);
        assert_eq!(f32::from_bits(scope[1].load(Relaxed)), 0.0);

        // No editor, no buckets, no special case for the plugin.
        with_rt_context(Transport::default(), &[], &[], |rt| {
            assert_eq!(rt.scope_len(), 0);
            rt.set_scope(0, 1.0);
        });
    }

    #[test]
    fn param_reads_are_total() {
        with_rt_context(Transport::default(), &[0.25, 0.75], &[], |rt| {
            assert_eq!(rt.param(0), 0.25);
            assert_eq!(rt.param(1), 0.75);
            assert_eq!(rt.param(99), 0.0);
        });
    }
}
