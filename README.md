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
| `seco-clap` | The CLAP adapter: hand-written `#[repr(C)]` mirrors of the CLAP 1.2.10 headers (pinned at commit `195b42a`), entry point, factory, params/state/audio-ports/gui extensions. | The only crate with `unsafe` in the shipped binary; every block carries a `SAFETY:` comment citing the header line it relies on |
| `seco-dsp` | Allocation-free DSP utilities: slew limiter, one-pole smoother, curve lookup tables, the ducking shapes. Pure functions, unit-tested without a host. | `#![forbid(unsafe_code)]` |
| `plugins/zape` | The plugin: multiplies audio by a gain curve indexed by the host's beat position, plus its editor page. No sidechain input, no signal analysis. | none |
| `xtask` | Build tasks (`cargo xtask bundle zape --release --install`): builds the crate and assembles the platform artifact. Not shipped, not linked into any plugin. | `dlopen`/`dlsym` only, to read the built plugin's own descriptor |

Build and install: [docs/building.md](docs/building.md). CLAP header findings
with citations and the empirical transport results: [docs/clap-notes.md](docs/clap-notes.md).

A plugin is `impl Plugin` plus `seco_export!(T)`; everything else — the entry
symbol, the bundle, the `Info.plist` — is the framework's job. The bundle
metadata is read back out of the compiled plugin (`clap_entry` → factory →
descriptor), so `impl Plugin` is the only place a plugin's identifier, name
and version are written.

## Zape

Tempo-synced ducking ("ghost kick"): `gain = curve(phase)` where `phase` is
the host's beat position folded into a cycle. The editor draws the audio behind
the curve — dry as a ghost, ducked on top — bucketed by phase, so the
picture is triggered on the beat rather than scrolling past. Four parameters: **Rate**
(1/1 … 1/16), **Mix**, **Curve** (15 shapes: single dips of varying
recovery, gated holds, and 2/3/4-per-cycle patterns), **Bypass**. Fifteen shapes ship and the sixteenth is drawn in the editor.
Every shape puts its floor exactly on the beat and closes the cycle seam at the
floor, by construction — see the click saga below for why that sentence is
the whole design. Edge
cases are handled from evidence, not guesses: hosts freeze the beat position
when stopped (Zape keeps free-running), loop wraps jump it backward without
interpolation (Zape resyncs hard and ramps the gain), and hosts without a
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

### Plugin state that does not fit in a parameter

A drawn curve is 257 floats; parameters are 32 numbers. The obvious API —
"ask the plugin for its bytes in `clap.state.save`" — is unsound for the
same reason `get_value` was: both state callbacks are `[main-thread]`
(ext/state.h:25-33) and may run while the audio thread is inside
`process()`, so materializing `&mut P` there is two live `&mut` over one
instance.

So the adapter owns the block, exactly as it already owns parameter values.
The main thread reads and writes it (session load, and the editor); the
audio thread receives it inside `process()` through `Plugin::apply_state`.
Nothing is ever read back out of a live plugin at an unsafe moment.

The hand-off is a triple buffer: three preallocated slots and one atomic
index, where `back`, `front` and `shared` always hold a permutation of
`{0,1,2}`. Each side moves an index by swapping it *through* `shared`, so
no interleaving can put both on the same slot, the audio thread never
waits, and nothing allocates.

Both swaps are `AcqRel`, and miri is why. With a plain `Release` on the
producer the handshake only models one direction — publishing writes to the
consumer — and misses the other: the index that comes *back* out of the
swap is a slot the consumer was reading, which the next `publish` writes
into. miri called it immediately: *"Data race detected between (1)
non-atomic read on thread ... and (2) retag write of type
`plugin_state::Chunk`"*. `publish_races_take_without_ub` runs the crossing
under the race detector and checks every delivered block is one that was
actually written, never a torn mix of two.

State format v2 appends the block after the parameter entries. Version 1
blobs still load, and an absent block is published as an *empty* one rather
than left alone: a preset saved before a plugin grew a curve must clear
that curve, not inherit whatever was drawn last
(`a_version_1_state_clears_the_plugin_block`).

