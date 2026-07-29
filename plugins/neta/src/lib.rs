//! Neta — a loudness meter.
//!
//! *La neta* is the truth, and that is the whole product: what a mix
//! actually measures, when three hours of listening have stopped being
//! evidence.
//!
//! This crate is the CLAP shell. The measurement lives in `neta-meter`,
//! which knows nothing about plugins or hosts, so a standalone application
//! can be a second shell around the same numbers rather than a second
//! implementation of them.
//!
//! # Where it stands
//!
//! It loads, passes audio through untouched, and measures nothing yet. That
//! is deliberately the whole of it: the scaffold is real — it builds,
//! bundles and passes clap-validator — and no measurement is faked in the
//! meantime. The order from here is the order the specification is written
//! in: K-weighting, mean square and gating, then true peak, then the
//! display.
//!
//! # What the framework still owes this plugin
//!
//! - `seco-clap` hard-codes one stereo input and one stereo output. A meter
//!   wants an input and no output at all, and a mastering meter wants more
//!   than two channels. Passing through is correct for an insert meter, so
//!   this is a ceiling rather than a blocker today.
//! - The editor is macOS-only, and a meter is mostly editor.

use neta_meter::Biquad;
use seco_clap::seco_export;
use seco_core::{AudioBuffer, Plugin, RtContext};

struct Neta {
    /// One K-weighting chain per channel, once there is one to run. Kept
    /// here so `activate` owns the allocation and `process` never does.
    weighting: Vec<Biquad>,
}

impl Plugin for Neta {
    const ID: &'static str = "dev.seco.neta";
    const NAME: &'static str = "Neta";
    const VENDOR: &'static str = "SECO";
    const VERSION: &'static str = "0.1.0";
    const DESCRIPTION: &'static str = "Loudness meter";

    // A meter has nothing to automate yet. `PARAMS` defaults to empty, and
    // the adapter reports zero parameters rather than inventing one.

    fn new() -> Self {
        Neta { weighting: Vec::new() }
    }

    fn activate(&mut self, _sample_rate: f64, _max_frames: u32) {
        // Where the filter chain gets built, once it exists. Allocation
        // belongs here: the host has told us the block size, and `process`
        // must not touch the heap.
        self.weighting.clear();
    }

    fn process(&mut self, audio: &mut AudioBuffer, _rt: &RtContext) {
        // A meter is an observer: the audio leaves exactly as it arrived.
        // `process` receives the buffer mutably because the framework has
        // one shape for every plugin, not because this one writes to it.
        let _ = audio;
    }
}

seco_export!(Neta);

#[cfg(feature = "vst3")]
clap_wrapper::export_vst3!();
