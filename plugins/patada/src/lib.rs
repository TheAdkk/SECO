//! patada — tempo-synced ducking.
//!
//! Phase 1: a fixed -6 dB gain, to prove the CLAP plumbing end to end.
//! The actual ducking arrives with the transport and parameter phases.

use seco_clap::seco_export;
use seco_core::{AudioBuffer, Plugin, RtContext};

struct Patada;

impl Plugin for Patada {
    const ID: &'static str = "dev.seco.patada";
    const NAME: &'static str = "patada";
    const VENDOR: &'static str = "SECO";
    const VERSION: &'static str = "0.1.0";
    const DESCRIPTION: &'static str = "Tempo-synced ducking";

    fn new() -> Self {
        Patada
    }

    fn process(&mut self, audio: &mut AudioBuffer, _rt: &RtContext) {
        // 0.5 linear ≈ -6.02 dB: clearly audible, clearly not a bypass.
        for channel in audio.channels_mut() {
            for sample in channel {
                *sample *= 0.5;
            }
        }
    }
}

seco_export!(Patada);
