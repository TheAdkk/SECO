# Neta — plan

*La neta* is the truth. The product is one sentence: **what your audio
actually measures, and what it actually looks like, without leaving the
screen.**

Three things people currently buy separately:

| What | Reference | Price |
|---|---|---|
| A broadcast-grade loudness meter | Youlean Loudness Meter 2 | free / 30 USD pro |
| An always-on scope that lives over everything | MiniMeters | 20 USD |
| An audio-reactive visualizer | Astro Visuals | 15 USD |

Neta is all three, for 10 USD. Zape stays free — a ducking plugin that
costs money is a ducking plugin nobody installs.

---

## 1. The one thing that decides everything else

**Where does the audio come from?** Every other decision follows from this,
and the three references answer it differently: Youlean is a plugin (it sees
one bus), MiniMeters is an application (it sees the system).

Neta needs both, so it is built as **one measurement core with two shells**:

```
neta-meter     the measurement — no host, no window, no plugin format
neta-visual    the visuals — geometry, shaders, no window
  ↑        ↑
neta (plugin)  neta-app (standalone)
```

That split already exists in the repository (`plugins/neta/meter`). Nothing
in this plan is allowed to violate it: the day a measurement needs a window
or a plugin API to work, the standalone application becomes a rewrite.

### Audio sources, by difficulty

| Source | Platform | Difficulty | Permission |
|---|---|---|---|
| Plugin on a track/bus | all | **done** — the framework exists | none |
| WASAPI loopback | Windows | easy, built into the OS | none |
| PipeWire / PulseAudio monitor | Linux | easy | none |
| Core Audio process tap | macOS 14.4+ | medium | audio-capture prompt |
| ScreenCaptureKit audio | macOS 13+ | medium | **screen recording** prompt |
| Virtual device (BlackHole) | macOS < 14.4 | user installs a driver | none, but a driver |
| Plugin → app over shared memory | all | medium | none |

Two things worth staring at:

- **Windows and Linux are the easy platforms here**, which inverts the usual
  order. Loopback capture is a built-in OS feature on both.
- **macOS is the hard one**, and asking a music user for *Screen Recording*
  permission so an audio app can hear them is a bad prompt to have to
  explain. The Core Audio process tap (14.4+) is the right path and the
  ScreenCaptureKit route is the fallback for 13.x.

The **plugin → app** path sidesteps macOS permissions entirely for DAW
users: the plugin already has the audio, and it can hand it to the
application through a shared-memory ring buffer. That is also the only way
to meter *one track* in the standalone window.

---

## 2. Rendering: where the efficiency claim is won or lost

The pitch is "ours is lighter". That is a claim about the renderer, and it
is only true if the renderer is chosen for it.

Zape's editor is a WKWebView. It is the right tool there: a small window,
repainting at 30 Hz, only while open. **It is the wrong tool for Neta.** A
meter repaints every frame, forever, full window, and a browser engine
compositing that is exactly the CPU cost this product is supposed to
undercut.

**Decision: wgpu, one renderer, both shells.**

- One backend per platform, from one codebase: Metal on macOS, D3D12 on
  Windows, Vulkan on Linux.
- The work lands on the GPU, where drawing a spectrum belongs.
- It is what the standalone application needs anyway, so writing it twice is
  avoided rather than deferred.

The cost is honest and should be stated: this needs a **non-webview GUI path
in `seco-clap`** — a raw `NSView`/`HWND`/`X11` surface handed to wgpu instead
of a webview. That is real framework work, and it is the single largest
engineering item in this plan. Zape keeps its webview; the two paths
coexist behind `Plugin::EDITOR`.

Budget to hold ourselves to, measured rather than asserted:

- **< 2% CPU** on an M-series laptop with the window open at 60 fps.
- **0 allocations** on the audio thread — enforced the way Zape's are, by
  the detector that aborts.
- The renderer stops existing when the window closes, as Zape's already does.

---

## 3. Dalia is not a dependency to port. It is a crate to link.

`~/Documents/GitHub/dalia` is a Rust/WASM audio-reactive engine with 48
commits: FFT analysis, 7-band energy, chromagram, BPM and beat phase,
spectral flux gating, transient detection, lookahead energy forecasting, 20
procedural geometry presets, a mashup controller, and a harmonic colour
system.

The important finding, from reading it:

```
audio.rs        898 lines   wasm_bindgen: 0   web_sys: 0   js_sys: 0
geometry.rs     999 lines   wasm_bindgen: 0   web_sys: 0   js_sys: 0
color.rs        459 lines   wasm_bindgen: 0   ...
presets.rs      198 lines   wasm_bindgen: 0
unit_lattice.rs 326 lines   wasm_bindgen: 0
mashup.rs        72 lines   wasm_bindgen: 0
lib.rs          241 lines   wasm_bindgen: 4   ← the entire browser coupling
```

