use std::marker::PhantomData;

/// Witness that code is running inside the real-time audio callback.
///
/// APIs that must only run on the audio thread take `&RtContext`, so calling
/// them elsewhere fails to compile unless a witness is in scope.
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
/// The type is zero-sized; it compiles to nothing.
///
/// Phase 3 wires the debug allocation detector into the runner: for its
/// duration the thread is flagged as real-time, and heap allocation panics in
/// debug builds. Two constraints already known: the flag must be scoped per
/// callback, never latched per thread (CLAP's audio thread is a role that may
/// hop between OS threads), and it must be a nesting *counter*, not a bool —
/// with nested runner calls, a bool would be cleared by the innermost exit
/// while the outer callback is still real-time.
pub struct RtContext {
    // Raw pointers are neither `Send` nor `Sync`, so this marker makes the
    // whole type `!Send + !Sync` with zero cost and zero `unsafe`.
    _not_send_not_sync: PhantomData<*const ()>,
}

/// Runs `f` with a real-time context witness. Adapter crates call this once
/// per audio callback, wrapping the call into
/// [`Plugin::process`](crate::Plugin::process).
///
/// Exposed only through `seco_core::__private`: see [`RtContext`] for what
/// that fence does and does not guarantee.
pub fn with_rt_context<R>(f: impl FnOnce(&RtContext) -> R) -> R {
    f(&RtContext { _not_send_not_sync: PhantomData })
}
