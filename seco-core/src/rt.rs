use std::marker::PhantomData;

/// Witness that code is running inside the real-time audio callback.
///
/// A `&RtContext` only ever exists inside [`with_rt_context`]: user code
/// cannot construct one (the field is private), cannot send it to another
/// thread (`!Send`), and cannot share it across threads (`!Sync`). APIs that
/// must only run on the audio thread take `&RtContext`, turning misuse into
/// a type error instead of a runtime bug.
///
/// The type is zero-sized; it compiles to nothing.
///
/// Phase 3 wires the debug allocation detector into [`with_rt_context`]: for
/// its duration the thread is flagged as real-time, and any heap allocation
/// panics in debug builds. This matches CLAP's threading model, where the
/// audio thread is a *role* that may hop between OS threads — the flag is
/// scoped per callback, never latched per thread.
pub struct RtContext {
    // Raw pointers are neither `Send` nor `Sync`, so this marker makes the
    // whole type `!Send + !Sync` with zero cost and zero `unsafe`.
    _not_send_not_sync: PhantomData<*const ()>,
}

/// Runs `f` with a real-time context witness.
///
/// This is the only way to obtain a `&RtContext`. Adapter crates call it
/// once per audio callback, wrapping the call into
/// [`Plugin::process`](crate::Plugin::process); the borrow cannot escape the
/// closure.
pub fn with_rt_context<R>(f: impl FnOnce(&RtContext) -> R) -> R {
    f(&RtContext { _not_send_not_sync: PhantomData })
}