**The engine is already browser-free.** The WASM surface is four
annotations in one file. So the integration is not a port:

1. `crate-type = ["cdylib", "rlib"]` — keep the WASM artifact, add the
   native library.
2. Move the `wasm_bindgen` surface behind a `wasm` feature (default-on for
   the web build, off for Neta), so `wasm-bindgen`, `js-sys` and `web-sys`
   become optional dependencies.
3. Neta depends on the crate directly, natively. No WASM runtime, no JS
   bridge, no serialization across a boundary.

What transfers unchanged: **all of the analysis and all of the geometry
generation**, because vertices are computed in Rust today and only *drawn*
by Three.js.

What does not transfer: the Three.js rendering, the post-processing chain
(bloom, chromatic aberration, glitch) and the camera work — those are
WebGL through Three.js and become wgpu shaders. That is the real work of
the visuals section, and it is shader work rather than logic work.

Open question for the owner of both repos: **does dalia stay its own
product** (the Vercel demo keeps shipping) with Neta as a second consumer,
or does it fold in? The plan assumes the former — it costs nothing and the
web demo is a marketing asset for the plugin.

---

## 4. What Neta contains

Three rooms, switchable, each independently useful. Everything in room 1 is
specified by a standards body; everything in room 3 is taste.

### Room 1 — Meter (the Youlean half)

The numbers, and the numbers must be right.

**P0 — the measurement**
- Momentary (400 ms), short-term (3 s) and integrated loudness, ITU-R
  BS.1770-4: K-weighting, mean square, channel weights, EBU R128 gating
  (absolute −70 LUFS, relative −10 LU).
- True peak, BS.1770-4 annex 2, ≥ 4× oversampled.
- Sample peak and RMS per channel.
- Loudness range (LRA), EBU Tech 3342.
- PSR / PLR (dynamics), as Youlean shows them.

**P0 — correctness proof**
- Verified against **EBU Tech 3341 compliance signals**, which publish the
  reading each test signal must produce and the tolerance. This is not
  optional and it is not a stretch goal: a meter that cannot quote its
  result on those signals is asking to be believed rather than checked.
  Test names in the suite map one-to-one onto test functions.

**P1 — the display**
- The vertical LUFS bar with the target bracket, the histogram, and the
  time graph with short-term and integrated traces.
- Delivery targets as presets: Spotify −14, YouTube −14, Apple Music −16,
  broadcast R128 −23, club/DJ. The target moves the bracket, nothing else.
- True-peak overs marked on the timeline.
- A/B comparison of two measurement runs.

**P2**
- Report export: CSV and PNG of the session, for delivery paperwork.
- Automatic loudness normalization preview ("what Spotify will do to this").

### Room 2 — Scope (the MiniMeters half)

Always on, always readable, cheap enough to leave open.

**P0**
- Spectrum analyzer, log frequency, with configurable ballistics.
- Spectrogram (the waterfall in the reference screenshot).
- Oscilloscope, waveform, correlation meter, goniometer / vectorscope.
- Stereo width and mid/side balance.

**P1**
- Note and frequency readout under the cursor (`86.64 Hz | F2 | −13 cents`).
- Peak hold, freeze, and a scrolling history buffer.
- Layout that survives being 200 px tall on a second monitor.

### Room 3 — Visuals (the Astro / Dalia half)

Where the product stops being a tool and becomes a thing people show off.

**P0**
- Dalia's 20 procedural presets, driven by dalia's analysis, rendered in
  wgpu.