The editor writes blocks through the same slot. It needs no queue of its
own — WebKit delivers its messages on the main thread, which is exactly
where the block may be written — so a `state <payload>` message goes
straight in and the triple buffer carries it to the audio thread at the
next `process()`. The payload is opaque: the framework stores the bytes a
page sent and hands back exactly those, and the plugin decides what they
mean.

Two details that are only obvious once they bite. A block that does not fit
is dropped, never truncated — half a saved curve is worse than no curve.
And the host is told with `clap_host_state.mark_dirty`: a parameter change
is implicitly dirty, a state block is not (ext/state.h:37-38), so without
that call the DAW would let the user close the session and lose the edit.
The wire protocol lives in `instance::gui_queue` rather than beside the
WebKit plumbing, so it is tested on every platform and not only where the
editor compiles.

Zape's drawn curve is what all of that carries: sixteen control points as
comma-separated text (`custom.rs`), which survive a session file, parse on
the audio thread without allocating, and are what the editor actually drags.
A sampled 257-point table would be none of those things.

Its seam is enforced, not requested. The first point is the beat and the
beat is silent, so the editor draws that handle hollow and refuses to move
it — and `parse` pins it to zero anyway, because a state block is just bytes
that may come from an older build or a hand-edited project file. A curve
starting at 0.8 would step the gain at every cycle boundary; the slew
limiter would ramp it rather than click, but the duck would not be a duck.
Everything unreadable — junk, wrong length, invalid UTF-8 — reads as the
default curve: a session that cannot be understood must still play.

The page samples the drawn curve with the plugin's own interpolation *and*
its dip-entry fade, so the line under the handles is the gain the audio
gets, not an illustration of it.

### The curve library, and where presets are allowed to live

Drawn curves save to disk — one small file each, under
`~/Library/Application Support/SECO/Zape` (macOS), `$XDG_CONFIG_HOME` or
`~/.config/seco/zape` (Linux), `%APPDATA%\SECO\Zape` (Windows), overridable
with `SECO_ZAPE_DIR`. One root for everything Zape owns: pointing an
override at the curve folder alone left the skin preference being written to
*its parent*, which during tests was the system temp directory.

One rule comes before the feature: **the curve in use lives in the session,
not in the library.** `clap.state` already carries it, so a project sounds
the same on a machine that has never seen the directory. The library is a
drawer to pull shapes out of, never the source of truth for how a track
sounds — a plugin whose presets live only on disk is a plugin that sounds
different when you send the project to someone else.

The plumbing is a third editor channel, `Plugin::editor_message`: the page
sends `msg <request>`, the plugin answers with JavaScript that the adapter
evaluates back into it. Main thread, so it may allocate and do I/O; no
`self`, because the plugin instance belongs to the audio thread. It
deliberately cannot change the plugin's state — `load` is not a request at
all. The answer to `list` carries every curve's points, so the page draws
the shapes *and* loads one by sending it back as a state block, keeping
every state change on the single path that also marks the session dirty.

Writing to a user's disk is the one thing here that leaves the process, so
the name typed into the webview goes through an allowlist rather than a
blocklist: letters, digits, space, dash and underscore survive, everything
else becomes a dash. That takes `../../.bashrc`, an embedded NUL, a Windows
reserved name and a 300-character title out of play in one rule, and the
tests check that a traversing save lands inside the directory and a
traversing delete leaves a bystander file alone. Saves go to a temporary
file and are renamed, which is atomic on all three platforms: a crash
mid-write leaves the previous curve rather than half of a new one.

### Skins

Five of them, chosen from the editor and remembered on disk beside the curve
library — a preference, so it follows the person rather than the project and
never dirties a session.

They are CSS and nothing else. Every colour the canvas drawing uses is a
custom property the stylesheet sets, read back with `getComputedStyle`, so
adding a skin is a block of variables and no JavaScript. The one thing that
is not automatic: tile thumbnails are *painted*, not styled, so switching
skins repaints them rather than waiting for the next parameter push to do
it a tick later.

