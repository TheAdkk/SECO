use std::marker::PhantomData;

use crate::Transport;

/// Witness and per-callback context for the real-time audio thread.
///
/// Carries everything a plugin may consult during one `process()` call —
/// today the [`Transport`] — so [`Plugin::process`](crate::Plugin::process)
/// keeps one stable context parameter as the framework grows, instead of
/// sprouting a new argument per phase.
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
/// Phase 3 wires the debug allocation detector into the runner: for its
/// duration the thread is flagged as real-time, and heap allocation panics in
/// debug builds. Two constraints already known: the flag must be scoped per
/// callback, never latched per thread (CLAP's audio thread is a role that may
/// hop between OS threads), and it must be a nesting *counter*, not a bool —
/// with nested runner calls, a bool would be cleared by the innermost exit
/// while the outer callback is still real-time.
pub struct RtContext {
    transport: Transport,
    // Raw pointers are neither `Send` nor `Sync`, so this marker makes the
    // whole type `!Send + !Sync` with zero cost and zero `unsafe`.
    _not_send_not_sync: PhantomData<*const ()>,
}

impl RtContext {
    /// Musical time information for the current block, as of its first
    /// sample.
    pub fn transport(&self) -> &Transport {
        &self.transport
    }
}

/// Runs `f` with a real-time context. Adapter crates call this once per
/// audio callback, wrapping the call into
/// [`Plugin::process`](crate::Plugin::process).
///
/// Exposed only through `seco_core::__private`: see [`RtContext`] for what
/// that fence does and does not guarantee.
pub fn with_rt_context<R>(transport: Transport, f: impl FnOnce(&RtContext) -> R) -> R {
    f(&RtContext { transport, _not_send_not_sync: PhantomData })
}
