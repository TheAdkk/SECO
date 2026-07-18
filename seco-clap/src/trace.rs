//! Phase 2 scaffolding: observes transport values from outside the audio
//! thread, per the phase requirement (no `println!` in `process()`).
//!
//! The audio thread only does relaxed atomic stores into [`TransportTrace`];
//! a background thread started at instance creation samples the atomics
//! every 250 ms and appends changed snapshots to `/tmp/zape-transport.log`.
//! Compiled only into debug builds; release builds carry none of this.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::ffi::{CLAP_BEATTIME_FACTOR, CLAP_SECTIME_FACTOR, ClapEventTransport, ClapProcess};

const LOG_PATH: &str = "/tmp/zape-transport.log";
const SAMPLE_EVERY: Duration = Duration::from_millis(250);

/// One relaxed-atomic slot per transport fact worth observing. Written by
/// the audio thread once per block, read by the logger thread.
#[derive(Default)]
pub(crate) struct TransportTrace {
    pub(crate) blocks: AtomicU64,
    /// Mid-block `CLAP_EVENT_TRANSPORT` events seen in `in_events`.
    pub(crate) transport_events: AtomicU64,
    /// Host lifecycle calls — how often the host re-activates, resets, or
    /// restarts processing. Diagnoses flush-on-transport-change behavior.
    pub(crate) activations: AtomicU64,
    pub(crate) resets: AtomicU64,
    pub(crate) processing_starts: AtomicU64,
    has_transport: AtomicBool,
    flags: AtomicU32,
    tempo_bits: AtomicU64,
    ppq_bits: AtomicU64,
    seconds_bits: AtomicU64,
    bar_start_bits: AtomicU64,
    bar_number: AtomicI32,
    /// Packed `numerator << 16 | denominator`.
    tsig: AtomicU32,
    steady_time: AtomicI64,
}

impl TransportTrace {
    /// Called from the audio thread, once per `process()`. Relaxed stores
    /// only: independent facts for a human reader, no ordering needed.
    pub(crate) fn record(&self, process: &ClapProcess, transport: Option<&ClapEventTransport>) {
        self.blocks.fetch_add(1, Relaxed);
        self.steady_time.store(process.steady_time, Relaxed);
        self.has_transport.store(transport.is_some(), Relaxed);
        if let Some(tp) = transport {
            self.flags.store(tp.flags, Relaxed);
            self.tempo_bits.store(tp.tempo.to_bits(), Relaxed);
            let ppq = tp.song_pos_beats as f64 / CLAP_BEATTIME_FACTOR as f64;
            self.ppq_bits.store(ppq.to_bits(), Relaxed);
            let secs = tp.song_pos_seconds as f64 / CLAP_SECTIME_FACTOR as f64;
            self.seconds_bits.store(secs.to_bits(), Relaxed);
            let bar_start = tp.bar_start as f64 / CLAP_BEATTIME_FACTOR as f64;
            self.bar_start_bits.store(bar_start.to_bits(), Relaxed);
            self.bar_number.store(tp.bar_number, Relaxed);
            self.tsig
                .store(u32::from(tp.tsig_num) << 16 | u32::from(tp.tsig_denom), Relaxed);
        }
    }

    fn snapshot_line(&self, host: &str) -> String {
        let tsig = self.tsig.load(Relaxed);
        format!(
            "host={host} pid={pid} blocks={blocks} tp_events={tp_events} \
             act={act} rst={rst} strt={strt} \
             has_tp={has_tp} flags={flags:#010b} tempo={tempo:.6} ppq={ppq:.6} \
             sec={sec:.6} bar_start={bar_start:.6} bar#={bar} tsig={num}/{den} \
             steady={steady}",
            pid = std::process::id(),
            blocks = self.blocks.load(Relaxed),
            tp_events = self.transport_events.load(Relaxed),
            act = self.activations.load(Relaxed),
            rst = self.resets.load(Relaxed),
            strt = self.processing_starts.load(Relaxed),
            has_tp = self.has_transport.load(Relaxed),
            flags = self.flags.load(Relaxed),
            tempo = f64::from_bits(self.tempo_bits.load(Relaxed)),
            ppq = f64::from_bits(self.ppq_bits.load(Relaxed)),
            sec = f64::from_bits(self.seconds_bits.load(Relaxed)),
            bar_start = f64::from_bits(self.bar_start_bits.load(Relaxed)),
            bar = self.bar_number.load(Relaxed),
            num = tsig >> 16,
            den = tsig & 0xffff,
            steady = self.steady_time.load(Relaxed),
        )
    }
}

/// Samples `trace` until `stop` is set, appending a line to the log whenever
/// the block counter moved. Runs on its own thread — never the audio thread.
pub(crate) fn spawn_logger(
    host: String,
    trace: Arc<TransportTrace>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let Ok(mut file) = OpenOptions::new().create(true).append(true).open(LOG_PATH) else {
            return;
        };
        let mut last_blocks = u64::MAX;
        while !stop.load(Relaxed) {
            std::thread::sleep(SAMPLE_EVERY);
            let blocks = trace.blocks.load(Relaxed);
            if blocks != last_blocks {
                last_blocks = blocks;
                let _ = writeln!(file, "{}", trace.snapshot_line(&host));
                let _ = file.flush();
            }
        }
    })
}
