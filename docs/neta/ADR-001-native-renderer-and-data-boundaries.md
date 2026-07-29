# ADR-001: Native renderer, safe analysis boundary

**Status:** accepted — 2026-07-29

## Context

Neta needs dense, continuously moving metering in both a CLAP editor and a
standalone application. The audio callback must never wait for a GUI,
allocate, lock, or depend on a windowing API. The current SECO GUI hosts HTML
inside a macOS WebKit view; useful for controls, but not a durable rendering
home for the visual instrument described in the Neta plan.

Plugin hosts provide platform-native parent handles. Turning those handles
into a GPU surface is necessarily an OS/FFI boundary. Keeping that boundary
small matters more than pretending it does not exist.

## Decision

- `neta-meter`, scope analysis, geometry generation, frame serialization,
  bridge queues, and object parsing stay pure safe Rust (`forbid(unsafe_code)`).
- `neta-visual` owns renderer-independent scene data, shader text, palettes,
  and safe CPU-side tests. It must not know CLAP, a window, or audio threads.
- `neta-wgpu` owns `wgpu` surfaces. It is optional and the
  only crate allowed to contain a narrowly documented OS-handle/FFI `unsafe`
  boundary. No application logic may enter that boundary.
- `neta-wgpu` and `neta-app` declare Rust 1.87 because WGPU 29 requires it;
  the SECO workspace baseline remains Rust 1.85 for unrelated crates.
- The standalone app consumes an immutable `VisualFrame`. The plugin projects
  the same analyzer semantics into SECO's fixed 256-slot compatibility
  picture; it cannot carry the full waterfall/history without a framework
  transport extension. The audio thread writes only bounded scalar/point data
  to preallocated analyzers and an SPSC bridge; UI work happens after the copy.
- Until native host-surface support lands, the existing WebKit editor remains
  a compatibility shell that draws bounded waveform/spectrum/vectorscope
  data. It is not the rendering architecture of record.

## Consequences

Good:

- Measurement correctness remains testable without graphics hardware.
- WGPU, WebKit, CoreAudio, and CLAP raw handles cannot leak into DSP code.
- A standalone app and plugin share meter/scope semantics while their bounded
  rendering transports remain explicit and reviewable.
- Any future `unsafe` review has one obvious small target.

Costs:

- Native GPU embedding needs platform-specific lifecycle work before it can
  replace the compatibility editor.
- Snapshot transport has deliberately bounded resolution; UI may skip a
  frame rather than delaying audio.
- An offscreen WGPU readback test proves shader/pipeline output; final
  compositing still needs on-machine visual inspection.

## Rejected alternatives

- Put WGPU/raw handles in the plugin audio crate: couples host lifecycle to
  DSP and invites non-RT work in `process`.
- Share mutable meter state with the UI: requires locking or unsound aliasing.
- Use a WebView as permanent renderer: easy today, but makes high-rate scope,
  shader effects, and standalone parity depend on browser behavior.
