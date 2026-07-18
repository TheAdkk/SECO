#[cfg(debug_assertions)]
use std::cell::Cell;
use std::marker::PhantomData;

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
    f: impl FnOnce(&RtContext) -> R,
) -> R {
    #[cfg(debug_assertions)]
    let _guard = DepthGuard::enter();
    f(&RtContext { transport, params, _not_send_not_sync: PhantomData })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(debug_assertions)]
    #[test]
    fn depth_counts_nested_scopes() {
        assert_eq!(rt_depth(), 0);
        with_rt_context(Transport::default(), &[], |_| {
            assert_eq!(rt_depth(), 1);
            with_rt_context(Transport::default(), &[], |_| {
                assert_eq!(rt_depth(), 2);
            });
            // A bool flag would already be cleared here; the counter is not.
            assert_eq!(rt_depth(), 1);
        });
        assert_eq!(rt_depth(), 0);
    }

    #[test]
    fn param_reads_are_total() {
        with_rt_context(Transport::default(), &[0.25, 0.75], |rt| {
            assert_eq!(rt.param(0), 0.25);
            assert_eq!(rt.param(1), 0.75);
            assert_eq!(rt.param(99), 0.0);
        });
    }
}
