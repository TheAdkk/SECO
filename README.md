# SECO

A minimal audio plugin framework in pure Rust, and **patada**, a tempo-synced
ducking plugin built on it.

The name is a joke: JUCE sounds like "juice" (wet signal). This is SECO (dry).
Backronym: *Sistema Extensible de Componentes de audiO*.

## What this is

- `seco-core` — plugin traits and audio types. `#![forbid(unsafe_code)]`,
  zero dependencies, zero FFI. A `Plugin` compiles without knowing any host
  ABI exists.
- `seco-clap` — the CLAP adapter. Hand-written `#[repr(C)]` mirrors of the
  CLAP 1.2.10 headers, every `unsafe` block carrying a `SAFETY:` comment
  citing the header line it relies on. The only crate in the workspace with
  `unsafe`.
- `seco-dsp` — allocation-free DSP utilities (one-pole smoother, curve
  tables), pure and unit-tested.
- `plugins/patada` — the plugin. Multiplies audio by a gain curve indexed by
  the host's beat position. No sidechain input, no signal analysis. Zero
  `unsafe`.

Build and install instructions: [docs/building.md](docs/building.md).
CLAP header findings with citations: [docs/clap-notes.md](docs/clap-notes.md).

## Formats, honestly

- **CLAP** is the native format: pure Rust from entry symbol to DSP.
- **VST3** (`--features vst3`, off by default) is produced by
  [free-audio/clap-wrapper](https://github.com/free-audio/clap-wrapper),
  which re-hosts the compiled CLAP plugin and presents it as a VST3 —
  compiled into the binary via the [`clap-wrapper`
  crate](https://crates.io/crates/clap-wrapper). **That wrapper is C++
  inside**, and it embeds Steinberg's VST 3 SDK (MIT-licensed as of the
  current SDK). SECO's core is pure Rust; the format adapters are borrowed.
  A binary built with `vst3` enabled is not "100% Rust", and this README
  will not pretend otherwise.

VST® is a trademark of Steinberg Media Technologies GmbH. This project only
claims compatibility and uses no Steinberg branding.

## Real-time safety, honestly

Rust has no effect system: heap allocation in the audio callback cannot be
made a *compile* error, and anyone claiming otherwise is selling something.
SECO does two real things instead:

1. **Defensive API design** — `process()` receives borrowed buffer views and
   an `RtContext` witness (`!Send`/`!Sync`, no public constructor), so the
   safe API surface offers nothing that allocates.
2. **Debug-build detection** — a wrapping `GlobalAlloc` aborts with a
   message if anything allocates inside the audio callback. Release builds
   compile the check away entirely.