- Harmonic colour from the chromagram, as dalia already computes it.
- Beat-locked preset switching (dalia's mashup controller).

**P1 — the 3D room**
- **Import your own model**: `.obj` first (a text format, short parser),
  `.glb`/glTF second (the correct long-term answer — binary, PBR, one file).
  Explicitly **not** `.blend`: it is Blender's internal memory dump,
  versioned, and read by essentially nothing but Blender. Export from it.
- Your logo, spinning, reacting to the audio — the machinery already exists
  and ships in Zape today (a lathed bottle in WebGL, spun by Mix, leaning
  into the beat). This is that, with the mesh loaded instead of generated.
- Glitch, chromatic aberration, bloom, RGB split, datamosh — all
  audio-reactive. Dalia already has the post-processing design; this is a
  shader port.
- PS2-era rendering as a first-class look, not a joke: nearest-neighbour
  textures, vertex wobble, no filtering, affine texture mapping. It is
  cheap, it is distinctive, and it is on brand.

**P2**
- Video capture of the visuals for social posts (the actual reason people
  buy visualizers).
- Fullscreen / second-monitor mode.
- Preset sharing, the way the curve library works in Zape: one small file
  per preset, in the user's data folder, mailable to a friend.

---

## 5. What the framework still owes

Blocking, in order:

1. **`audio_ports.rs` hard-codes one stereo input and one stereo output.**
   A meter wants an input and no output; a mastering meter wants more than
   two channels. This is the ceiling Zape never hit and Neta hits on day
   one. The fix is moving port layout into the `Plugin` trait.
2. **No GPU editor path.** `seco-clap` can only make a webview. Section 2 is
   the argument for why Neta cannot use it.
3. **The editor is macOS-only.** For a paid, visual product, that is not a
   footnote — it is two thirds of the market.
4. **No latency or tail reporting.** A meter needs neither; noting it here
   so the delay plugin does not rediscover it.

Non-blocking but relevant: parameters are read once per block, and there is
no sample-accurate event handling. A meter does not care.

---

## 6. Selling it

**Price: 10 USD.** Against MiniMeters at 20 and Astro at 15, that is the
whole positioning, and it only works if the product does not feel like a
tenth of the price. It shouldn't: the measurement is standards-compliant and
the visuals are an engine that already exists.

Practical machinery, none of it built yet:

- **Payment**: Lemon Squeezy or Paddle act as merchant of record and handle
  VAT/IVA on international sales — which matters selling from Mexico to the
  EU. Gumroad is simpler and takes more. Stripe direct is cheapest and
  leaves tax handling to you. Fees at a 10 USD price point are 5–10%.
- **Licensing**: a key plus offline activation. Keep it light. At 10 USD,
  aggressive DRM costs more in support tickets and false positives than it
  recovers, and it always gets cracked anyway. The people who would pirate a
  10 USD plugin were never going to buy it.
- **Trial**: full-featured with a nag or a timed session, not crippled
  features. The visuals *are* the demo — hiding them hides the reason to buy.
- **Free vs paid**: Zape free forever. A possible split for Neta is meters
  free / visuals paid, which makes the free tier a genuine Youlean
  competitor and the paid tier the fun part. Worth deciding before launch,
  not after.

### The one collision worth naming

**A paid macOS app that asks for audio-capture permission while unsigned is
a conversion problem.** For a free plugin, the Gatekeeper warning is a
minor annoyance and the Read Me explains it. For a 10 USD purchase, being
told by the operating system that the software is from an unidentified
developer — right before it asks to listen to your computer — is where
people close the window and ask for a refund.

Apple's Developer ID is 99 USD a year. **That is ten sales.** It is a cost
of doing business for a paid product, in a way it never was for Zape. The
Windows equivalent (a code-signing certificate for the installer) runs
200–400 USD a year and can wait — SmartScreen on an installer is survivable
in a way that a permission prompt on an unsigned audio app is not.

---

## 7. Order of work

Each phase ends with something that runs. No phase is allowed to end with
"the foundation is in place".

**Phase 1 — the meter is right.**
Neta the plugin, measuring correctly, drawn with what already exists. All of
room 1's P0, verified against Tech 3341. No visuals, no standalone. This is
the phase that earns the product's name.

**Phase 2 — the renderer.**
wgpu in `seco-clap` behind a GPU editor path. Port room 1's display to it.
Measure the CPU budget from section 2 and publish the number.

**Phase 3 — the scope.**
Room 2. Spectrum, spectrogram, scope, correlation. At this point Neta
replaces MiniMeters for a DAW user.

**Phase 4 — the standalone.**
Window, WASAPI on Windows, PipeWire on Linux, process tap on macOS. Plus
the plugin→app ring buffer. At this point Neta replaces MiniMeters
generally.

**Phase 5 — dalia.**
Link the crate natively, port the geometry rendering to wgpu, bring over the
presets, the mashup controller and the post-processing.

**Phase 6 — the 3D room.**
`.obj` loading, then `.glb`. Your logo, spinning, glitching, reacting.

Phases 1–3 are a product on their own. Phases 4–6 are what makes it worth
more than the meter.

---

## 8. Explicitly not doing

- **`.blend` import.** Export to `.obj` or `.glb`.
- **A DAW-side video export pipeline.** Screen capture of the visuals is
  enough; encoding video in a plugin is a different product.
- **Aggressive DRM.** See section 6.
- **Cross-compiling from one machine to three.** CI already borrows the
  other two.
- **Beating MiniMeters on breadth in version 1.** It has years of small
  meters. Neta wins on the three rooms being one purchase, not on having
  every meter ever shipped.
