//! Manual proof that the RT allocation detector is armed.
//!
//! Run `cargo run -p patada --example rt_alloc_abort` in a DEBUG build: it
//! must print `seco: heap allocation inside the real-time audio callback`
//! and abort (the allocation happens inside a real-time scope). In release
//! builds the detector compiles away and this exits normally — which is the
//! point: zero cost where it matters.
//!
//! This crosses the `__private` fence deliberately: it IS a runner harness.

// The example binary does not link the patada cdylib (it never names it), so
// `seco_export!`'s registration inside the plugin does not apply here; the
// harness registers the detector itself, exactly like the macro does.
#[global_allocator]
static SECO_RT_ALLOC_CHECK: seco_clap::RtAllocCheck = seco_clap::RtAllocCheck;

fn main() {
    seco_core::__private::with_rt_context(seco_core::Transport::default(), &[], |_| {
        // One heap allocation inside the "audio callback".
        let boom: Vec<u8> = Vec::with_capacity(64);
        std::hint::black_box(boom);
    });
    println!("no abort: detector inactive (release build?)");
}