The skin id is validated against the shipped list before it is written or
sent back — it ends up in a stylesheet selector and in a filename, and it
arrives from a webview.

They are palettes and attitude, taken from things worth ripping off: a
burned-DVD cover from a Mexico State tianguis, a bruised-purple cartoon
farmhouse, candy pinks over hard black ink, and a diner in yellow and
orange. No marks, no badges, no characters, no team crests; the names are
ours.

The treatment on top of those palettes is one direction, applied to all
five: bootleg mix-cover maximalism — a chrome plaque behind the header with
a flare and a lightning bolt, speed streaks and blooms in the background,
scanlines on the screen, a crooked sticker on the glass, rivets in the
corners, and a glow on whatever is switched on. Every one of those is a
variable away from being a different skin's, so the palettes stayed put
while the style moved. The plain grey one keeps the same treatment dialled
almost to nothing: there is still a skin for people who want a plugin
rather than a poster.

The rule the decoration obeys: nothing decorative takes a pointer event,
and nothing decorative sits between the eye and a measurement. The
scanlines run at the alpha where you feel them and cannot read them, and
the sticker sits in the top-left corner of the display, which is the one
region a duck curve never occupies.

### 3D in the editor, and what it costs

The default skin's mascot is a real mesh: a lathed bottle in raw WebGL,
spinning, leaning into the beat. No library — the plugin is one HTML string
with no way to fetch anything — so the renderer, the mesh and the texture
are all generated in the page. A bottle is a surface of revolution, which is
both how the shape is described (fourteen radial segments over a profile of
fourteen points) and why describing it costs nothing.

What it costs is worth being precise about, because "3D in a plugin" usually
means "a plugin that drops out". It is GPU work on the *main* thread, capped
at 30 Hz, and it stops existing when the editor closes. It cannot touch
audio: the page's only line to the audio thread is the scope array, which is
relaxed atomic stores in one direction. There is nothing to contend for, no
lock to wait on and no allocation on the path. The audio callback does not
know the renderer exists.

It can still be turned off — a machine is a machine — and that choice is
remembered like the skin. If the WebGL context fails to come up (an old
machine, a remote session, a host with an unusual sandbox) the flat drawing
stays rather than leaving a hole.

On model files: `.obj` is a text format and its parser is short, `.glb` is
the right long-term answer, and `.blend` is Blender's internal memory dump —
versioned, undocumented in practice, and read by essentially nothing but
Blender. Export from it, don't parse it.

The default skin also rebuilds the grammar of a 2008 fan page:
a bevelled plaque behind the header, a gloss highlight over the top half of
every control, an inset screen with a diagonal reflection, chrome lettering,
and a caguama — a real one, spun in WebGL, with the flat drawing kept as
its fallback. Decoration is markup a skin shows or hides, so it costs the
other four nothing and the binary is still the whole plugin: no image files,
no fetches, no asset directory to lose.

Two things that only work if you know why: the reflection over the display
is `pointer-events: none`, or it would eat the drag that draws the curve;
and the wordmark's shadow is a `filter`, not a `text-shadow`, because the
letters are a clipped gradient and a text-shadow paints straight through
them.

### The picture of the audio (a deliberately weak channel)

The editor draws the incoming signal behind the curve, which means the
audio thread has to tell the main thread something 30 times a second. The
state block's triple buffer would work and is the wrong tool: it guarantees
that a reader sees one coherent snapshot, and a waveform does not need
that.

`RtContext::set_scope` is therefore a plain relaxed store into a fixed
array of atomics — nothing blocks, nothing allocates, nothing is
coordinated. A reader can see bucket 3 from this block next to bucket 4
from the previous one, and for a picture that is invisible. The doc comment
says so out loud, because the same shortcut applied to state a decision
depends on would be a bug rather than a trade.

