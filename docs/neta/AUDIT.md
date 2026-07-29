# Neta implementation audit

**Snapshot:** 2026-07-29

This is a build/audit handoff, not a claim that every production integration
is finished. “Done” means code exists, is connected, and has a local check.
“Gap” means user-visible capability still needs work before release.

## Implemented blocks

| Plan block | Delivered | Evidence |
|---|---|---|
| Loudness meter | BS.1770 K-weighting, 4x true peak, M/S/I/LRA/PSR/PLR, bounded history | `neta-meter`; Tech 3341/3342 synthetic tests |
| Scope | min/max waveform, correlation, M/S, width, goniometer, FFT log spectrum, circular spectrogram/waterfall, ballistics | `neta_meter::scope`, fixed storage, scope tests |
| Plugin telemetry | meter/scope analyzed in `process`; bounded atomic slots feed editor; audio untouched | `plugins/neta/src/lib.rs`; RT allocation test |
| Compatibility editor | deliberate Neta dashboard: RT runways, waveform, spectrum-history waterfall, vectorscope | `plugins/neta/src/editor.rs`; macOS WebKit feature |
| Native GPU | WGPU instanced-quad renderer with bounded reusable staging, offscreen readback proof, safe surface recreation, point-cloud path | `neta-wgpu` |
| Standalone app | Winit + WGPU Neta demo paced at 60 Hz, native Dalia cloud (1,024 evenly sampled points from its 12,000-point domain), Space changes Dalia preset | `cargo run -p neta-app -- --demo` |
| Input capture | CPAL default input → fixed SPSC bridge → same meter/scope/Dalia path | `cargo run -p neta-app -- --input` |
| Bridge core | safe preallocated SPSC blocks, release/acquire publication, no producer allocation/lock/wait | `neta-bridge`; concurrent FIFO stress test |
| Dalia native | Dalia compiles as native `rlib`; app calls safe slice API | `dalia-core` native feature split; `neta-app` |
| 3D import | size-bounded OBJ plus glTF/GLB POSITION import, scene-node transforms, centering, bounded point-cloud projection/reactivity | `neta-visual::mesh`, `neta-app --object FILE` |

## Intentional `unsafe` boundary

Neta DSP, scope, bridge, visual composer, parser, plugin shell, and app use
`forbid(unsafe_code)`. `neta-wgpu` contains exactly one public unsafe function:
`create_foreign_surface`. It only wraps WGPU raw host-window surface creation,
documents lifetime/thread requirements, and is unused by the safe standalone
window path. See [ADR-001](ADR-001-native-renderer-and-data-boundaries.md).

## Release blockers / honest gaps

1. **System-output capture is not wired.** `--input` captures the OS default
   input (including a user-selected virtual loopback device). Native system
   mix needs a small platform layer: ScreenCaptureKit on macOS, WASAPI
   loopback on Windows, and PipeWire portal/monitor selection on Linux.
   `--system-audio` refuses rather than pretending it works.
2. **Plugin → standalone bridge is in-process only.** The SPSC protocol is
   verified and used by standalone capture, but the cross-process shared-memory
   mapping/handshake is deliberately not faked. Implement it as a separately
   audited OS adapter around `neta-bridge`, with the same slot protocol.
3. **Plugin GPU embedding is pending.** Neta’s WGPU renderer runs in
   standalone Winit today. The CLAP editor is a macOS WebKit compatibility
   view until the host-child surface adapter calls the audited raw-handle
   boundary. No graphics API leaks into audio/DSP code.
4. **3D is point-cloud visual import, not full scene rendering.** OBJ and
   glTF/GLB POSITION accessors load safely, apply scene-node transforms, and
   center/normalize clouds; triangles, materials, textures, animation, and
   depth rendering remain future work.
5. **Meter validation needs official programme WAV fixtures.** Synthetic
   Tech 3341/3342 cases cover algorithms; fetches of EBU programme assets were
   unavailable in this environment. Add the official cases (not just tones)
   before marketing standards compliance.
6. **SECO CLAP port model is still stereo pass-through.** Neta works as an
   insert meter, but a mastering meter needs configurable input-only and
   multichannel ports in `seco-clap`.
7. **The standalone Dalia dependency currently assumes the sibling checkout
   layout.** `neta-app` references `../../../../dalia/dalia-core`; package a
   git revision, submodule, or vendored release before a clean SECO-only CI
   or distribution build.
8. ~~**Plugin integrated loudness/LRA cannot travel through current generic
   editor slots without doing history work in the audio callback.**~~ Closed.
   `LoudnessHistogram` files every gated block into fixed 0.1 LU bins, so
   integrated, LRA and PLR are O(bins) to read and the audio callback does a
   bounded amount of work per block regardless of programme length. Only the
   gate *decision* is quantized — each bin keeps an exact energy sum — and
   `realtime_gating_agrees_with_the_exact_history_scan` holds the realtime
   path to within one bin of the exact scan, which is kept as the oracle.
   The plugin publishes all three on scope slots 10–12, so the editor shows
   them live rather than pointing at the native app.

## Performance watch list

- WGPU now uploads one 48-byte instance per primitive and synthesizes its six
  triangle vertices on GPU. The fixed 20,000-instance buffer fits the full
  scope/waterfall plus the standalone Dalia cloud. Dalia keeps its original
  12,000-point parameter domain but emits an evenly sampled 1,024-point cloud
  for Neta, never a front-truncated slice.
- Measured on this ARM64 macOS machine at a 1,280×760 native demo window:
  `cargo run --release -p neta-app -- --demo`, then `ps` after more than one
  minute, averaged **4.8% CPU**. That improves the full-12,000-point path's
  **16.3%**, but misses the plan's <2% target. A five-second system sample
  attributes remaining foreground work to scope/meter processing, visual
  composition, and Dalia. Do not market the <2% claim yet.
- The WebKit editor has only 64 waveform, 80 spectrum, and 16 goniometer
  points because the framework’s generic 256-slot picture channel is bounded.
  Native standalone consumes the full scope history.
- `LoudnessMeter::snapshot()` scans/sorts preallocated history. Standalone
  caches I/LRA at one-second audio intervals and uses `realtime_snapshot()`
  every visual frame; plugin audio uses `realtime_snapshot()` only.
- The native window schedules redraws at 60 Hz and stops when Winit reports
  occlusion. A lost/validation surface is recreated from its live safe window
  handle, then retried at most once per second.
- Offscreen WGPU readback proves non-clear shader output. Desktop capture on
  this machine could not read the Winit window content, so final visual polish
  still needs owner-side screen inspection.
- `clap-validator` passes every applicable Neta check (14 pass, 0 fail), but
  reports a **231 ms scan-time warning** against its 100 ms target. Profile
  plugin initialization and adapter load cost before distribution.
- `neta_visual::dalia` is a non-runtime recipe catalogue. The standalone
  runtime source of truth is external `dalia-core`; do not add presets to both
  without a deliberate consolidation.

## Suggested wake-up audit

1. Run `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`.
2. Run `cargo run -p neta-app -- --demo`; resize window and press Space.
3. With an approved input device, run `cargo run -p neta-app -- --input`.
4. Run `cargo run -p neta-app -- --demo --object path/to/model.glb`.
5. Build/bundle plugin with its macOS GUI feature and test DAW reopen/close.
6. Audit each item under “Release blockers” before calling Neta production-ready.
