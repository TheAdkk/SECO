# SECO

![CI](https://github.com/TheAdkk/SECO/actions/workflows/ci.yml/badge.svg)

A minimal audio plugin framework in pure Rust, and **Zape**, a tempo-synced
ducking plugin built on it.

The name is a joke: JUCE sounds like "juice" (wet signal). This is SECO (dry).
Backronym: *Sistema Extensible de Componentes de audiO*. A *zape* is a
Mexican affectionate smack upside the head — which is what it does to your
mix, once per beat.

This is a scope-honest portfolio project, not a JUCE or nih-plug competitor:
one plugin format implemented by hand, one real plugin, and the real-time
correctness work documented below.

## Architecture

| Crate | Role | `unsafe` |
|---|---|---|
| `seco-core` | Plugin traits and audio types. Zero dependencies, zero FFI. A `Plugin` compiles without knowing any host ABI exists. | `#![forbid(unsafe_code)]` |
| `seco-clap` | The CLAP adapter: hand-written `#[repr(C)]` mirrors of the CLAP 1.2.10 headers (pinned at commit `195b42a`), entry point, factory, params/state/audio-ports extensions. | The only crate with `unsafe`; every block carries a `SAFETY:` comment citing the header line it relies on |
| `seco-dsp` | Allocation-free DSP utilities: one-pole smoother, curve lookup tables. Pure functions, unit-tested without a host. | `#![forbid(unsafe_code)]` |
| `plugins/zape` | The plugin: multiplies audio by a gain curve indexed by the host's beat position. No sidechain input, no signal analysis. | none |

Build and install: [docs/building.md](docs/building.md). CLAP header findings
with citations and the empirical transport results: [docs/clap-notes.md](docs/clap-notes.md).

## Zape

Tempo-synced ducking ("ghost kick"): `gain = curve(phase)` where `phase` is
the host's beat position folded into a cycle. Four parameters: **Rate**
(1/1 … 1/16), **Mix**, **Curve** (Pump / Punch / Soft), **Bypass**. Edge
cases are handled from evidence, not guesses: hosts freeze the beat position
when stopped (Zape keeps free-running), loop wraps jump it backward without
interpolation (Zape resyncs hard and glides the gain), and hosts without a
tempo get 120 BPM.

## The technical part

### Real-time safety, honestly

Rust has no effect system: a heap allocation inside the audio callback
cannot be made a *compile* error, and anyone claiming otherwise is selling
something. SECO does two real things instead:

1. **Defensive API design.** `process()` receives borrowed buffer views and
   an `RtContext` witness — `!Send`/`!Sync`, no public constructor, only
   lent inside the adapter's callback scope. The safe API offers nothing
   that allocates; buffers are pre-allocated in `activate()`, where the host
   declares the maximum block size.
2. **Debug-build detection.** `seco_export!` registers a wrapping
   `GlobalAlloc`; in debug builds, any allocator call while the thread-local
   real-time depth counter is nonzero writes a static message and aborts.
   Release builds compile the check to a direct forward.

Limits, explicitly: the witness is a convention fence (`__private`,
serde-style), not an unforgeable capability; the detector only exists in
debug builds; and the depth counter must be a *counter*, not a flag —
nested scopes would clear a flag early. First lesson the detector taught:
its own reporting path allocated (std's stderr machinery), recursing into
itself — the report path now carries a re-entrancy guard.

### The aliasing model (a race Rust's rules surfaced)

CLAP's `params.get_value` runs on the main thread *while the audio thread is
inside `process()`*. The first adapter handed every callback `&mut
Instance`; two live `&mut` over one instance is UB regardless of which
fields they touch. The redesign: an instance is only ever borrowed shared;
concurrently-visible values live in atomics; the plugin state sits in an
`UnsafeCell` from which `&mut` is materialized only inside callbacks whose
exclusivity CLAP itself guarantees, each with the guarantee cited. A
barrier-synchronized test races `get_value` against `process()` under miri's
data-race detector (`get_value_races_process_without_ub`).

### The unsafe audit (two real UB holes)

Auditing every `unsafe` against *hostile-but-legal* host inputs — instead of
the well-formed ones tests naturally construct — found two:

- **Event alignment.** CLAP events are specified as memcpy-able blobs,
  never as aligned. Casting a packed-queue event to `&ClapEventParamValue`
  (an align-8 struct) is UB the moment the reference exists — miri:
  *"required 8 byte alignment but found 4"*. Fix: `read_unaligned`, same
  machine code on the targets that matter, no assumption.
  (`param_event_at_unaligned_address_is_read_safely`)
- **Mirrored channel pointers.** Hosts may hand the same buffer in both
  channel slots. Two `&mut [f32]` over that memory is aliasing UB, and the
  gain was applied twice (measured: a half-gain plugin produced 0.25). Fix:
  duplicate pointers collapse to one processed slice.
  (`duplicate_channel_pointers_process_once`)

### The click saga (three bugs, three fail-first tests)

All three were found by ear in REAPER, then reproduced as failing tests
before fixing — the numbers below are from those red runs:

1. **The seam.** The first curve shapes ended the cycle at gain 1.0 and
   began it at 0.0: a full-scale step at every phase wrap, a click per
   cycle. `duck_curves_close_the_seam` failed with `|f(0) − f(1)| = 1`;
   the shapes now close at 1.0 by construction, with the dip inside the
   cycle. A follow-up invariant (`duck_lands_near_the_beat`) pins the
   opposite failure: an attack drawn too wide parks ~36 ms of full volume
   after every beat before the duck lands.
2. **The start-up overshoot.** The gain smoother initialized at 1.0 while
   the curve at the starting phase rarely is: the first ~10 ms played loud.
   `first_sample_starts_on_the_curve_not_at_unity` failed at `0.9906` vs
   the expected `0.0975`; the smoother now snaps onto the curve's actual
   first-sample target — on fresh streams only.
3. **`reset()` mid-stream.** Hosts flush FX around transport changes; the
   session log showed REAPER calling `reset()` on *every* play start
   (`rst=25` in one session). Re-arming the start snap there turned a
   transport jump into a one-sample gain step of `0.424` — a click.
   `reset_plus_jump_glides_instead_of_clicking` pinned it; `reset()` now
   keeps the smoother's value and lets the glide absorb the jump.

A host-realistic offline scan (`click_scan_diagnostic`) measures the worst
per-sample gain step across curves and rates: steady playback tops out at
`0.00686` — attack slope, no discontinuity.

## Formats, honestly

- **CLAP** is the native format: pure Rust from entry symbol to DSP.
- **VST3** (`--features vst3`, off by default) is produced by
  [free-audio/clap-wrapper](https://github.com/free-audio/clap-wrapper),
  which re-hosts the compiled CLAP plugin and presents it as a VST3 —
  compiled in via the [`clap-wrapper` crate](https://crates.io/crates/clap-wrapper).
  **That wrapper is C++ inside**, and it embeds Steinberg's VST 3 SDK
  (MIT-licensed as of the current SDK). SECO's core is pure Rust; the
  format adapters are borrowed. A binary built with `vst3` enabled is not
  "100% Rust", and this README will not pretend otherwise.

VST® is a trademark of Steinberg Media Technologies GmbH. This project only
claims compatibility and uses no Steinberg branding.

## Known limits

- Parameters and transport are read once per block; sample-accurate event
  splitting is not implemented.
- "Beat = quarter note" is verified empirically in REAPER (and
  clap-validator); a second real DAW has not cross-checked it.
- `tempo_inc` ramps and mid-block transport events have never been observed
  in the tested hosts and are not consumed sample-accurately.
- Stereo only (`MAX_CHANNELS = 2`); one plugin per binary (the descriptor
  storage is a single static, enforced by the duplicate `clap_entry` link
  error).
- Curve attacks are drawn in phase, so their absolute duration scales with
  rate and tempo.
- Buffer safety assumes hosts don't hand *partially* overlapping in/out
  buffers (exact in-place aliasing is handled; partial overlap is a host
  contract violation, documented at the `SAFETY:` site).