Zape buckets peaks by *phase*, so the display is beat-aligned rather than
scrolling — a scope triggered on the beat, drawn on the same axis as the
curve above it. It publishes two of them, the signal in and the signal out:
the first version drew only the input, which made the one thing the plugin
does invisible. The duck is the gap between the two bands, and
`the_scope_pictures_both_signals_against_the_beat` asserts that gap rather
than just that something was drawn. Two details that were wrong first: the release has to fire
once per pass rather than once per sample (per sample, a bucket ends up
holding the last few samples instead of the loudest of the pass — a thin
wobble where the envelope should be), and reconstructing the block's start
phase by subtracting leaves a negative epsilon that `rem_euclid` wraps to
~0.99999, lighting the last bucket on every block. Both are pinned by test.

The push is split accordingly: `editor_script` is deduplicated for things
that change rarely (the shapes, the drawn curve), `editor_frame` is
evaluated every refresh for the scope, because comparing a snippet that
always differs would cost more than sending it.

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

### The click saga (four bugs, four fail-first tests)

All four were found by ear in a DAW, then reproduced as failing tests before
fixing — the numbers below are from those red runs:

1. **The seam.** The first shapes ended the cycle at gain 1.0 and began it
   at 0.0: a full-scale step at every phase wrap, a click per cycle. The
   test failed with `|f(0) − f(1)| = 1`.
2. **The start-up overshoot.** The gain state initialized at 1.0 while the
   shape at the starting phase rarely is: the first ~10 ms played loud.
   `first_sample_starts_on_the_curve_not_at_unity` failed at `0.9906` vs the
   expected `0.0975`; the gain now snaps onto the shape's actual
   first-sample target — on fresh streams only.
3. **`reset()` mid-stream.** Hosts flush FX around transport changes; the
   session log showed REAPER calling `reset()` on *every* play start
   (`rst=25` in one session). Re-arming the start snap there turned a
   transport jump into a one-sample gain step of `0.424` — a click.
   `reset_plus_jump_glides_instead_of_clicking` pinned it; `reset()` now
   keeps the current gain and lets the ramp absorb the jump.
4. **The fix for (1) caused a tick of its own.** Closing the seam at gain
   1.0 put *unity on the beat*: the transient the duck exists to make room
   for passed at full level and was cut a moment later — ~5 ms above 0.9
   gain, then a 25 ms slam, at 95 BPM / rate 1/2. Worse, the entry was drawn
   in phase, so its real duration scaled with tempo and rate: 25 ms at 1/2,
   ~3 ms at 1/16, and the fast end clicked outright.

The fourth one is why a shape is no longer a sampled cycle. It is now a
*recovery* plus the phases where dips happen (`DuckShape`), with the entry
into each dip applied at play time as a fade of a fixed duration in
**seconds**. That inverts the invariants:

- the floor lands exactly on the beat — the seam closes at 0.0 on both
  sides of every dip, structurally, with nothing to declick
  (`every_shape_is_down_on_the_beat`, `shapes_close_the_seam_at_the_floor`);
- the entry takes the same 5 ms at any tempo and rate
  (`the_fade_holds_at_any_tempo_and_rate`, run at 1/16 @ 200 BPM / 44.1 kHz
  through 1/1 @ 60 BPM / 96 kHz).

Following that with a one-pole smoother then made the duck land *late*: a
smoother lags everything by about its time constant, so the gain was still
at `0.33` on the beat. It was replaced by a slew-rate limiter, which is
transparent while the shape moves within the limit — the curve arrives on
time — and only intervenes on what actually clicks: parameter flips,
transport jumps, preset changes.

The result is a bound rather than a measurement. The offline scan
(`click_scan_diagnostic`) reports the same worst per-sample step,
`0.00655`, for *every* scenario — steady playback at any rate, every
parameter flip, transport jumps, `reset()` — because that number is the
limiter's ceiling, not luck.

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
- Buffer safety assumes hosts don't hand *partially* overlapping in/out
  buffers (exact in-place aliasing is handled; partial overlap is a host
  contract violation, documented at the `SAFETY:` site).
