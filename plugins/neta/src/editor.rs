//! Neta's WebKit compatibility editor.
//!
//! WGPU is Neta's native renderer architecture. This page is deliberately a
//! small compatibility skin for hosts using SECO's current macOS WebKit GUI;
//! it consumes the same bounded analysis vocabulary and never speaks to DSP
//! state directly.
//!
//! # Shape
//!
//! One horizontal MiniMeters-style rail: every module stays side by side.
//! Spectrum and time read left to right; Loudness and VU read level bottom
//! to top. Dividers let the user give room to whichever instrument matters
//! now. The optional VISUALS experiment stays out of the default metering
//! path.
//!
//! Visibility, order, widths, and model path live in the plugin state block;
//! see `settings`.
//!
//! Everything it draws comes from one fixed 896-float picture. The UI does
//! not infer, allocate, or resample the audio-thread data: the slot map below
//! is the contract shared with `lib.rs`.

use base64::{STANDARD, encode_config_slice};

use seco_core::EditorPage;

pub(crate) const PAGE: EditorPage = EditorPage {
    html: HTML,
    width: 1_440,
    height: 440,
    // Nine instruments plus dividers still need readable labels and data.
    minimum: Some((960, 240)),
};

/// Fixed native-to-page payload. Slots stay in little-endian f32 Base64 on
/// the WebKit bridge, then land in one reusable `Float32Array(896)`.
pub(crate) const SCOPE_SLOTS: usize = 896;
const METRICS: usize = 0;
const METRICS_END: usize = 32;
const TIMELINE: usize = 32;
const TIMELINE_POINTS: usize = 64;
const TIMELINE_MOMENTARY: usize = TIMELINE;
const TIMELINE_SHORT: usize = TIMELINE_MOMENTARY + TIMELINE_POINTS;
const TIMELINE_INTEGRATED: usize = TIMELINE_SHORT + TIMELINE_POINTS;
const HISTOGRAM: usize = TIMELINE_INTEGRATED + TIMELINE_POINTS;
const HISTOGRAM_POINTS: usize = 64;
const WAVE_MIN: usize = HISTOGRAM + HISTOGRAM_POINTS;
const WAVE_MAX: usize = WAVE_MIN + 64;
const WAVE_POINTS: usize = 64;
const OSCILLOSCOPE: usize = WAVE_MAX + WAVE_POINTS;
const OSCILLOSCOPE_POINTS: usize = 128;
const SPECTRUM: usize = OSCILLOSCOPE + OSCILLOSCOPE_POINTS * 2;
const SPECTRUM_POINTS: usize = 96;
const GONIOMETER: usize = SPECTRUM + SPECTRUM_POINTS;
const GONIOMETER_POINTS: usize = 64;
const SCOPE_BYTES: usize = SCOPE_SLOTS * size_of::<f32>();
const SCOPE_BASE64_BYTES: usize = SCOPE_BYTES.div_ceil(3) * 4;

const _: () = {
    assert!(METRICS == 0 && METRICS_END == TIMELINE);
    assert!(TIMELINE_INTEGRATED + TIMELINE_POINTS == HISTOGRAM);
    assert!(HISTOGRAM + HISTOGRAM_POINTS == WAVE_MIN);
    assert!(WAVE_MIN + WAVE_POINTS == WAVE_MAX);
    assert!(OSCILLOSCOPE + OSCILLOSCOPE_POINTS * 2 == SPECTRUM);
    assert!(SPECTRUM + SPECTRUM_POINTS == GONIOMETER);
    assert!(GONIOMETER + GONIOMETER_POINTS * 2 == SCOPE_SLOTS);
};

/// Serializes one exact frame without decimal formatting or JSON churn.
///
/// The base64 text is safe in a JavaScript string literal. WebKit decodes it
/// into a preallocated byte buffer and reads little-endian floats directly.
pub(crate) fn frame(scope: &[f32]) -> Option<String> {
    if scope.len() < SCOPE_SLOTS {
        return None;
    }
    let mut bytes = [0_u8; SCOPE_BYTES];
    for (index, value) in scope[..SCOPE_SLOTS].iter().enumerate() {
        let start = index * size_of::<f32>();
        bytes[start..start + size_of::<f32>()].copy_from_slice(&value.to_le_bytes());
    }
    let mut encoded = [0_u8; SCOPE_BASE64_BYTES];
    let length = encode_config_slice(bytes, STANDARD, &mut encoded);
    let payload = core::str::from_utf8(&encoded[..length]).ok()?;
    let mut script = String::with_capacity(payload.len() + 48);
    script.push_str("window.__neta_frame&&window.__neta_frame(\"");
    script.push_str(payload);
    script.push_str("\");");
    Some(script)
}

const HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<style>
  :root {
    /* Midnight Kids: flat violet surfaces, hot pink labels, cyan data.
       Data may glow; chrome never pretends to be a texture. */
    --void:#171320; --panel:#211a2e; --well:#0f0c16;
    --line:#33273f; --edge:#463656;
    --pink:#ff3d8a; --pink-dim:#a2537f; --pink-faint:#5e3a52;
    --cyan:#5ddce9; --cyan-dim:#3a8a94;
    --amber:#ffc247; --red:#ff4d5e;
  }
  :root[data-theme="coal"] {
    --void:#16181b; --panel:#22262a; --well:#101214; --line:#394047; --edge:#515b64;
    --pink:#e78ab4; --pink-dim:#aa718b; --pink-faint:#65515d; --cyan:#82d8d0; --cyan-dim:#538f89;
    --amber:#d7b76d; --red:#e67575;
  }
  :root[data-theme="mono"] {
    --void:#171717; --panel:#242424; --well:#0d0d0d; --line:#444; --edge:#666;
    --pink:#d6d6d6; --pink-dim:#9a9a9a; --pink-faint:#666; --cyan:#f0f0f0; --cyan-dim:#aaa;
    --amber:#d5d5d5; --red:#fff;
  }
  * { box-sizing:border-box; }
  html,body { margin:0; width:100%; height:100%; background:var(--void); color:var(--pink); }
  body {
    font:12px/1.35 ui-monospace,"SF Mono",Menlo,Consolas,monospace;
    -webkit-font-smoothing:antialiased;
    user-select:none; -webkit-user-select:none;
    overflow:hidden;
  }
  button { font:inherit; color:inherit; background:none; border:0; padding:0; cursor:pointer; }
  :focus-visible { outline:1px solid var(--cyan); outline-offset:2px; }

  .shell {
    height:100%; min-width:0; min-height:0; padding:10px; display:grid; gap:8px;
    grid-template-rows:auto minmax(0,1fr) auto;
    background:var(--void);
  }
  /* The rail keeps a floor while the drawer is open, so opening settings
     shrinks the meters instead of squeezing them out of existence. */
  .shell.tuning { grid-template-rows:auto minmax(74px,1fr) minmax(0,auto) auto; }

  /* ---- header ---- */
  .top { display:flex; align-items:center; gap:12px; height:26px; min-width:0; }
  .brand { font-size:19px; font-weight:700; letter-spacing:.34em; color:var(--pink); }
  .brand i { font-style:normal; color:var(--cyan); }
  .rule { flex:1; height:1px; background:var(--line); }
  .headline { font-size:11px; letter-spacing:.1em; color:var(--pink-dim); white-space:nowrap; }
  .headline b { color:var(--cyan); font-weight:400; }
  .gear {
    padding:3px 10px; font-size:10px; letter-spacing:.12em; text-transform:uppercase;
    color:var(--pink); border:1px solid var(--line); background:var(--panel);
  }
  .gear:hover, .gear[aria-expanded="true"] { background:var(--cyan); color:var(--void); border-color:var(--cyan); }
  .actions { display:flex; align-items:center; gap:4px; }
  .action { padding:3px 7px; border:1px solid var(--line); background:var(--panel); font-size:9px;
            letter-spacing:.08em; text-transform:uppercase; }
  .action:hover { border-color:var(--cyan); color:var(--cyan); }

  /* ---- resizable instrument rail ------------------------------------- */
  .row { display:flex; min-width:0; min-height:0; overflow:hidden; }
  .box {
    flex:1 1 0; min-width:74px; min-height:0; position:relative; overflow:hidden;
    border:1px solid var(--line); background:var(--panel);
    display:grid; grid-template-rows:auto minmax(0,1fr) auto;
  }
  .box[hidden] { display:none; }
  .cap {
    padding:5px 7px; border-bottom:1px solid var(--line);
    font-size:9px; letter-spacing:.14em; text-transform:uppercase; color:var(--pink-dim);
    display:flex; justify-content:space-between; gap:5px; white-space:nowrap; overflow:hidden;
  }
  .cap b { color:var(--pink); font-weight:400; }
  /* `flex:1` so the subtitle is what gives way when the cross needs room, and
     right-aligned so the header still reads title-left, detail-right. */
  .cap span { flex:1; min-width:0; text-align:right; color:var(--pink-faint);
              overflow:hidden; text-overflow:ellipsis; }
  /* The header is the panel's handle. Reordering and hiding are the two things
     people do to a rail constantly, and walking to a settings tab for either is
     a trip a meter should not ask for. */
  .cap { cursor:grab; touch-action:none; }
  .cap.dragging { cursor:grabbing; background:var(--well); color:var(--cyan); }
  .box.carrying { opacity:.5; }
  /* Insertion mark, drawn on the panel the drop would land in front of. A live
     reorder is not possible here: `layout` empties the rail to rebuild it,
     which would release the pointer capture mid-drag. */
  .box[data-drop="before"] { box-shadow:inset 3px 0 0 var(--cyan); }
  .box[data-drop="after"] { box-shadow:inset -3px 0 0 var(--cyan); }
  .capHide {
    flex:0 0 auto; width:15px; height:14px; margin:-2px -3px -2px 0;
    display:grid; place-items:center; font-size:13px; line-height:1;
    color:var(--pink-faint); border:1px solid transparent;
  }
  .capHide:hover { color:var(--red); border-color:var(--line); }
  .capHide:disabled { opacity:.25; cursor:default; }
  .note { padding:4px 7px; border-top:1px solid var(--line); font-size:9px;
          letter-spacing:.06em; color:var(--pink-faint); min-height:20px;
          overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
  canvas { display:block; width:100%; height:100%; min-height:0; background:var(--well); }

  .box[data-module="object"] canvas { cursor:grab; touch-action:none; }
  .box[data-module="object"] canvas.dragging { cursor:grabbing; }

  /* ---- bare chrome ------------------------------------------------------
     A rail of bordered cards reads as eight plugins that happen to be adjacent.
     Stripping the borders, the panel fill and the caption bars leaves one
     continuous field, which is what makes a meter bridge read as one
     instrument. Every panel already draws its own axes into its own canvas, so
     nothing was being carried by the furniture except the module's name - and
     that comes back under the pointer, where it is needed and nowhere else. */
  .shell[data-chrome="bare"] .row {
    border:1px solid var(--line); background:var(--well);
  }
  .shell[data-chrome="bare"] .box {
    border:0; background:transparent;
    grid-template-rows:minmax(0,1fr);
  }
  /* Out of flow, so the canvas gets the whole panel rather than the leftovers.
     `pointer-events` follows the reveal: a transparent caption sitting over the
     top of every canvas would eat the hover readout and the object's orbit. */
  .shell[data-chrome="bare"] .cap,
  .shell[data-chrome="bare"] .note {
    position:absolute; left:0; right:0; z-index:2;
    border:0; opacity:0; pointer-events:none; transition:opacity .12s;
  }
  .shell[data-chrome="bare"] .cap {
    top:0; background:linear-gradient(var(--void) 60%,rgba(0,0,0,0));
    padding-bottom:9px;
  }
  .shell[data-chrome="bare"] .note {
    bottom:0; background:linear-gradient(rgba(0,0,0,0),var(--void) 40%);
    padding-top:9px;
  }
  .shell[data-chrome="bare"] .box:hover .cap,
  .shell[data-chrome="bare"] .box:hover .note,
  .shell[data-chrome="bare"] .box:focus-within .cap,
  .shell[data-chrome="bare"] .cap.dragging {
    opacity:1; pointer-events:auto;
  }
  /* Hairline separators, so the panels almost touch. */
  .shell[data-chrome="bare"] .grip { flex:0 0 5px; }
  .shell[data-chrome="bare"] .grip::after { inset:0 2px; background:var(--void); }
  .shell[data-chrome="bare"] .foot { border-color:var(--line); background:var(--well); }
  .shell[data-chrome="bare"] .cell { border-right-color:var(--line); }

  /* Bigger hit target than its visible one-pixel divider. */
  .grip {
    flex:0 0 8px; cursor:col-resize; position:relative; touch-action:none;
    background:transparent;
  }
  .grip::after {
    content:""; position:absolute; inset:14px 3px; background:var(--line);
  }
  .grip:hover::after, .grip.dragging::after { background:var(--cyan); }

  /* ---- footer ---- */
  .foot { display:grid; grid-auto-flow:column; grid-auto-columns:1fr;
          border:1px solid var(--line); background:var(--panel); }
  .cell { padding:5px 9px; border-right:1px solid var(--line); min-width:0; }
  .cell:last-child { border-right:none; }
  .cell label { display:block; font-size:8.5px; letter-spacing:.14em;
                text-transform:uppercase; color:var(--pink-faint); }
  .cell output { display:block; margin-top:1px; font-size:15px; color:var(--cyan);
                 font-variant-numeric:tabular-nums; }
  .cell output.pending { color:var(--pink-faint); font-size:10px; letter-spacing:.1em; }
  .cell output.hot { color:var(--red); }
  .cell output em { font-style:normal; font-size:9px; color:var(--pink-dim); margin-left:3px; }

  /* Pointer readout. Fixed, never inside a panel: a tooltip clipped by the
     box it belongs to is a tooltip you cannot read at the edges. */
  .tip {
    position:fixed; z-index:6; pointer-events:none;
    padding:4px 8px; font-size:10px; line-height:1.45; letter-spacing:.04em;
    white-space:pre; color:var(--cyan);
    background:var(--void); border:1px solid var(--edge);
  }
  .tip b { color:var(--pink); font-weight:400; }
  .tip[hidden] { display:none; }

  /* ---- settings drawer -------------------------------------------------
     A drawer inside the shell's own grid, not an overlay. Settings that cover
     the meters make you close them to check what the change did, and a full
     screen of stacked cards makes you fullscreen a plugin window to reach the
     last one. This slides in under the rail: the meters shrink, stay live, and
     the drawer's content runs sideways in as many columns as the width gives.
     Tabs are what keep it short - a group that would stack downwards becomes a
     tab instead. */
  .sheet {
    min-width:0; min-height:0;
    display:grid; grid-template-rows:auto minmax(0,1fr);
    border:1px solid var(--line); background:var(--panel);
    max-height:min(56vh,320px);
  }
  .sheet[hidden] { display:none; }

  .tabs {
    display:flex; flex-wrap:wrap; align-items:center; gap:4px;
    padding:5px 6px; border-bottom:1px solid var(--line); background:var(--void);
  }
  .tab {
    padding:3px 10px; font-size:10px; letter-spacing:.12em; text-transform:uppercase;
    color:var(--pink-dim); border:1px solid transparent;
  }
  .tab:hover { color:var(--cyan); }
  .tab[aria-selected="true"] {
    color:var(--void); background:var(--pink); border-color:var(--pink);
  }
  .tabSpace { flex:1; min-width:8px; }

  .pages { min-height:0; overflow:auto; overscroll-behavior:contain; padding:9px; }
  .page { display:none; }
  .page[data-active="true"] {
    display:grid; gap:9px; align-items:start;
    /* auto-fit is the whole point: one more column every time the window gets
       wider, instead of a fixed count that stacks when it does not fit. */
    grid-template-columns:repeat(auto-fit,minmax(196px,1fr));
  }
  .page .wide { grid-column:span 2; }
  @media (max-width:640px) { .page .wide { grid-column:span 1; } }

  .group { min-width:0; border:1px solid var(--line); background:var(--well); padding:8px 10px 10px; }
  .group h2 { margin:0 0 8px; font-size:9.5px; font-weight:400;
              letter-spacing:.18em; text-transform:uppercase; color:var(--pink-dim); }
  .chips { display:flex; flex-wrap:wrap; gap:5px; }
  .chip {
    padding:3px 9px; font-size:11px; letter-spacing:.04em;
    color:var(--pink); border:1px solid var(--line); background:var(--panel);
  }
  .chip[aria-pressed="true"] { background:var(--cyan); color:var(--void); border-color:var(--cyan); }
  .chip:hover { border-color:var(--cyan); }
  .field { display:flex; gap:6px; margin-top:2px; }
  .field input {
    flex:1; min-width:0; font:inherit; font-size:11px; padding:4px 7px;
    color:var(--cyan); background:var(--panel); border:1px solid var(--line);
    user-select:text; -webkit-user-select:text;
  }
  .field input:focus { outline:none; border-color:var(--cyan); }
  .go { padding:4px 12px; font-size:11px; color:var(--void); background:var(--pink); }
  .go:hover { background:var(--cyan); }
  .hint { margin:7px 0 0; font-size:10px; line-height:1.45; color:var(--pink-faint); }
  .hint b { color:var(--pink-dim); font-weight:400; }
  .controlLabel { margin:7px 0 4px; font-size:9px; letter-spacing:.1em; text-transform:uppercase; color:var(--pink-faint); }
  .controlLabel:first-child { margin-top:0; }
  .tune { display:grid; grid-template-columns:1fr auto; gap:5px 8px; align-items:center; margin-top:7px; }
  .tune label { color:var(--pink-dim); font-size:10px; }
  .tune input { width:82px; accent-color:var(--cyan); }
  .reset { padding:3px 10px; font-size:10px; letter-spacing:.1em; text-transform:uppercase;
           color:var(--pink); border:1px solid var(--line); }
  .reset:hover { border-color:var(--cyan); color:var(--cyan); }
  .close { padding:3px 12px; font-size:10px; letter-spacing:.1em; text-transform:uppercase;
           color:var(--void); background:var(--cyan); }

  @media (max-height:560px) {
    .sheet { max-height:min(62vh,260px); }
    .pages { padding:7px; }
    .group { padding:7px 8px 8px; }
    .group h2 { margin-bottom:6px; }
    .controlLabel { margin:5px 0 3px; }
    .hint { margin-top:5px; }
  }
  /* Below this there is no height left to share. Giving the drawer the rail's
     space beats clipping both, and the readouts in the footer stay put. */
  @media (max-height:400px) {
    .shell.tuning .row { display:none; }
    .sheet { max-height:none; }
  }

  @media (max-width:960px) {
    .shell { padding:8px; gap:6px; }
    .top { gap:8px; }
    .headline { min-width:0; overflow:hidden; text-overflow:ellipsis; }
    .cap { padding:5px 6px; letter-spacing:.08em; }
    .cell { padding:5px 4px; overflow:hidden; }
    .cell label { font-size:7px; letter-spacing:.04em; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
    .cell output { font-size:12px; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
    .cell output em { display:none; }
  }
  @media (max-height:360px) {
    .shell { padding:6px; gap:5px; }
    .top { height:22px; }
    .brand { font-size:14px; }
    .headline { font-size:8px; }
    .cap { padding:4px 5px; font-size:8px; }
    .foot .cell { padding:3px 4px; }
    .foot .cell label { font-size:7px; }
    .foot .cell output { font-size:11px; }
  }
</style>
</head>
<body>
<main class="shell">
  <header class="top">
    <div class="brand">NE<i>TA</i></div>
    <div class="rule"></div>
    <div class="headline">integrated <b id="hint">listening</b></div>
    <div class="actions" aria-label="Loudness actions">
      <button class="action" id="actionReset">reset</button>
      <button class="action" id="actionCaptureA">A</button>
      <button class="action" id="actionCaptureB">B</button>
    </div>
    <button class="gear" id="gear" aria-expanded="false" aria-controls="sheet">settings</button>
  </header>

  <div class="row" id="row">
    <section class="box" data-module="loudness"><div class="cap"><b>loudness</b><span>LUFS</span></div><canvas id="loudness"></canvas></section>
    <section class="box" data-module="vu"><div class="cap"><b>VU</b><span id="vuMode">calibrated</span></div><canvas id="vu"></canvas></section>
    <section class="box" data-module="spectrum"><div class="cap"><b>spectrum</b><span>20&#8202;Hz&#8202;–&#8202;20&#8202;k</span></div><canvas id="spectrum"></canvas></section>
    <section class="box" data-module="spectrogram"><div class="cap"><b>spectrogram</b><span>history</span></div><canvas id="spectrogram"></canvas></section>
    <section class="box" data-module="waveform"><div class="cap"><b>waveform</b><span>envelope</span></div><canvas id="waveform"></canvas></section>
    <section class="box" data-module="oscilloscope"><div class="cap"><b>oscilloscope</b><span id="oscMode">pitch · multi</span></div><canvas id="oscilloscope"></canvas></section>
    <section class="box" data-module="stereo"><div class="cap"><b>stereo</b><span>L / R</span></div><canvas id="stereo"></canvas></section>
    <section class="box" data-module="object">
      <div class="cap"><b>object</b><span id="objectPoints">drag to orbit</span></div>
      <canvas id="object"></canvas>
      <div class="note" id="objectNote">drag to orbit · scroll to zoom</div>
    </section>
    <section class="box" data-module="visuals">
      <div class="cap"><b>visuals</b><span>experimental</span></div>
      <canvas id="visuals"></canvas>
      <div class="note">isolated from metering · opt in from settings</div>
    </section>
  </div>

  <section class="sheet" id="sheet" hidden role="group" aria-label="Settings">
    <div class="tabs" id="tabs" role="tablist" aria-label="Settings sections">
      <span class="tabSpace"></span>
      <button class="reset" id="sheetReset">reset layout</button>
      <button class="close" id="sheetClose">close</button>
    </div>
    <div class="pages" id="pages">

      <div class="page" data-tab="layout">
        <section class="group">
          <h2>Modules</h2>
          <div class="chips" id="moduleChips"></div>
          <p class="hint">Drag a panel's header to move it, its divider to
            resize it, or hit its <b>&#215;</b> to hide it. A hidden module stops
            drawing; measurement carries on either way.</p>
        </section>
        <section class="group">
          <h2>Chrome</h2>
          <div class="chips" id="chromeChips"></div>
          <p class="hint"><b>bare</b> drops the panel borders and shows a header
            only under the pointer, so the rail reads as one instrument.</p>
        </section>
      </div>

      <div class="page" data-tab="view">
        <section class="group">
          <h2>Palette</h2>
          <p class="controlLabel">theme</p><div class="chips" id="themeChips"></div>
          <p class="controlLabel">colour map</p><div class="chips" id="mapChips"></div>
        </section>
        <section class="group">
          <h2>Rendering</h2>
          <p class="controlLabel">refresh</p><div class="chips" id="fpsChips"></div>
          <p class="hint">Below your monitor's refresh rate costs less CPU and
            makes fast transients harder to see.</p>
        </section>
        <section class="group">
          <h2>Pointer</h2>
          <div class="chips" id="hoverChips"></div>
          <p class="hint">Hover any panel for the value under the cursor.</p>
        </section>
      </div>

      <div class="page" data-tab="analysis">
        <section class="group">
          <h2>Input</h2>
          <p class="controlLabel">source</p><div class="chips" id="routeChips"></div>
          <div class="tune">
            <label for="trimInput">trim dB</label><input id="trimInput" type="number" min="-24" max="24" step="1">
          </div>
          <p class="hint"><b>Input bus:</b> stereo insert. Source and trim affect analysis only; Neta never changes output audio.</p>
        </section>
        <section class="group">
          <h2>FFT</h2>
          <p class="controlLabel">size</p><div class="chips" id="fftChips"></div>
          <p class="controlLabel">scale</p><div class="chips" id="scaleChips"></div>
          <div class="tune">
            <label for="smoothingInput">smoothing</label><input id="smoothingInput" type="range" min="0" max="100" step="1">
            <label for="tiltInput">tilt dB</label><input id="tiltInput" type="number" min="-18" max="18" step="1">
          </div>
        </section>
        <section class="group">
          <h2>Spectrum</h2>
          <p class="controlLabel">style</p><div class="chips" id="styleChips"></div>
          <p class="controlLabel">peak trace</p><div class="chips" id="traceChips"></div>
          <p class="hint">Plotted over a 90&#8239;dB window. The trace holds what the section reaches.</p>
        </section>
      </div>

      <div class="page" data-tab="loudness">
        <section class="group">
          <h2>Delivery target</h2>
          <div class="chips" id="targetChips"></div>
          <p class="hint">Reference only. It moves the guide line; Neta never changes audio.</p>
        </section>
        <section class="group">
          <h2>Programme</h2>
          <p class="controlLabel">timeline, histogram, overs</p><div class="chips" id="loudnessChips"></div>
          <p class="controlLabel">actions</p><div class="chips" id="actionChips"></div>
          <p class="hint">Reset and A/B use automatable counter parameters. They survive host undo correctly.</p>
        </section>
        <section class="group">
          <h2>VU</h2>
          <p class="controlLabel">mode</p><div class="chips" id="vuChips"></div>
          <div class="tune">
            <label for="calibrationInput">calibration</label><input id="calibrationInput" type="number" min="-36" max="-6" step="1">
          </div>
        </section>
      </div>

      <div class="page" data-tab="scopes">
        <section class="group">
          <h2>Spectrogram</h2>
          <p class="controlLabel">history</p><div class="chips" id="spectrogramChips"></div>
          <div class="tune">
            <label for="speedInput">speed</label><input id="speedInput" type="number" min="1" max="4" step="1">
          </div>
          <p class="controlLabel">extras</p><div class="chips" id="historyChips"></div>
        </section>
        <section class="group">
          <h2>Waveform</h2>
          <p class="controlLabel">colour</p><div class="chips" id="waveformChips"></div>
          <p class="controlLabel">style</p><div class="chips" id="waveStyleChips"></div>
          <p class="controlLabel">gain</p><div class="chips" id="waveGainChips"></div>
        </section>
        <section class="group">
          <h2>Oscilloscope</h2>
          <p class="controlLabel">trigger</p><div class="chips" id="oscChips"></div>
          <p class="controlLabel">view</p><div class="chips" id="oscViewChips"></div>
          <p class="controlLabel">gain</p><div class="chips" id="oscGainChips"></div>
        </section>
      </div>

      <div class="page" data-tab="stereo">
        <section class="group">
          <h2>Stereometer</h2>
          <p class="controlLabel">style</p><div class="chips" id="stereoStyleChips"></div>
          <p class="controlLabel">colour</p><div class="chips" id="stereoColorChips"></div>
          <p class="controlLabel">point size</p><div class="chips" id="pointSizeChips"></div>
        </section>
        <section class="group wide">
          <h2>3D model</h2>
          <div class="field">
            <input id="modelPath" type="text" spellcheck="false" placeholder="/path/to/model.obj" aria-label="Path to an OBJ file">
            <button class="go" id="modelLoad">Load</button>
          </div>
          <p class="controlLabel">view</p><div class="chips" id="objectChips"></div>
          <p class="hint" id="modelStatus">Wavefront <b>.obj</b> up to 96&#8239;MB. Neta samples it down to a
            point cloud. Drag to orbit, <b>scroll or pinch to zoom</b>; automatic spin keeps running.</p>
        </section>
      </div>

      <div class="page" data-tab="presets">
        <section class="group">
          <h2>Built-ins</h2>
          <div class="chips" id="presetChips"></div>
          <p class="hint">A preset sets the whole view, including choices it does not name.</p>
        </section>
        <section class="group wide">
          <h2>Four user slots</h2>
          <div class="chips" id="slotChips"></div>
          <p class="hint">Tap Save N to store current view. Tap Load N to restore it.</p>
        </section>
      </div>

    </div>
  </section>

  <footer class="foot">
    <div class="cell"><label>integrated</label><output id="int">&#8212;</output></div>
    <div class="cell"><label>range</label><output id="lra">&#8212;</output></div>
    <div class="cell"><label>psr</label><output id="psr">&#8212;</output></div>
    <div class="cell"><label>plr</label><output id="plr">&#8212;</output></div>
    <div class="cell"><label>true peak</label><output id="tp">&#8212;</output></div>
    <div class="cell"><label>correlation</label><output id="corr">+0.00</output></div>
    <div class="cell"><label>width</label><output id="width">0.00</output></div>
  </footer>
</main>

<div class="tip" id="tip" hidden aria-hidden="true"></div>

<script>
(() => {
  "use strict";
  /* Native scope map — keep this verbatim with Neta::publish_picture:
     0..32 metrics; 32..224 three 64-point loudness timelines;
     224..288 histogram; 288..416 waveform min/max; 416..672 L/R scope
     pairs; 672..768 spectrum; 768..896 side/mid goniometer pairs. */
  const I={m:0,s:1,tp:2,corr:3,width:4,l:5,r:6,mid:7,side:8,psr:9,int:10,lra:11,plr:12,
           vuL:13,vuR:14,vuHoldL:15,vuHoldR:16,sampleL:17,sampleR:18,
           source:19,frames:20,fft:21,moduleMask:22,captureA:23,captureB:24,
           captureAShort:25,captureBShort:26,captureATp:27,captureBTp:28,target:29,
           vuRawL:30,vuRawR:31,timelineM:32,timelineS:96,timelineI:160,histogram:224,
           waveMin:288,waveMax:352,oscilloscope:416,spectrum:672,gonio:768};
  const SCOPE_SLOTS=896, SCOPE_BYTES=SCOPE_SLOTS*4;
  // The plugin sends this when a gated value has no programme behind it yet.
  const UNMEASURED=-150;
  const BANDS=96, WAVE_POINTS=64, OSCILLOSCOPE_POINTS=128, GONIO_POINTS=64;
  const TOP_DB=0, BOTTOM_DB=-60, TP_CEILING=-1;
  const TARGETS=[-24,-23,-16,-14,-9], DEFAULT_TARGET=-14;
  // Legacy indices 0..6 never move. VU and oscilloscope append at 7/8.
  const MODULES=["loudness","spectrum","spectrogram","waveform","stereo","object","visuals","vu","oscilloscope"];

  const $=id=>document.getElementById(id);
  const clamp=(x,a,b)=>Math.max(a,Math.min(b,x));
  const real=x=>Number.isFinite(x)&&x>UNMEASURED;
  const norm=db=>clamp((db-BOTTOM_DB)/(TOP_DB-BOTTOM_DB),0,1);
  const post=m=>{ try{ window.webkit.messageHandlers.seco.postMessage(m); }catch(_){} };

  const DEFAULT_ORDER=[0,7,1,2,3,8,4,5,6], DEFAULT_WIDTHS=[150,120,220,180,180,180,160,120,170];
  const MIN_WIDTH=52, MAX_WIDTH=2000;

  // Only these two typed buffers live for the page lifetime. `atob()` gives
  // a transient binary string, then its little-endian bytes land here.
  const v=new Float32Array(SCOPE_SLOTS);
  const base64Bytes=new Uint8Array(SCOPE_BYTES);
  const base64View=new DataView(base64Bytes.buffer);
  function decodeBase64Frame(encoded){
    let binary;
    try{ binary=atob(encoded); }catch(_){ return false; }
    if(binary.length!==SCOPE_BYTES) return false;
    for(let index=0;index<SCOPE_BYTES;index++) base64Bytes[index]=binary.charCodeAt(index);
    for(let index=0;index<SCOPE_SLOTS;index++) v[index]=base64View.getFloat32(index*4,true);
    return true;
  }
  // VISUALS is deliberately opt-in. It is a separate creative experiment,
  // not evidence a mix needs while making a loudness decision.
  let enabled=MODULES.map(name=>name!=="visuals");
  let order=DEFAULT_ORDER.slice(), widths=DEFAULT_WIDTHS.slice(), modelPath="", targetLufs=DEFAULT_TARGET;
  let theme="midnight", colorMap="neta", renderFps="vsync", route="stereo", trimDb=0,
      fftSize=2048, spectrumScale="log", spectrumStyle="both", smoothing=90, tiltDb=0,
      spectrumHold=false, vuMode="vu", vuCalibration=-18, oscMode="pitch", oscCycles="multi";
  let loudnessTimeline=true, loudnessHistogram=true, loudnessOvers=true,
      spectrogramHistoryMode="fast", spectrogramSpeed=4, spectrogramLoop=true,
      spectrogramTimecode=false, stereoStyle="linear", stereoColor="static",
      pointSize="auto", waveformColor="pink", preset="default", slots=["","","",""];
  let waveformStyle="band", waveformGain=1, oscStyle="overlay", oscGain=1,
      spectrumTrace=true, hoverReadout=true, objectSpin=true, chrome="bare";
  const GAINS=[1,2,4];
  // Object camera zoom, matching settings.rs's tenths bounds. Past the ceiling
  // a point cloud stops being a car and becomes its points.
  const OBJECT_ZOOM_MIN=0.5, OBJECT_ZOOM_MAX=4;
  let objectZoom=1;
  let clock=0;

  // Resolved once instead of per draw: seven canvases times a dozen colour
  // lookups a frame is a forced style recalculation for no benefit.
  let ink={}, rgb={};
  // Same colours as `ink`, as bare "r,g,b" triples. Every gradient and every
  // translucent stroke needs them, and parsing a hex string per stroke is the
  // kind of per-frame work this page exists to avoid.
  const hexToRgb=hex=>{
    const match=/^#?([0-9a-f]{3}|[0-9a-f]{6})$/i.exec(hex.trim());
    if(!match) return "255,255,255";
    let digits=match[1];
    if(digits.length===3){
      digits=digits[0]+digits[0]+digits[1]+digits[1]+digits[2]+digits[2];
    }
    return parseInt(digits.slice(0,2),16)+","+parseInt(digits.slice(2,4),16)
          +","+parseInt(digits.slice(4,6),16);
  };
  const readInk=()=>{
    const style=getComputedStyle(document.documentElement);
    for(const name of ["void","panel","well","line","edge","pink","pink-dim","pink-faint",
                       "cyan","cyan-dim","amber","red"]){
      ink[name]=style.getPropertyValue("--"+name).trim();
      rgb[name]=hexToRgb(ink[name]);
    }
    gradients=new WeakMap();
  };

  let layoutEpoch=0;
  const fitCache=new WeakMap();
  // Gradients are pinned to pixel geometry, so a resize retires them with the
  // fit cache rather than leaving a map that grows once per drag frame.
  const invalidateCanvases=()=>{ layoutEpoch++; gradients=new WeakMap(); };
  function fit(canvas){
    let cached=fitCache.get(canvas);
    if(!cached||cached.epoch!==layoutEpoch){
      const dpr=Math.min(devicePixelRatio||1,2), rect=canvas.getBoundingClientRect();
      const w=Math.max(1,Math.round(rect.width*dpr)), h=Math.max(1,Math.round(rect.height*dpr));
      if(canvas.width!==w||canvas.height!==h){ canvas.width=w; canvas.height=h; }
      cached={ctx:canvas.getContext("2d"),w,h,dpr,epoch:layoutEpoch};
      fitCache.set(canvas,cached);
    }
    const {ctx,w,h,dpr}=cached;
    // Opaque fill already clears every pixel. clearRect doubles Retina work.
    ctx.fillStyle=ink.well; ctx.fillRect(0,0,w,h);
    return [ctx,w,h,dpr];
  }
  const mono=(ctx,px)=>{ ctx.font=px+'px ui-monospace,Menlo,monospace'; ctx.textBaseline='middle'; };

  /* ==== shared plot furniture =============================================
     Nine panels sharing one dark well have to share one grammar too: the same
     grid weight, the same fade under a curve, the same way a line glows. Nine
     panels each inventing their own is how a window ends up looking like nine
     plugins that happen to be adjacent. */

  // Gradients belong to the context that made them, so they are cached per
  // context and thrown away whenever the palette or the layout changes.
  let gradients=new WeakMap();
  function verticalFade(ctx,triple,top,bottom,strong,weak){
    let perContext=gradients.get(ctx);
    if(!perContext){ perContext=new Map(); gradients.set(ctx,perContext); }
    const key=triple+"|"+Math.round(top)+"|"+Math.round(bottom)+"|"+strong+"|"+weak;
    let fade=perContext.get(key);
    if(!fade){
      fade=ctx.createLinearGradient(0,top,0,bottom);
      fade.addColorStop(0,"rgba("+triple+","+strong+")");
      fade.addColorStop(1,"rgba("+triple+","+weak+")");
      perContext.set(key,fade);
    }
    return fade;
  }

  // Brightest on the middle line, fading to both edges. A single two-stop
  // gradient cannot do this, and an envelope lit from one side reads as a
  // shape that is falling over.
  function centreFade(ctx,triple,top,bottom,strong,weak){
    let perContext=gradients.get(ctx);
    if(!perContext){ perContext=new Map(); gradients.set(ctx,perContext); }
    const key="c"+triple+"|"+Math.round(top)+"|"+Math.round(bottom)+"|"+strong+"|"+weak;
    let fade=perContext.get(key);
    if(!fade){
      fade=ctx.createLinearGradient(0,top,0,bottom);
      fade.addColorStop(0,"rgba("+triple+","+weak+")");
      fade.addColorStop(.5,"rgba("+triple+","+strong+")");
      fade.addColorStop(1,"rgba("+triple+","+weak+")");
      perContext.set(key,fade);
    }
    return fade;
  }

  // Scratch for one curve. 256 covers the widest series the plugin publishes
  // (128 oscilloscope points); nothing here allocates per frame.
  const curveX=new Float32Array(256), curveY=new Float32Array(256);
  // Quadratic through the midpoints. Ninety-six bands across a 200 px panel
  // is a polyline with a visible corner at every band, and those corners are
  // most of what reads as crude — one extra number per point removes them.
  function curveThrough(ctx,count,continuing){
    if(count<=0) return;
    // A closed envelope is one path down its top edge and back along its
    // bottom. Opening the return leg with `moveTo` splits it into two
    // subpaths, and two subpaths fill as two separate shapes.
    if(continuing) ctx.lineTo(curveX[0],curveY[0]); else ctx.moveTo(curveX[0],curveY[0]);
    for(let index=1;index<count-1;index++){
      ctx.quadraticCurveTo(curveX[index],curveY[index],
        (curveX[index]+curveX[index+1])*.5,(curveY[index]+curveY[index+1])*.5);
    }
    if(count>1) ctx.lineTo(curveX[count-1],curveY[count-1]);
  }
  // A wide faint stroke under a thin bright one. `shadowBlur` reads the same
  // and costs an offscreen blur per stroke, every frame, on every panel.
  function glowStroke(ctx,triple,width){
    ctx.lineJoin="round"; ctx.lineCap="round";
    ctx.strokeStyle="rgba("+triple+",.16)"; ctx.lineWidth=width*3.2; ctx.stroke();
    ctx.strokeStyle="rgba("+triple+",.95)"; ctx.lineWidth=width;     ctx.stroke();
  }
  function gridLines(ctx,alpha){
    ctx.strokeStyle=ink.line; ctx.lineWidth=1; ctx.globalAlpha=alpha;
    ctx.stroke(); ctx.globalAlpha=1;
  }
  // Labels drop out rather than shrink or overlap: an unreadable label is
  // worse than a missing one, and the grid line stays either way.
  function labelRun(ctx){
    let lastRight=-1e9;
    return (text,centre,y)=>{
      const half=ctx.measureText(text).width*.5+3;
      if(centre-half<lastRight) return;
      ctx.fillText(text,centre,y);
      lastRight=centre+half;
    };
  }

  /* Geometry each panel already computed, kept for the pointer readout. Fields
     are written into preallocated objects rather than fresh ones: the readout
     must not turn nine panels into nine allocations a frame. */
  const PROBE={loudness:{},vu:{},spectrum:{},spectrogram:{},waveform:{},
               oscilloscope:{},stereo:{}};

  /* ==== spectrum data =====================================================
     The plugin publishes (dBFS + 120) / 120 per band. The page plots a 90 dB
     window out of that 120: nothing a mix decision depends on lives below
     -90 dBFS, and spending a quarter of the panel on it is why the curve read
     as a solid block with no floor under it. */
  const SLOT_FLOOR_DB=-120, SLOT_RANGE_DB=120;
  const SPECTRUM_TOP_DB=0, SPECTRUM_FLOOR_DB=-90;
  const slotDb=slot=>SLOT_FLOOR_DB+slot*SLOT_RANGE_DB;
  const spectrumFraction=db=>
    clamp((db-SPECTRUM_FLOOR_DB)/(SPECTRUM_TOP_DB-SPECTRUM_FLOOR_DB),0,1);

  // 20 Hz to 20 kHz, matching ScopeConfig's defaults. The analyzer narrows the
  // top to Nyquist below a 40 kHz session, which shifts the labels a little at
  // 32 kHz and not at all at anything a host runs by default.
  const SPECTRUM_MIN_HZ=20, SPECTRUM_MAX_HZ=20000;
  const melOf=hz=>2595*Math.log10(1+hz/700);
  // Where a frequency sits across the rail, in the same scale the analyzer
  // binned it with. Getting this wrong is not cosmetic: with the decade marks
  // hardcoded at 0.24 / 0.5 / 0.79 the 1 k label sat a sixth of the panel
  // away from 1 k, and 10 k was off by more than a tenth.
  function bandPosition(hz){
    if(spectrumScale==="linear"){
      return (hz-SPECTRUM_MIN_HZ)/(SPECTRUM_MAX_HZ-SPECTRUM_MIN_HZ);
    }
    if(spectrumScale==="mel"){
      const low=melOf(SPECTRUM_MIN_HZ), high=melOf(SPECTRUM_MAX_HZ);
      return (melOf(hz)-low)/(high-low);
    }
    return Math.log10(hz/SPECTRUM_MIN_HZ)/Math.log10(SPECTRUM_MAX_HZ/SPECTRUM_MIN_HZ);
  }

  // The inverse of `bandPosition`, for turning a pointer position back into a
  // frequency. Kept next to it so the pair cannot drift apart.
  function positionHz(position){
    const at=clamp(position,0,1);
    if(spectrumScale==="linear"){
      return SPECTRUM_MIN_HZ+(SPECTRUM_MAX_HZ-SPECTRUM_MIN_HZ)*at;
    }
    if(spectrumScale==="mel"){
      const low=melOf(SPECTRUM_MIN_HZ), high=melOf(SPECTRUM_MAX_HZ);
      return 700*(Math.pow(10,(low+(high-low)*at)/2595)-1);
    }
    return SPECTRUM_MIN_HZ*Math.pow(SPECTRUM_MAX_HZ/SPECTRUM_MIN_HZ,at);
  }
  const hzText=hz=>hz>=10000?(hz/1000).toFixed(1)+" kHz"
                  :hz>=1000?(hz/1000).toFixed(2)+" kHz"
                  :Math.round(hz)+" Hz";
  const dbText=(value,digits=1)=>real(value)?value.toFixed(digits)+" dB":"--";

  const smoothedSpectrum=new Float32Array(BANDS), spectrumPeak=new Float32Array(BANDS);
  function resetSpectrumView(){
    smoothedSpectrum.fill(0);
    spectrumPeak.fill(0);
    clearSpectrogram();
  }
  function updateSpectrum(){
    // 0% smoothing follows the analyzer; 100% still converges, never freezes.
    const follow=.08+(100-smoothing)/100*.92;
    for(let band=0;band<BANDS;band++){
      const raw=clamp(v[I.spectrum+band]||0,0,1);
      smoothedSpectrum[band]+= (raw-smoothedSpectrum[band])*follow;
      // Instant attack, slow release. This trace is what makes a spectrum
      // readable at a glance: the live curve says what is sounding now, the
      // trace says what the section has been reaching.
      const level=smoothedSpectrum[band];
      spectrumPeak[band]=level>spectrumPeak[band]
        ? level
        : Math.max(level,spectrumPeak[band]-(spectrumHold?0:0.0035));
    }
  }
  // Tilt is read as total dB across the visible span, so ±9 dB at the edges at
  // the control's limit. Applied in dB, before the window, because a tilt
  // applied to a normalised height is a tilt of the panel, not of the signal.
  const spectrumTilt=band=>(band/(BANDS-1)-.5)*tiltDb;
  function spectrumLevel(band){
    return spectrumFraction(slotDb(smoothedSpectrum[band])+spectrumTilt(band));
  }
  const spectrumPeakLevel=band=>
    spectrumFraction(slotDb(spectrumPeak[band])+spectrumTilt(band));

  /* ==== loudness =========================================================
     Four vertical strips share one dB axis. This keeps M/S/I/TP readable
     when Neta sits in a shallow MiniMeters-style rail. */
  const BARS=[
    {key:"m",  label:"M",  idx:I.m,  colour:()=>ink.pink, triple:()=>rgb.pink},
    {key:"s",  label:"S",  idx:I.s,  colour:()=>ink.pink, triple:()=>rgb.pink},
    {key:"i",  label:"I",  idx:I.int,colour:()=>ink.cyan, triple:()=>rgb.cyan},
    {key:"tp", label:"TP", idx:I.tp, colour:x=>x>TP_CEILING?ink.red:ink["cyan-dim"],
                                     triple:x=>x>TP_CEILING?rgb.red:rgb["cyan-dim"]},
  ];
  const TICKS=[-60,-48,-36,-24,-18,-12,-6,0];
  const holds={};

  function drawLoudness(){
    const [ctx,w,h,dpr]=fit($("loudness"));
    const left=21*dpr, right=4*dpr, top=12*dpr;
    const programmeH=(loudnessTimeline||loudnessHistogram)&&h>142*dpr?Math.min(38*dpr,h*.23):0;
    const floor=h-15*dpr-programmeH;
    const plotW=w-left-right;
    const plotH=floor-top;
    if(plotW<=0||plotH<=0) return;
    const y=db=>floor-norm(db)*plotH;

    mono(ctx,8*dpr); ctx.textAlign="right";
    for(const db of TICKS){
      const ty=y(db);
      ctx.strokeStyle=ink.line; ctx.lineWidth=1;
      ctx.beginPath(); ctx.moveTo(left,ty); ctx.lineTo(w-right,ty); ctx.stroke();
      ctx.fillStyle=ink["pink-faint"]; ctx.fillText(String(db),left-3*dpr,ty+3*dpr);
    }
    const targetY=y(targetLufs);
    ctx.setLineDash([3*dpr,3*dpr]); ctx.strokeStyle=ink.amber; ctx.lineWidth=1*dpr;
    ctx.beginPath(); ctx.moveTo(left,targetY); ctx.lineTo(w-right,targetY); ctx.stroke();
    ctx.setLineDash([]);

    const gap=Math.max(3*dpr,plotW*.035), stripW=(plotW-gap*(BARS.length-1))/BARS.length;
    if(stripW<2*dpr) return;
    Object.assign(PROBE.loudness,{left,top,floor,plotH,stripW,gap,dpr});
    BARS.forEach((bar,index)=>{
      const x=left+index*(stripW+gap), value=v[bar.idx], live=real(value);
      ctx.fillStyle="rgba(255,255,255,.035)";
      ctx.fillRect(x,top,stripW,plotH);
      if(live){
        const colour=bar.colour(value), peak=y(clamp(value,BOTTOM_DB,TOP_DB));
        // Bright at the top of the strip, dark at the floor. A flat rectangle
        // gives the eye nothing to read but its own edge; the fade puts the
        // weight of the bar where the number is.
        ctx.fillStyle=verticalFade(ctx,bar.triple(value),top,floor,.95,.30);
        ctx.fillRect(x,peak,stripW,floor-peak);
        const hold=holds[bar.key]||(holds[bar.key]={v:BOTTOM_DB,t:0}), now=performance.now();
        if(value>=hold.v||now-hold.t>1400){ hold.v=value; hold.t=now; }
        ctx.fillStyle=colour;
        ctx.fillRect(x,y(clamp(hold.v,BOTTOM_DB,TOP_DB))-dpr,stripW,2*dpr);
      }
      mono(ctx,8*dpr); ctx.textAlign="center";
      ctx.fillStyle=live?ink.pink:ink["pink-faint"];
      ctx.fillText(bar.label,x+stripW/2,floor+9*dpr);
      if(live&&stripW>=22*dpr){
        ctx.fillStyle=ink.cyan;
        ctx.fillText(value.toFixed(1),x+stripW/2,Math.max(top+7*dpr,y(value)-4*dpr));
      }
    });
    // A compact three-run history turns the headline into a programme view
    // without turning vertical meters back into horizontal runways.
    if(programmeH>0){
      const chartTop=floor+7*dpr, chartBottom=h-4*dpr;
      if(chartBottom-chartTop>10*dpr){
        const chartH=chartBottom-chartTop;
        if(loudnessHistogram){
          ctx.fillStyle=ink["pink-faint"]; ctx.globalAlpha=.35;
          for(let point=0;point<64;point++){
            const level=clamp(v[I.histogram+point]||0,0,1);
            const chartX=left+point*plotW/64;
            ctx.fillRect(chartX,chartBottom-level*chartH,Math.max(dpr,plotW/64-dpr*.5),level*chartH);
          }
        }
        if(loudnessTimeline) for(let series=0;series<3;series++){
          const start=series===0?I.timelineM:series===1?I.timelineS:I.timelineI;
          ctx.strokeStyle=series===0?ink.pink:series===1?ink.amber:ink.cyan;
          ctx.lineWidth=dpr; ctx.globalAlpha=.8; ctx.beginPath();
          for(let point=0;point<64;point++){
            const chartX=left+point*plotW/63;
            const chartY=chartBottom-norm(v[start+point])*chartH;
            point?ctx.lineTo(chartX,chartY):ctx.moveTo(chartX,chartY);
          }
          ctx.stroke();
        }
        ctx.globalAlpha=1;
      }
    }
  }

  /* ==== VU ===============================================================
     Calibrated L/R strips. Native peak holds stay visible as horizontal caps. */
  const VU_ROWS=[["L",I.vuL,I.vuHoldL],["R",I.vuR,I.vuHoldR]];
  function drawVu(){
    const [ctx,w,h,dpr]=fit($("vu"));
    const left=19*dpr, right=4*dpr, top=12*dpr, floor=h-15*dpr, span=floor-top;
    const plotW=w-left-right;
    if(span<=0||plotW<=0) return;
    const y=db=>floor-norm(db)*span;
    mono(ctx,7.5*dpr); ctx.textAlign="right";
    for(const db of [-60,-36,-18,0]){
      const at=y(db); ctx.strokeStyle=ink.line; ctx.lineWidth=1;
      ctx.beginPath(); ctx.moveTo(left,at); ctx.lineTo(w-right,at); ctx.stroke();
      ctx.fillStyle=ink["pink-faint"]; ctx.fillText(String(db),left-3*dpr,at+3*dpr);
    }
    const gap=Math.max(4*dpr,plotW*.12), stripW=(plotW-gap)/2;
    if(stripW<3*dpr) return;
    Object.assign(PROBE.vu,{left,top,floor,span,stripW,gap,dpr});
    for(let index=0;index<VU_ROWS.length;index++){
      const [label,valueIndex,holdIndex]=VU_ROWS[index], x=left+index*(stripW+gap);
      const value=v[valueIndex], hold=v[holdIndex], live=real(value);
      ctx.fillStyle="rgba(255,255,255,.035)"; ctx.fillRect(x,top,stripW,span);
      if(live){
        const peak=y(clamp(value,BOTTOM_DB,TOP_DB));
        ctx.fillStyle=verticalFade(ctx,index===0?rgb.cyan:rgb.pink,top,floor,.95,.30);
        ctx.fillRect(x,peak,stripW,floor-peak);
        if(real(hold)){
          ctx.fillStyle=hold>TP_CEILING?ink.red:ink.amber;
          ctx.fillRect(x,y(clamp(hold,BOTTOM_DB,TOP_DB))-dpr,stripW,2*dpr);
        }
      }
      mono(ctx,8*dpr); ctx.textAlign="center";
      ctx.fillStyle=live?(index===0?ink.cyan:ink.pink):ink["pink-faint"];
      ctx.fillText(label,x+stripW/2,floor+9*dpr);
      if(live&&stripW>=25*dpr){
        ctx.fillText(value.toFixed(1),x+stripW/2,Math.max(top+7*dpr,y(value)-4*dpr));
      }
    }
    $("vuMode").textContent=vuMode+" · "+vuCalibration+" dB";
  }

  /* ==== spectrum =========================================================
     A frequency plot with no axis on it is a texture. This one carries both
     axes, a filled curve for the live shape, and a peak trace for what the
     section keeps reaching — which is the reading an analyzer is actually for.
     Minor grid lines are unlabelled on purpose: they give the eye a decade
     rhythm without four numbers fighting for a 150 px rail. */
  const FREQ_MARKS=[[30,""],[50,""],[100,"100"],[200,""],[300,""],[500,"500"],
                    [1000,"1k"],[2000,""],[3000,"3k"],[5000,""],[10000,"10k"],[15000,""]];
  const SPECTRUM_DB_MARKS=[-12,-24,-36,-48,-60,-72];
  function drawSpectrum(){
    const [ctx,w,h,dpr]=fit($("spectrum"));
    const axis=h-10*dpr, top=6*dpr, span=axis-top;
    if(span<=0||w<=0) return;
    const yOf=fraction=>axis-fraction*span;
    Object.assign(PROBE.spectrum,{top,axis,span,w});

    ctx.beginPath();
    for(const [hz] of FREQ_MARKS){
      const gx=Math.round(bandPosition(hz)*w)+.5;
      if(gx<0||gx>w) continue;
      ctx.moveTo(gx,top); ctx.lineTo(gx,axis);
    }
    for(const db of SPECTRUM_DB_MARKS){
      const gy=Math.round(yOf(spectrumFraction(db)))+.5;
      ctx.moveTo(0,gy); ctx.lineTo(w,gy);
    }
    gridLines(ctx,.5);
    if(w>110*dpr){
      mono(ctx,7.5*dpr); ctx.textAlign="right"; ctx.fillStyle=ink["pink-faint"];
      for(const db of [-24,-48,-72]) ctx.fillText(db,w-3*dpr,yOf(spectrumFraction(db)));
    }

    for(let band=0;band<BANDS;band++){
      curveX[band]=band*w/(BANDS-1);
      curveY[band]=yOf(spectrumLevel(band));
    }
    if(spectrumStyle!=="fft"){
      // One hue, brightness earned from level. Alternating cyan and pink by
      // band index was colour standing in for information it did not have.
      const gap=Math.max(1,dpr*.8), width=Math.max(1,w/BANDS-gap);
      ctx.fillStyle=verticalFade(ctx,rgb.cyan,top,axis,.95,.28);
      for(let band=0;band<BANDS;band++){
        const level=spectrumLevel(band), y=yOf(level);
        ctx.globalAlpha=.26+.74*level;
        ctx.fillRect(band*w/BANDS+gap*.5,y,width,axis-y);
      }
      ctx.globalAlpha=1;
    }
    if(spectrumStyle!=="bars"){
      ctx.beginPath(); curveThrough(ctx,BANDS);
      ctx.lineTo(w,axis); ctx.lineTo(0,axis); ctx.closePath();
      ctx.fillStyle=verticalFade(ctx,rgb.cyan,top,axis,.34,.02); ctx.fill();
      ctx.beginPath(); curveThrough(ctx,BANDS);
      glowStroke(ctx,rgb.cyan,1.1*dpr);
    }

    if(spectrumTrace){
      for(let band=0;band<BANDS;band++) curveY[band]=yOf(spectrumPeakLevel(band));
      ctx.beginPath(); curveThrough(ctx,BANDS);
      ctx.strokeStyle="rgba("+rgb.pink+",.8)"; ctx.lineWidth=1*dpr;
      ctx.lineJoin="round"; ctx.lineCap="round"; ctx.stroke();
    }

    mono(ctx,8*dpr); ctx.textAlign="center"; ctx.fillStyle=ink["pink-faint"];
    const place=labelRun(ctx);
    for(const [hz,label] of FREQ_MARKS){
      if(!label) continue;
      const lx=bandPosition(hz)*w;
      if(lx<0||lx>w) continue;
      place(label,lx,h-5*dpr);
    }
  }

  /* ==== spectrogram ======================================================
     Fixed ring + RGB lookup table. No RGB arrays per pixel, no history
     shift, no GC stutter while Reaper keeps feeding the editor. */
  // 200 columns rather than 64. Sixty-four columns stretched across a 250 px
  // panel is a 4x upscale, and a 4x upscale of a spectrogram is the blotchy
  // pink cloud this module used to draw instead of a time axis.
  const ROWS=200, spectrogramHistory=new Float32Array(ROWS*BANDS);
  let historyLength=0, historyCursor=0;
  const MAP_NETA=[[0,[14,10,22]],[.2,[54,28,84]],[.42,[168,42,120]],
                  [.62,[255,61,138]],[.82,[255,180,120]],[1,[224,250,255]]];
  const MAP_INFERNO=[[0,[0,0,4]],[.25,[64,9,102]],[.5,[187,55,84]],
                     [.75,[249,142,8]],[1,[252,255,164]]];
  const MAP_VIRIDIS=[[0,[68,1,84]],[.25,[59,82,139]],[.5,[33,145,140]],
                     [.75,[94,201,98]],[1,[253,231,37]]];
  const heatLut=new Uint8ClampedArray(256*3);
  // Declared before the first `buildHeatLut()` on purpose: the builder marks
  // the ring for repaint, and a `let` below the call would be a TDZ throw
  // during page setup.
  let spectrogramTick=0, spectrogramDirty=true;
  function buildHeatLut(){
    const ramp=colorMap==="inferno"?MAP_INFERNO:colorMap==="viridis"?MAP_VIRIDIS:MAP_NETA;
    for(let value=0;value<256;value++){
      const t=value/255;
      let low=ramp[0], high=ramp[ramp.length-1];
      for(let index=1;index<ramp.length;index++){
        if(t<=ramp[index][0]){ high=ramp[index]; low=ramp[index-1]; break; }
      }
      const k=(t-low[0])/((high[0]-low[0])||1), offset=value*3;
      heatLut[offset]=low[1][0]+(high[1][0]-low[1][0])*k;
      heatLut[offset+1]=low[1][1]+(high[1][1]-low[1][1])*k;
      heatLut[offset+2]=low[1][2]+(high[1][2]-low[1][2])*k;
    }
    spectrogramDirty=true;
  }
  buildHeatLut();
  function clearSpectrogram(){
    spectrogramHistory.fill(0);
    historyLength=0; historyCursor=0; spectrogramTick=0; spectrogramDirty=true;
  }
  function pushSpectrogram(){
    if(spectrogramHistoryMode==="off") return;
    const divider=Math.max(1,5-spectrogramSpeed);
    spectrogramTick=(spectrogramTick+1)%divider;
    if(spectrogramTick) return;
    if(historyLength===ROWS&&!spectrogramLoop) return;
    const offset=historyCursor*BANDS;
    for(let band=0;band<BANDS;band++){
      spectrogramHistory[offset+band]=spectrumLevel(band);
    }
    historyCursor=(historyCursor+1)%ROWS;
    historyLength=Math.min(ROWS,historyLength+1);
    spectrogramDirty=true;
  }
  const off=document.createElement("canvas");
  off.width=ROWS; off.height=BANDS;
  const offCtx=off.getContext("2d"), offImage=offCtx.createImageData(ROWS,BANDS);

  function drawSpectrogram(){
    const [ctx,w,h,dpr]=fit($("spectrogram"));
    if(spectrogramHistoryMode==="off"){
      mono(ctx,9*dpr); ctx.textAlign="center"; ctx.fillStyle=ink["pink-faint"];
      ctx.fillText("history off",w/2,h/2);
      return;
    }
    // The ring only moves when a column was pushed, so the 19 200-pixel repaint
    // follows the data rather than the refresh rate. The upscale still runs
    // every frame, because `fit` clears the canvas.
    if(spectrogramDirty){
      const data=offImage.data;
      const first=(historyCursor-historyLength+ROWS)%ROWS;
      for(let band=0;band<BANDS;band++){
        for(let column=0;column<ROWS;column++){
          const o=((BANDS-1-band)*ROWS+column)*4;
          // One column of history is one column of image. Resampling the
          // history across the full width instead meant a warming spectrogram
          // stretched its first two columns over the whole panel, which is the
          // pink blob this module was accused of being.
          const level=column<historyLength
            ? spectrogramHistory[((first+column)%ROWS)*BANDS+band]
            : 0;
          // Smoothstep before the lookup. The map is a ramp, so a linear level
          // spends most of it on quiet bands and everything ends up mid-bright
          // pink; the S-curve gives the floor back to the floor and lets loud
          // bands reach the top of the ramp.
          const shaped=level*level*(3-2*level);
          const lutOffset=(shaped*255|0)*3;
          data[o]=heatLut[lutOffset]; data[o+1]=heatLut[lutOffset+1];
          data[o+2]=heatLut[lutOffset+2]; data[o+3]=255;
        }
      }
      offCtx.putImageData(offImage,0,0);
      spectrogramDirty=false;
    }
    // Smoothing on: a spectrogram is a continuous surface, and 200 columns is
    // now fine enough that interpolating reads as detail instead of blur.
    ctx.imageSmoothingEnabled=true;
    ctx.imageSmoothingQuality="high";
    const filled=Math.max(1,historyLength);
    ctx.drawImage(off,0,0,filled,BANDS,0,0,w*filled/ROWS,h);
    Object.assign(PROBE.spectrogram,{w,h,filled});
    // Frequency guides, so a bright band can be named rather than only seen.
    if(h>70*dpr){
      ctx.beginPath();
      for(const [hz] of FREQ_MARKS){
        if(hz!==100&&hz!==1000&&hz!==10000) continue;
        const gy=Math.round(h-bandPosition(hz)*h)+.5;
        ctx.moveTo(0,gy); ctx.lineTo(w,gy);
      }
      ctx.strokeStyle="rgba(255,255,255,.14)"; ctx.lineWidth=1; ctx.stroke();
      if(w>90*dpr){
        mono(ctx,7.5*dpr); ctx.textAlign="left"; ctx.fillStyle="rgba(255,255,255,.5)";
        for(const [hz,label] of [[100,"100"],[1000,"1k"],[10000,"10k"]]){
          ctx.fillText(label,3*dpr,h-bandPosition(hz)*h-5*dpr);
        }
      }
    }
    if(historyLength<ROWS){
      mono(ctx,8*dpr); ctx.textAlign="center"; ctx.fillStyle=ink["pink-faint"];
      ctx.fillText("warming history "+Math.round(historyLength/ROWS*100)+"%",w/2,h/2);
    }
    if(spectrogramTimecode){
      mono(ctx,8*dpr); ctx.textAlign="right"; ctx.fillStyle=ink["pink-faint"];
      ctx.fillText("live · "+(clock/60).toFixed(1)+" s",w-4*dpr,8*dpr);
    }
  }

  /* ==== waveform ========================================================= */
  function drawWaveform(){
    const [ctx,w,h,dpr]=fit($("waveform"));
    // Same reasoning as the loudness block: an envelope stretched over 600
    // pixels of column is a shape, not a reading.
    const band=Math.min(h*0.8,190*dpr), mid=h/2, scale=band*0.46*waveformGain;
    const triple=waveformColor==="cyan"?rgb.cyan:waveformColor==="amber"?rgb.amber:rgb.pink;
    Object.assign(PROBE.waveform,{mid,scale,w});

    // Half-scale guides before the envelope, so the band has something to be
    // loud against.
    ctx.beginPath();
    for(const at of [-1,-.5,.5,1]){
      const gy=Math.round(mid+at*scale)+.5;
      ctx.moveTo(0,gy); ctx.lineTo(w,gy);
    }
    gridLines(ctx,.45);

    // One closed path, curved on both edges. The fill is brightest at the
    // centre line and fades out to the peaks, which is what makes an envelope
    // read as a body of sound rather than a pair of scribbled outlines.
    // Bars are one column per published point - the shape a DAW draws, and the
    // one that reads transients rather than smoothing them into the body.
    if(waveformStyle==="bars"){
      const columnW=Math.max(1,w/WAVE_POINTS-Math.max(1,dpr*.6));
      ctx.fillStyle=centreFade(ctx,triple,mid-scale,mid+scale,.95,.35);
      for(let i=0;i<WAVE_POINTS;i++){
        const high=mid-clamp(v[I.waveMax+i]||0,-1,1)*scale;
        const low=mid-clamp(v[I.waveMin+i]||0,-1,1)*scale;
        ctx.fillRect(i*w/WAVE_POINTS,Math.min(high,low),columnW,Math.max(dpr,Math.abs(low-high)));
      }
    } else {
      ctx.beginPath();
      for(let i=0;i<WAVE_POINTS;i++){
        curveX[i]=i*w/(WAVE_POINTS-1);
        curveY[i]=mid-clamp(v[I.waveMax+i]||0,-1,1)*scale;
      }
      curveThrough(ctx,WAVE_POINTS);
      for(let i=0;i<WAVE_POINTS;i++){
        curveX[i]=(WAVE_POINTS-1-i)*w/(WAVE_POINTS-1);
        curveY[i]=mid-clamp(v[I.waveMin+WAVE_POINTS-1-i]||0,-1,1)*scale;
      }
      curveThrough(ctx,WAVE_POINTS,true);
      ctx.closePath();
      // `line` keeps the outline and drops the body, for reading peaks over a
      // spectrogram or a busy neighbour.
      if(waveformStyle!=="line"){
        ctx.fillStyle=centreFade(ctx,triple,mid-scale,mid+scale,.42,.05); ctx.fill();
      }
      glowStroke(ctx,triple,1*dpr);
    }

    ctx.strokeStyle="rgba("+triple+",.5)"; ctx.lineWidth=1;
    ctx.beginPath(); ctx.moveTo(0,Math.round(mid)+.5); ctx.lineTo(w,Math.round(mid)+.5); ctx.stroke();
    if(waveformGain>1){
      mono(ctx,7.5*dpr); ctx.textAlign="right"; ctx.fillStyle=ink["pink-faint"];
      ctx.fillText(waveformGain+"x",w-3*dpr,8*dpr);
    }
  }

  /* ==== oscilloscope ====================================================
     416..672 is exactly 128 interleaved L/R samples. Raw samples stay raw:
     envelope data belongs in the waveform module above, not here. */
  function drawOscilloscope(){
    const [ctx,w,h,dpr]=fit($("oscilloscope"));
    const top=8*dpr, bottom=h-8*dpr;
    // `split` gives each channel its own axis in half the height, so detail
    // stops competing with the other trace for the same pixels.
    const lanes=oscStyle==="split"?2:1;
    const laneH=(bottom-top)/lanes;
    const scale=laneH*(lanes>1?0.40:0.42)*oscGain;
    const mid=top+laneH*.5;
    if(laneH<=0) return;
    Object.assign(PROBE.oscilloscope,{w,mid,scale,laneH,lanes});
    // Graticule, the shape a hardware scope has: quarter divisions across and
    // down, so a trace has a scale instead of only a middle.
    ctx.beginPath();
    for(let step=1;step<4;step++){
      const gx=Math.round(step*w/4)+.5;
      ctx.moveTo(gx,top); ctx.lineTo(gx,bottom);
    }
    for(let lane=0;lane<lanes;lane++){
      const centre=mid+lane*laneH;
      for(const at of [-1,-.5,.5,1]){
        const gy=Math.round(centre+at*laneH*.42)+.5;
        ctx.moveTo(0,gy); ctx.lineTo(w,gy);
      }
    }
    gridLines(ctx,.45);
    ctx.strokeStyle=ink.edge; ctx.lineWidth=1; ctx.beginPath();
    for(let lane=0;lane<lanes;lane++){
      const gy=Math.round(mid+lane*laneH)+.5;
      ctx.moveTo(0,gy); ctx.lineTo(w,gy);
    }
    ctx.stroke();

    // `sum` reads the mid signal, which is the channel a mono fold-down would
    // produce - the trace that answers "does this survive one speaker".
    const sample=(point,channel)=>oscStyle==="sum"
      ? (clamp(v[I.oscilloscope+point*2],-1,1)+clamp(v[I.oscilloscope+point*2+1],-1,1))*.5
      : clamp(v[I.oscilloscope+point*2+channel],-1,1);
    const drawChannel=(channel,triple,centre,offset)=>{
      ctx.beginPath();
      for(let point=0;point<OSCILLOSCOPE_POINTS;point++){
        curveX[point]=point*w/(OSCILLOSCOPE_POINTS-1);
        curveY[point]=centre-sample(point,channel)*scale+offset;
      }
      curveThrough(ctx,OSCILLOSCOPE_POINTS);
      glowStroke(ctx,triple,1*dpr);
    };
    if(oscStyle==="sum"){
      drawChannel(0,rgb.cyan,mid,0);
    } else if(oscStyle==="split"){
      drawChannel(0,rgb.cyan,mid,0);
      drawChannel(1,rgb.pink,mid+laneH,0);
      if(w>70*dpr){
        mono(ctx,7.5*dpr); ctx.textAlign="left";
        ctx.fillStyle=ink["cyan-dim"]; ctx.fillText("L",3*dpr,top+7*dpr);
        ctx.fillStyle=ink["pink-dim"]; ctx.fillText("R",3*dpr,top+laneH+7*dpr);
      }
    } else {
      drawChannel(0,rgb.cyan,mid,-dpr*.5);
      drawChannel(1,rgb.pink,mid,dpr*.5);
    }
    if(oscGain>1){
      mono(ctx,7.5*dpr); ctx.textAlign="right"; ctx.fillStyle=ink["pink-faint"];
      ctx.fillText(oscGain+"x",w-3*dpr,8*dpr);
    }
    $("oscMode").textContent=oscStyle==="sum"?oscMode+" · sum":oscMode+" · "+oscCycles;
  }

  /* ==== stereo ===========================================================
     A horizontal unipolar stereometer, the shape MiniMeters uses: position
     across the lane is where the energy sits between the channels, height is
     how much of it there is. Sixteen points a frame is a scatter, so they
     accumulate into a decaying histogram instead. */
  const PAN_BINS=64, pan=new Float32Array(PAN_BINS), panScratch=new Float32Array(PAN_BINS);
  // The scale the distribution is drawn against. Floored at 1 so anything
  // below the old saturation point still reads absolutely; above it the shape
  // is normalised, because `Math.min(1, ...)` per bin meant a mix at any real
  // level pinned every bin to the ceiling and the panel drew a filled block.
  let panScale=1;
  function pushStereo(){
    for(let i=0;i<PAN_BINS;i++) pan[i]*=0.88;
    for(let i=0;i<GONIO_POINTS;i++){
      const side=v[I.gonio+i*2]||0, m=v[I.gonio+i*2+1]||0;
      const energy=Math.hypot(m,side);
      if(energy<1e-4) continue;
      // -1 is all in one channel, +1 all in the other, 0 is centred.
      const position=clamp(side/(Math.abs(m)+Math.abs(side)),-1,1);
      const bin=clamp(Math.round((position+1)*0.5*(PAN_BINS-1)),0,PAN_BINS-1);
      pan[bin]+=energy;
    }
    // Sixteen points landing in ninety-six bins is a picket fence, and a
    // picket fence is not a stereo image. One blur pass a frame turns the
    // same samples into the distribution they are drawn from.
    for(let i=0;i<PAN_BINS;i++){
      const before=pan[i-1]??pan[i], after=pan[i+1]??pan[i];
      panScratch[i]=before*0.25+pan[i]*0.5+after*0.25;
    }
    pan.set(panScratch);
    // Peak follows instantly up and crawls down, so the shape keeps its
    // proportions between frames instead of rescaling on every one.
    let peak=0;
    for(let i=0;i<PAN_BINS;i++) if(pan[i]>peak) peak=pan[i];
    panScale=Math.max(1,peak>panScale?peak:panScale*0.985+peak*0.015);
  }
  function stereoInk(index){
    if(stereoColor==="static") return ink.cyan;
    if(stereoColor==="rgb") return "hsl("+(190+index/GONIO_POINTS*145)+" 74% 62%)";
    return index/GONIO_POINTS>.6?ink.pink:ink.cyan;
  }
  function drawStereo(){
    const [ctx,w,h,dpr]=fit($("stereo"));
    if(stereoStyle==="vectorscope"){
      const cx=w/2, cy=h/2, span=Math.min(w,h)*.42;
      // The graticule a hardware vectorscope has: two rings, the M/S cross,
      // and the L/R diagonals a phase reading is actually taken against.
      ctx.beginPath();
      for(const ring of [.5,1]){ ctx.moveTo(cx+span*ring,cy); ctx.arc(cx,cy,span*ring,0,Math.PI*2); }
      ctx.moveTo(cx-span,cy); ctx.lineTo(cx+span,cy);
      ctx.moveTo(cx,cy-span); ctx.lineTo(cx,cy+span);
      gridLines(ctx,.5);
      ctx.beginPath();
      const diagonal=span*Math.SQRT1_2;
      ctx.moveTo(cx-diagonal,cy+diagonal); ctx.lineTo(cx+diagonal,cy-diagonal);
      ctx.moveTo(cx-diagonal,cy-diagonal); ctx.lineTo(cx+diagonal,cy+diagonal);
      ctx.strokeStyle=ink.edge; ctx.lineWidth=1; ctx.globalAlpha=.5; ctx.stroke(); ctx.globalAlpha=1;
      if(w>90*dpr&&h>90*dpr){
        mono(ctx,7.5*dpr); ctx.fillStyle=ink["pink-faint"];
        ctx.textAlign="left";  ctx.fillText("L",cx-diagonal-9*dpr,cy-diagonal-2*dpr);
        ctx.textAlign="right"; ctx.fillText("R",cx+diagonal+9*dpr,cy-diagonal-2*dpr);
      }
      const radius=pointSize==="small"?1*dpr:pointSize==="large"?3*dpr:pointSize==="medium"?2*dpr:1.5*dpr;
      for(let i=0;i<GONIO_POINTS;i++){
        const side=clamp(v[I.gonio+i*2]||0,-1,1), mid=clamp(v[I.gonio+i*2+1]||0,-1,1);
        // Round dots, and the trail fades into the past rather than sitting at
        // a flat alpha. Square pixels at 64 points is how a scope reads cheap.
        ctx.fillStyle=stereoInk(i); ctx.globalAlpha=.22+.6*i/GONIO_POINTS;
        ctx.beginPath(); ctx.arc(cx+side*span,cy-mid*span,radius,0,Math.PI*2); ctx.fill();
      }
      ctx.globalAlpha=1;
      return;
    }
    const top=15*dpr, floor=h-17*dpr, span=floor-top;
    if(span<=0) return;
    Object.assign(PROBE.stereo,{w,top,floor,span});
    ctx.beginPath();
    for(const at of [0,0.25,0.5,0.75,1]){
      const gx=Math.round(at*w)+.5;
      ctx.moveTo(gx,top); ctx.lineTo(gx,floor);
    }
    for(const at of [0.33,0.66]){
      const gy=Math.round(floor-at*span)+.5;
      ctx.moveTo(0,gy); ctx.lineTo(w,gy);
    }
    gridLines(ctx,.45);

    // Sixty-four bars a distribution wide was a picket fence with a fill; the
    // same numbers as one curve are the distribution they came from. Colour by
    // position stays: centre energy is mono, edge energy is wide, and that is
    // visible here without reading the correlation number.
    for(let i=0;i<PAN_BINS;i++){
      curveX[i]=i*w/(PAN_BINS-1);
      curveY[i]=floor-clamp(pan[i]/panScale,0,1)*span*.92;
    }
    ctx.beginPath(); curveThrough(ctx,PAN_BINS);
    ctx.lineTo(w,floor); ctx.lineTo(0,floor); ctx.closePath();
    if(stereoColor==="static"){
      // One horizontal gradient rather than a per-bin colour test, so the
      // wide edges shade into the mono centre instead of switching at a bin.
      let perContext=gradients.get(ctx);
      if(!perContext){ perContext=new Map(); gradients.set(ctx,perContext); }
      const key="pan|"+Math.round(w);
      let across=perContext.get(key);
      if(!across){
        across=ctx.createLinearGradient(0,0,w,0);
        across.addColorStop(0,"rgba("+rgb.pink+",.55)");
        across.addColorStop(.28,"rgba("+rgb.cyan+",.5)");
        across.addColorStop(.72,"rgba("+rgb.cyan+",.5)");
        across.addColorStop(1,"rgba("+rgb.pink+",.55)");
        perContext.set(key,across);
      }
      ctx.fillStyle=across;
    } else {
      ctx.fillStyle=verticalFade(ctx,rgb.cyan,top,floor,.5,.1);
    }
    ctx.fill();
    ctx.beginPath(); curveThrough(ctx,PAN_BINS);
    glowStroke(ctx,rgb.cyan,1*dpr);
    mono(ctx,8.5*dpr); ctx.fillStyle=ink["pink-faint"];
    ctx.textAlign="left";   ctx.fillText("L",3*dpr,floor+9*dpr);
    ctx.textAlign="center"; ctx.fillText("MONO",w/2,floor+9*dpr);
    ctx.textAlign="right";  ctx.fillText("R",w-3*dpr,floor+9*dpr);

    // Correlation as a needle on the same axis, since it answers the same
    // question this lane is already asking.
    const corr=Number.isFinite(v[I.corr])?clamp(v[I.corr],-1,1):0;
    const nx=(corr*0.5+0.5)*w;
    ctx.fillStyle=corr<0?ink.red:ink.amber;
    ctx.fillRect(nx-1.5*dpr,top-6*dpr,3*dpr,7*dpr);
  }

  /* ==== visuals rail =====================================================
     Rays from a core: length is a band, the core is momentary loudness. The
     rail is the one place in the window that is allowed to be decorative,
     so the decoration is still made of the audio. */
  function drawVisuals(){
    const [ctx,w,h,dpr]=fit($("visuals"));
    const cx=w/2, cy=h/2, radius=Math.min(w,h)*0.42;
    const loud=real(v[I.m])?norm(v[I.m]):0;
    const turn=clock*0.004;

    const core=radius*(0.12+loud*0.3);
    // Flat chrome; colour appears only where data makes it appear.
    ctx.fillStyle=ink.pink; ctx.globalAlpha=.25+loud*.55;
    ctx.beginPath(); ctx.arc(cx,cy,Math.max(core,1),0,Math.PI*2); ctx.fill();
    ctx.globalAlpha=1;

    ctx.lineWidth=Math.max(1,1.4*dpr); ctx.lineCap="round";
    for(let i=0;i<BANDS;i++){
      const level=spectrumLevel(i);
      if(level<0.02) continue;
      const angle=turn+i/BANDS*Math.PI*2;
      const inner=radius*0.22, outer=inner+level*radius*0.78;
      ctx.strokeStyle=i<BANDS*0.4?ink.cyan:ink.pink;
      ctx.globalAlpha=0.25+level*0.75;
      ctx.beginPath();
      ctx.moveTo(cx+Math.cos(angle)*inner,cy+Math.sin(angle)*inner);
      ctx.lineTo(cx+Math.cos(angle)*outer,cy+Math.sin(angle)*outer);
      ctx.stroke();
    }
    ctx.globalAlpha=1;
  }

  /* ==== object rail ======================================================
     A WebGL point cloud. Points rather than a surface for two reasons: the
     files people actually have are hundreds of thousands of triangles, and
     a cloud is something the audio can push around — a surface could only
     spin. */
  const objectCanvas=$("object");
  let gl=null, program=null, buffer=null, pointCount=0, glDead=false;
  let objectLayoutEpoch=-1, objectW=0, objectH=0, objectDpr=1;
  let objectLastDraw=0, objectSpinTimer=0;
  // User orbit is an offset, never a replacement for programme motion.
  // Releasing the mouse leaves automatic rotation alive.
  let objectYaw=0, objectPitch=.42, objectPointer=null, objectX=0, objectY=0;
  const attributes={};

  // `mediump` is declared in both stages on purpose: a uniform shared
  // between them must agree, and a vertex shader defaults to `highp`, so
  // omitting this here links fine on some drivers and fails on others with
  // "Precisions of uniform 'scatter' differ between VERTEX and FRAGMENT".
  const VERTEX=`
    precision mediump float;
    attribute vec3 position;
    uniform float turn, pitch, scatter, size, aspect, zoom;
    varying float depth;
    void main(){
      float c=cos(turn), s=sin(turn);
      vec3 p=vec3(position.x*c+position.z*s, position.y, -position.x*s+position.z*c);
      p=vec3(p.x, p.y*cos(pitch)-p.z*sin(pitch), p.y*sin(pitch)+p.z*cos(pitch));
      // Loudness pushes every point out along its own radius, so the object
      // breathes instead of merely rotating.
      p*=1.0+scatter*0.22;
      // A fixed camera down -Z with a gentle perspective divide. The zoom
      // is passed in rather than fixed because this panel is a tall narrow
      // rail: dividing x by the aspect is the correct projection, and on a
      // portrait viewport that correct projection is exactly what pushes a
      // wide model off both edges.
      // (No backticks in here - this shader lives in a template literal.)
      float z=p.z+3.4;
      gl_Position=vec4(p.x*zoom/(aspect*z), p.y*zoom/z, 0.0, 1.0);
      gl_PointSize=size*(1.6/z);
      depth=clamp((3.4-p.z)/2.6,0.0,1.0);
    }`;
  const FRAGMENT=`
    precision mediump float;
    uniform float scatter;
    varying float depth;
    void main(){
      vec2 d=gl_PointCoord-vec2(0.5);
      if(dot(d,d)>0.25) discard;
      vec3 near=vec3(1.0,0.24,0.54), far=vec3(0.36,0.86,0.91);
      vec3 tint=mix(far,near,depth);
      gl_FragColor=vec4(tint,(0.30+depth*0.62)*(0.75+scatter*0.25));
    }`;

  function compile(context,kind,source){
    const shader=context.createShader(kind);
    context.shaderSource(shader,source); context.compileShader(shader);
    if(!context.getShaderParameter(shader,context.COMPILE_STATUS)) return null;
    return shader;
  }
  // Once a canvas has a WebGL context it can never hand out a 2D one, so a
  // failure here has to report itself in the panel's note rather than by
  // drawing an apology on the canvas.
  const giveUp=reason=>{ glDead=true; gl=null; $("objectNote").textContent=reason; return null; };

  function initGl(){
    if(gl||glDead) return gl;
    gl=objectCanvas.getContext("webgl",{alpha:true,antialias:true,premultipliedAlpha:false});
    if(!gl) return giveUp("no 3D on this system");
    const vertex=compile(gl,gl.VERTEX_SHADER,VERTEX);
    const fragment=compile(gl,gl.FRAGMENT_SHADER,FRAGMENT);
    if(!vertex||!fragment) return giveUp("3D shaders would not compile");
    program=gl.createProgram();
    gl.attachShader(program,vertex); gl.attachShader(program,fragment); gl.linkProgram(program);
    if(!gl.getProgramParameter(program,gl.LINK_STATUS)){
      return giveUp("3D program would not link");
    }
    gl.useProgram(program);
    attributes.position=gl.getAttribLocation(program,"position");
    for(const name of ["turn","pitch","scatter","size","aspect","zoom"]){
      attributes[name]=gl.getUniformLocation(program,name);
    }
    buffer=gl.createBuffer();
    gl.enable(gl.BLEND); gl.blendFunc(gl.SRC_ALPHA,gl.ONE_MINUS_SRC_ALPHA);
    setCloud(defaultCloud());
    return gl;
  }
  /// A Fibonacci sphere, so the panel shows what it is for before a model
  /// is chosen rather than sitting empty.
  function defaultCloud(){
    const total=768, points=new Float32Array(total*3), golden=Math.PI*(3-Math.sqrt(5));
    for(let i=0;i<total;i++){
      const y=1-(i/(total-1))*2, r=Math.sqrt(Math.max(0,1-y*y)), a=golden*i;
      points[i*3]=Math.cos(a)*r; points[i*3+1]=y; points[i*3+2]=Math.sin(a)*r;
    }
    return points;
  }
  function setCloud(points){
    if(!initGl()) return;
    gl.bindBuffer(gl.ARRAY_BUFFER,buffer);
    gl.bufferData(gl.ARRAY_BUFFER,points,gl.STATIC_DRAW);
    gl.enableVertexAttribArray(attributes.position);
    gl.vertexAttribPointer(attributes.position,3,gl.FLOAT,false,0,0);
    pointCount=points.length/3;
    $("objectPoints").textContent=pointCount?pointCount.toLocaleString()+" pts":"";
  }
  function drawObject(force=false){
    if(!initGl()) return;
    const now=performance.now();
    if(!force&&now-objectLastDraw<30) return;
    objectLastDraw=now;
    if(objectLayoutEpoch!==layoutEpoch){
      objectDpr=Math.min(devicePixelRatio||1,2);
      const rect=objectCanvas.getBoundingClientRect();
      objectW=Math.max(1,Math.round(rect.width*objectDpr));
      objectH=Math.max(1,Math.round(rect.height*objectDpr));
      if(objectCanvas.width!==objectW||objectCanvas.height!==objectH){
        objectCanvas.width=objectW; objectCanvas.height=objectH;
      }
      objectLayoutEpoch=layoutEpoch;
    }
    gl.viewport(0,0,objectW,objectH);
    gl.clearColor(0.059,0.047,0.086,1); gl.clear(gl.COLOR_BUFFER_BIT);
    if(!pointCount) return;
    const loud=real(v[I.m])?norm(v[I.m]):0;
    // Automatic spin is part of the turn, so switching it off leaves the model
    // exactly where the user last dragged it instead of snapping to zero.
    gl.uniform1f(attributes.turn,(objectSpin?now*.00036:0)+objectYaw);
    gl.uniform1f(attributes.pitch,objectPitch);
    gl.uniform1f(attributes.scatter,loud);
    const pointScale=pointSize==="small"?.8:pointSize==="medium"?1.15:pointSize==="large"?1.6:1;
    gl.uniform1f(attributes.size,Math.max(1.25,1.75*objectDpr*pointScale));
    const aspect=objectW/objectH;
    gl.uniform1f(attributes.aspect,aspect);
    // The cloud is normalised to +/-1 on its longest axis, so this is the
    // largest zoom that still fits that axis inside the narrower dimension.
    // The cloud is normalised to +/-1 on its longest axis, so the base is the
    // largest zoom that still fits that axis inside the narrower dimension.
    // The wheel scales that rather than replacing it, which keeps a zoomed
    // model framed the same way after a resize.
    gl.uniform1f(attributes.zoom,Math.min(1.7,Math.max(0.3,aspect*2.6))*objectZoom);
    gl.drawArrays(gl.POINTS,0,pointCount);
  }

  function scheduleObjectSpin(){
    if(objectSpinTimer||!objectSpin||!enabled[MODULES.indexOf("object")]) return;
    objectSpinTimer=window.setTimeout(()=>{
      objectSpinTimer=0;
      if(enabled[MODULES.indexOf("object")]){
        drawObject();
        scheduleObjectSpin();
      }
    },34);
  }

  objectCanvas.addEventListener("pointerdown",event=>{
    event.preventDefault();
    objectPointer=event.pointerId; objectX=event.clientX; objectY=event.clientY;
    objectCanvas.setPointerCapture(event.pointerId);
    objectCanvas.classList.add("dragging");
  });
  objectCanvas.addEventListener("pointermove",event=>{
    if(event.pointerId!==objectPointer) return;
    objectYaw+=(event.clientX-objectX)*.012;
    objectPitch=clamp(objectPitch+(event.clientY-objectY)*.009,-1.18,1.18);
    objectX=event.clientX; objectY=event.clientY;
    requestDraw();
  });
  const releaseObject=event=>{
    if(event.pointerId!==objectPointer) return;
    if(objectCanvas.hasPointerCapture(event.pointerId)) objectCanvas.releasePointerCapture(event.pointerId);
    objectPointer=null; objectCanvas.classList.remove("dragging");
    save();
  };
  // Wheel and trackpad. `passive:false` because the gesture has to be taken
  // from the page: without preventDefault a trackpad pinch zooms the whole
  // editor, and inside a plugin window there is no way back from that.
  let objectSaveTimer=0;
  const saveObjectSoon=()=>{
    if(objectSaveTimer) return;
    objectSaveTimer=window.setTimeout(()=>{ objectSaveTimer=0; save(); },320);
  };
  objectCanvas.addEventListener("wheel",event=>{
    event.preventDefault();
    // A trackpad reports many small deltas and a wheel a few large ones, and
    // macOS sends a pinch as ctrl+wheel. Clamping the delta before the
    // exponential makes all three feel like the same gesture, and keeps one
    // fast flick from crossing the whole range.
    // Measured against both: one wheel notch (deltaY 100-120, clamped to 40)
    // moves about 1.27x, so the 0.5x-4x range is six notches wide rather than
    // two. A pinch sends many small deltas, so it gets the larger coefficient
    // per unit and still lands on a comparable feel.
    const travel=clamp(event.deltaY,-40,40)*(event.ctrlKey?0.010:0.006);
    objectZoom=clamp(objectZoom*Math.exp(-travel),OBJECT_ZOOM_MIN,OBJECT_ZOOM_MAX);
    drawObject(true);
    saveObjectSoon();
  },{passive:false});
  objectCanvas.addEventListener("dblclick",event=>{
    event.preventDefault();
    objectZoom=1; objectYaw=0; objectPitch=.42;
    drawObject(true); save();
  });

  objectCanvas.addEventListener("pointerup",releaseObject);
  objectCanvas.addEventListener("pointercancel",releaseObject);
  objectCanvas.addEventListener("lostpointercapture",()=>{
    objectPointer=null; objectCanvas.classList.remove("dragging");
    save();
  });

  /* ==== pointer readout ==================================================
     Every panel answers the same question under the cursor: what is the value
     here. The geometry comes from PROBE, written by the draw that put the
     pixels there, so a readout cannot disagree with the plot it is read off.
     Text only - the panels are never redrawn on pointer move. */
  const tip=$("tip");
  function moduleReadout(module,x,y,dpr){
    const px=x*dpr, py=y*dpr;
    if(module==="loudness"){
      const g=PROBE.loudness;
      if(!g.stripW) return "";
      const slot=Math.floor((px-g.left)/(g.stripW+g.gap));
      const cursorDb=BOTTOM_DB+(g.floor-py)/g.plotH*(TOP_DB-BOTTOM_DB);
      if(slot<0||slot>=BARS.length){
        return py>=g.top&&py<=g.floor?"cursor  "+cursorDb.toFixed(1)+" LUFS":"";
      }
      const bar=BARS[slot], value=v[bar.idx];
      const hold=holds[bar.key];
      return bar.label+"  "+(real(value)?value.toFixed(1)+" LUFS":"listening")
        +(hold&&real(hold.v)?"\npeak  "+hold.v.toFixed(1):"")
        +"\ncursor  "+cursorDb.toFixed(1);
    }
    if(module==="vu"){
      const g=PROBE.vu;
      if(!g.stripW) return "";
      const slot=Math.floor((px-g.left)/(g.stripW+g.gap));
      if(slot<0||slot>1) return "";
      const [label,valueIndex,holdIndex]=VU_ROWS[slot];
      const value=v[valueIndex], hold=v[holdIndex];
      return label+"  "+(real(value)?value.toFixed(1)+" dB":"listening")
        +(real(hold)?"\npeak  "+hold.toFixed(1):"")
        +"\nref  "+vuCalibration+" dB · "+vuMode;
    }
    if(module==="spectrum"){
      const g=PROBE.spectrum;
      if(!g.w) return "";
      const hz=positionHz(px/g.w);
      const band=clamp(Math.round(bandPosition(hz)*(BANDS-1)),0,BANDS-1);
      const live=slotDb(smoothedSpectrum[band])+spectrumTilt(band);
      const peak=slotDb(spectrumPeak[band])+spectrumTilt(band);
      const cursorDb=SPECTRUM_FLOOR_DB
        +(g.axis-py)/g.span*(SPECTRUM_TOP_DB-SPECTRUM_FLOOR_DB);
      return hzText(hz)+"\nlevel  "+live.toFixed(1)+" dBFS"
        +(spectrumTrace?"\npeak  "+peak.toFixed(1):"")
        +"\ncursor  "+cursorDb.toFixed(1);
    }
    if(module==="spectrogram"){
      const g=PROBE.spectrogram;
      if(!g.h) return "";
      const hz=positionHz(1-py/g.h);
      // Columns are pushed on drawn frames, so age is counted in columns
      // rather than claimed in seconds the page cannot actually know.
      const column=Math.floor(px/g.w*ROWS);
      const age=clamp(g.filled-column,0,ROWS);
      return hzText(hz)+"\n"+(column<g.filled?age+" columns back":"no history here")
        +"\nhistory  "+Math.round(historyLength/ROWS*100)+"% of "+ROWS;
    }
    if(module==="waveform"){
      const g=PROBE.waveform;
      if(!g.w) return "";
      const point=clamp(Math.round(px/g.w*(WAVE_POINTS-1)),0,WAVE_POINTS-1);
      const high=v[I.waveMax+point]||0, low=v[I.waveMin+point]||0;
      const cursor=(g.mid-py)/g.scale;
      // Peak-to-peak in dB, which is the number an envelope is read for.
      const peak=Math.max(Math.abs(high),Math.abs(low));
      return "peak  "+(peak>0?(20*Math.log10(peak)).toFixed(1)+" dBFS":"silence")
        +"\nrange  "+low.toFixed(3)+" .. "+high.toFixed(3)
        +"\ncursor  "+cursor.toFixed(3);
    }
    if(module==="oscilloscope"){
      const g=PROBE.oscilloscope;
      if(!g.w) return "";
      const point=clamp(Math.round(px/g.w*(OSCILLOSCOPE_POINTS-1)),0,OSCILLOSCOPE_POINTS-1);
      const left=clamp(v[I.oscilloscope+point*2],-1,1);
      const right=clamp(v[I.oscilloscope+point*2+1],-1,1);
      if(oscStyle==="sum") return "sum  "+((left+right)*.5).toFixed(3);
      const lane=g.lanes>1?Math.floor(py/g.laneH):-1;
      return "L  "+left.toFixed(3)+"\nR  "+right.toFixed(3)
        +(lane===0?"\nlane  L":lane===1?"\nlane  R":"");
    }
    if(module==="stereo"){
      if(stereoStyle==="vectorscope"){
        const corr=Number.isFinite(v[I.corr])?v[I.corr]:0;
        return "correlation  "+(corr>=0?"+":"")+corr.toFixed(2)
          +"\nwidth  "+(Number.isFinite(v[I.width])?v[I.width].toFixed(2):"--");
      }
      const g=PROBE.stereo;
      if(!g.w) return "";
      const position=clamp(px/g.w*2-1,-1,1);
      const bin=clamp(Math.round((position+1)*.5*(PAN_BINS-1)),0,PAN_BINS-1);
      const share=clamp(pan[bin]/panScale,0,1);
      const side=position<-0.02?"left":position>0.02?"right":"centre";
      return side+"  "+Math.round(Math.abs(position)*100)+"%"
        +"\nenergy  "+Math.round(share*100)+"%"
        +"\ncorrelation  "+(Number.isFinite(v[I.corr])?(v[I.corr]>=0?"+":"")+v[I.corr].toFixed(2):"--");
    }
    if(module==="object"){
      return (pointCount?pointCount.toLocaleString()+" points":"no cloud")
        +"\nzoom  "+objectZoom.toFixed(2)+"x"
        +"\nscroll to zoom · double-click resets";
    }
    if(module==="visuals") return "driven by the spectrum\nnot a measurement";
    return "";
  }
  function hideTip(){
    if(!tip.hidden) tip.hidden=true;
  }
  function showTip(event){
    if(!hoverReadout||!$("sheet").hidden) return hideTip();
    const box=event.target.closest?.(".box");
    const canvas=box&&box.querySelector("canvas");
    if(!box||!canvas||event.target!==canvas) return hideTip();
    const module=box.dataset.module;
    const rect=canvas.getBoundingClientRect();
    const dpr=Math.min(devicePixelRatio||1,2);
    const text=moduleReadout(module,event.clientX-rect.left,event.clientY-rect.top,dpr);
    if(!text) return hideTip();
    if(tip.textContent!==text) tip.textContent=text;
    tip.hidden=false;
    // Placed against the viewport, flipping before it would leave it. A plugin
    // window does not scroll, so anything past the edge is simply gone.
    const width=tip.offsetWidth, height=tip.offsetHeight;
    let x=event.clientX+13, y=event.clientY+13;
    if(x+width>innerWidth-4) x=event.clientX-width-13;
    if(y+height>innerHeight-4) y=event.clientY-height-13;
    tip.style.left=Math.max(4,x)+"px";
    tip.style.top=Math.max(4,y)+"px";
  }
  $("row").addEventListener("pointermove",showTip);
  $("row").addEventListener("pointerleave",hideTip);
  $("row").addEventListener("pointerdown",hideTip);
  window.addEventListener("blur",hideTip);

  /* ==== readouts ========================================================= */
  function writeOutput(el,text,className=""){
    if(el.dataset.netaText!==text){ el.dataset.netaText=text; el.textContent=text; }
    if(el.className!==className) el.className=className;
  }
  function readout(id,value,digits,unit,hot){
    const el=$(id);
    if(!real(value)){ writeOutput(el,"listening","pending"); return; }
    writeOutput(el,value.toFixed(digits)+(unit?" "+unit:""),hot?"hot":"");
  }

  /* ==== frame ============================================================ */
  const DRAW={loudness:drawLoudness,vu:drawVu,spectrum:drawSpectrum,spectrogram:drawSpectrogram,
              waveform:drawWaveform,oscilloscope:drawOscilloscope,stereo:drawStereo,
              object:drawObject,visuals:drawVisuals};
  let RENDER_MS=0;
  function refreshRenderRate(){
    RENDER_MS=renderFps==="60"?1000/60:renderFps==="30"?1000/30:renderFps==="15"?1000/15:0;
  }
  const requestPaint=window.requestAnimationFrame
    ? callback=>window.requestAnimationFrame(callback)
    : callback=>window.setTimeout(()=>callback(performance.now()),0);
  let scheduledDraw=false, renderTimer=0, lastRender=-Infinity, dataDirty=false;

  function draw(){
    if(dataDirty){
      clock++;
      if(!spectrumHold){ updateSpectrum(); pushSpectrogram(); }
      pushStereo();
      dataDirty=false;
    }
    MODULES.forEach((name,index)=>{ if(enabled[index]) DRAW[name](); });
    readout("int",v[I.int],1,"LUFS");
    readout("lra",v[I.lra],1,"LU");
    readout("psr",v[I.psr],1,"LU");
    readout("plr",v[I.plr],1,"LU");
    const tp=v[I.tp];
    readout("tp",tp,1,"dBTP",real(tp)&&tp>TP_CEILING);
    const corr=Number.isFinite(v[I.corr])?v[I.corr]:0;
    const c=$("corr");
    writeOutput(c,(corr>=0?"+":"")+corr.toFixed(2),corr<0?"hot":"");
    writeOutput($("width"),(Number.isFinite(v[I.width])?v[I.width]:0).toFixed(2));
    const integrated=v[I.int];
    const hint=real(integrated)
      ? integrated.toFixed(1)+" LUFS · target "+targetLufs
      : "listening";
    if($("hint").textContent!==hint) $("hint").textContent=hint;
  }

  function requestDraw(){
    if(scheduledDraw||renderTimer) return;
    scheduledDraw=true;
    requestPaint(now=>{
      scheduledDraw=false;
      const wait=RENDER_MS-(now-lastRender);
      if(wait>0){
        renderTimer=window.setTimeout(()=>{ renderTimer=0; requestDraw(); },Math.ceil(wait));
        return;
      }
      lastRender=now;
      draw();
    });
  }

  window.__neta_frame=payload=>{
    if(payload instanceof Float32Array){
      if(payload.length!==SCOPE_SLOTS) return;
      v.set(payload);
    }else if(typeof payload!=="string"||!decodeBase64Frame(payload)){
      return;
    }
    dataDirty=true;
    requestDraw();
  };

  /* ==== settings ========================================================= */
  const chips=$("moduleChips"), targetChips=$("targetChips"), row=$("row");
  const boxes={};
  for(const box of row.querySelectorAll(".box")) boxes[box.dataset.module]=box;

  MODULES.forEach((name,index)=>{
    const chip=document.createElement("button");
    chip.className="chip"; chip.type="button"; chip.textContent=name;
    chip.addEventListener("click",()=>{ enabled[index]=!enabled[index]; applySettings(); save(); });
    chips.appendChild(chip);
  });

  TARGETS.forEach(target=>{
    const chip=document.createElement("button");
    chip.className="chip"; chip.type="button"; chip.dataset.target=target;
    chip.textContent=target+" LUFS";
    chip.setAttribute("aria-label","Set delivery target to "+target+" LUFS");
    chip.addEventListener("click",()=>{ targetLufs=target; applySettings(); save(); });
    targetChips.appendChild(chip);
  });

  // Settings live in state, never in a second shadow model. Each choice
  // changes one scalar, redraws locally, then sends the compact v2 block.
  const choiceBindings=[];
  function choices(id,entries){
    const container=$(id);
    entries.forEach(entry=>{
      const chip=document.createElement("button");
      chip.className="chip"; chip.type="button"; chip.textContent=entry.label;
      chip.addEventListener("click",()=>{ entry.select(); applySettings(); save(); });
      entry.node=chip; container.appendChild(chip);
    });
    choiceBindings.push(entries);
  }
  function syncChoices(){
    choiceBindings.forEach(entries=>entries.forEach(entry=>{
      entry.node.setAttribute("aria-pressed",entry.active()?"true":"false");
    }));
  }
  const setTheme=value=>{ theme=value; };
  choices("themeChips",["midnight","coal","mono"].map(value=>({label:value,active:()=>theme===value,select:()=>setTheme(value)})));
  choices("mapChips",["neta","inferno","viridis"].map(value=>({label:value,active:()=>colorMap===value,select:()=>{colorMap=value; buildHeatLut();}})));
  choices("fpsChips",[["vsync","VSync"],["60","60 FPS"],["30","30 FPS"],["15","15 FPS"]].map(([value,label])=>({label,active:()=>renderFps===value,select:()=>{renderFps=value;}})));
  choices("routeChips",[["stereo","Stereo"],["left","L"],["right","R"],["mid","M"],["side","S"]].map(([value,label])=>({
    label, active:()=>route===value,
    select:()=>{ if(route!==value){ route=value; resetSpectrumView(); } },
  })));
  choices("fftChips",[1024,2048,4096,8192,16384].map(value=>({
    label:String(value), active:()=>fftSize===value,
    select:()=>{ if(fftSize!==value){ fftSize=value; resetSpectrumView(); } },
  })));
  choices("scaleChips",["log","mel","linear"].map(value=>({
    label:value, active:()=>spectrumScale===value,
    select:()=>{ if(spectrumScale!==value){ spectrumScale=value; resetSpectrumView(); } },
  })));
  choices("styleChips",["fft","bars","both"].map(value=>({label:value,active:()=>spectrumStyle===value,select:()=>{spectrumStyle=value;}})));
  choices("vuChips",["vu","rms","peak"].map(value=>({label:value,active:()=>vuMode===value,select:()=>{vuMode=value;}})));
  choices("oscChips",[
    {label:"Pitch",active:()=>oscMode==="pitch",select:()=>{oscMode="pitch";}},
    {label:"Free",active:()=>oscMode==="free",select:()=>{oscMode="free";}},
    {label:"Single",active:()=>oscCycles==="single",select:()=>{oscCycles="single";}},
    {label:"Multi",active:()=>oscCycles==="multi",select:()=>{oscCycles="multi";}},
  ]);
  choices("loudnessChips",[
    {label:"timeline",active:()=>loudnessTimeline,select:()=>{loudnessTimeline=!loudnessTimeline;}},
    {label:"histogram",active:()=>loudnessHistogram,select:()=>{loudnessHistogram=!loudnessHistogram;}},
    {label:"overs",active:()=>loudnessOvers,select:()=>{loudnessOvers=!loudnessOvers;}},
  ]);
  choices("spectrogramChips",["off","fast","slow"].map(value=>({label:value,active:()=>spectrogramHistoryMode===value,select:()=>{
    if(spectrogramHistoryMode!==value){ spectrogramHistoryMode=value; clearSpectrogram(); }
  }})));
  choices("waveformChips",["pink","cyan","amber"].map(value=>({label:value,active:()=>waveformColor===value,select:()=>{waveformColor=value;}})));
  choices("historyChips",[
    {label:"loop",active:()=>spectrogramLoop,select:()=>{spectrogramLoop=!spectrogramLoop;}},
    {label:"timecode",active:()=>spectrogramTimecode,select:()=>{spectrogramTimecode=!spectrogramTimecode;}},
    {label:"hold",active:()=>spectrumHold,select:()=>{spectrumHold=!spectrumHold;}},
  ]);
  choices("stereoStyleChips",["linear","vectorscope"].map(value=>({label:value,active:()=>stereoStyle===value,select:()=>{stereoStyle=value;}})));
  choices("stereoColorChips",["static","rgb","multiband"].map(value=>({label:value,active:()=>stereoColor===value,select:()=>{stereoColor=value;}})));
  choices("oscViewChips",["overlay","split","sum"].map(value=>({
    label:value, active:()=>oscStyle===value, select:()=>{oscStyle=value;}})));
  choices("oscGainChips",GAINS.map(value=>({
    label:value+"x", active:()=>oscGain===value, select:()=>{oscGain=value;}})));
  choices("traceChips",[
    {label:"peak trace",active:()=>spectrumTrace,select:()=>{spectrumTrace=!spectrumTrace;}},
  ]);
  choices("waveStyleChips",["band","bars","line"].map(value=>({
    label:value, active:()=>waveformStyle===value, select:()=>{waveformStyle=value;}})));
  choices("waveGainChips",GAINS.map(value=>({
    label:value+"x", active:()=>waveformGain===value, select:()=>{waveformGain=value;}})));
  choices("chromeChips",["bare","framed"].map(value=>({
    label:value, active:()=>chrome===value,
    select:()=>{ chrome=value; },
  })));
  choices("hoverChips",[
    {label:"hover readout",active:()=>hoverReadout,select:()=>{
      hoverReadout=!hoverReadout;
      if(!hoverReadout) hideTip();
    }},
  ]);
  choices("objectChips",[
    {label:"auto spin",active:()=>objectSpin,select:()=>{
      objectSpin=!objectSpin;
      if(objectSpin) scheduleObjectSpin();
      drawObject(true);
    }},
    {label:"reset view",active:()=>false,select:()=>{
      objectZoom=1; objectYaw=0; objectPitch=.42; drawObject(true);
    }},
  ]);
  choices("pointSizeChips",["auto","small","medium","large"].map(value=>({label:value,active:()=>pointSize===value,select:()=>{pointSize=value;}})));

  let actionCounters=[0,0,0];
  function fireAction(index){
    const next=actionCounters[index]>=65535?0:actionCounters[index]+1;
    actionCounters[index]=next;
    post("begin "+index); post("set "+index+" "+next); post("end "+index);
  }
  const actionEntries=[["Reset",0],["Capture A",1],["Capture B",2]];
  actionEntries.forEach(([label,index])=>{
    const chip=document.createElement("button");
    chip.className="chip"; chip.type="button"; chip.textContent=label;
    chip.addEventListener("click",()=>fireAction(index));
    $("actionChips").appendChild(chip);
  });
  $("actionReset").addEventListener("click",()=>fireAction(0));
  $("actionCaptureA").addEventListener("click",()=>fireAction(1));
  $("actionCaptureB").addEventListener("click",()=>fireAction(2));

  function applyBuiltinPreset(value){
    preset=value;
    enabled=MODULES.map(name=>name!=="visuals");
    // A preset is a whole view, so every display choice it does not name is
    // returned to its default rather than inherited from whatever was set.
    waveformStyle="band"; waveformGain=1; oscStyle="overlay"; oscGain=1;
    spectrumTrace=true;
    if(value==="mix"){
      targetLufs=-14; fftSize=2048; spectrumScale="log"; spectrumStyle="both";
      loudnessTimeline=true; loudnessHistogram=true;
    }else if(value==="master"){
      targetLufs=-14; fftSize=4096; spectrumScale="log"; spectrumStyle="both";
      loudnessTimeline=true; loudnessHistogram=true; loudnessOvers=true;
    }else if(value==="scope"){
      targetLufs=-14; fftSize=1024; spectrumScale="linear"; spectrumStyle="fft";
      enabled=MODULES.map(name=>["spectrum","waveform","oscilloscope","stereo"].includes(name));
      // A scope view is read for shape, not for level: split channels, both
      // panels zoomed in, and the waveform as columns rather than a body.
      oscMode="pitch"; oscCycles="multi"; oscStyle="split"; oscGain=2;
      waveformStyle="bars"; waveformGain=2;
    }else{
      targetLufs=DEFAULT_TARGET; fftSize=2048; spectrumScale="log"; spectrumStyle="both";
      loudnessTimeline=true; loudnessHistogram=true; loudnessOvers=true;
    }
  }
  choices("presetChips",["default","mix","master","scope"].map(value=>({label:value,active:()=>preset===value,select:()=>applyBuiltinPreset(value)})));

  function snapshot(){
    return {v:2,e:enabled,o:order,w:widths,model:modelPath,target:targetLufs,theme,map:colorMap,fps:renderFps,
      route,trim:trimDb,fft:fftSize,scale:spectrumScale,style:spectrumStyle,smooth:smoothing,tilt:tiltDb,
      hold:spectrumHold,vu:vuMode,cal:vuCalibration,osc:oscMode,cycles:oscCycles,timeline:loudnessTimeline,
      hist:loudnessHistogram,overs:loudnessOvers,sphist:spectrogramHistoryMode,speed:spectrogramSpeed,
      loop:spectrogramLoop,timecode:spectrogramTimecode,stereo:stereoStyle,stereocolor:stereoColor,
      points:pointSize,wavecolor:waveformColor,wavestyle:waveformStyle,wavegain:waveformGain,
      oscstyle:oscStyle,oscgain:oscGain,trace:spectrumTrace,hover:hoverReadout,chrome,
      oy:objectTenths(objectYaw),op:objectTenths(objectPitch),
      oz:Math.round(clamp(objectZoom,OBJECT_ZOOM_MIN,OBJECT_ZOOM_MAX)*10),ospin:objectSpin,preset};
  }
  function saveSlot(index){
    try{ slots[index]=btoa(JSON.stringify(snapshot())); }catch(_){ slots[index]=""; }
    save(); syncSlots();
  }
  function loadSlot(index){
    if(!slots[index]) return;
    try{
      const restored=JSON.parse(atob(slots[index]));
      restored.slots=slots.slice();
      window.__neta_settings(restored); save();
    }catch(_){ slots[index]=""; save(); }
    syncSlots();
  }
  function syncSlots(){
    const container=$("slotChips");
    container.textContent="";
    slots.forEach((slot,index)=>{
      const saveButton=document.createElement("button");
      saveButton.className="chip"; saveButton.type="button"; saveButton.textContent="Save "+(index+1);
      saveButton.addEventListener("click",()=>saveSlot(index));
      const loadButton=document.createElement("button");
      loadButton.className="chip"; loadButton.type="button"; loadButton.textContent="Load "+(index+1);
      loadButton.disabled=!slot; loadButton.setAttribute("aria-pressed",slot?"true":"false");
      loadButton.addEventListener("click",()=>loadSlot(index));
      container.append(saveButton,loadButton);
    });
  }

  window.__seco_update=params=>{
    if(!Array.isArray(params)) return;
    params.slice(0,3).forEach((param,index)=>{
      const min=Number(param&&param.min), max=Number(param&&param.max), normalized=Number(param&&param.v);
      if(Number.isFinite(min)&&Number.isFinite(max)&&Number.isFinite(normalized)){
        actionCounters[index]=Math.round(min+(max-min)*clamp(normalized,0,1));
      }
    });
  };

  // Rebuild rail so order, visibility, and dividers always agree.
  function layout(){
    row.textContent="";
    for(const box of Object.values(boxes)) delete box.dataset.drop;
    const visible=order.filter(module=>enabled[module]);
    visible.forEach((module,position)=>{
      if(position>0) row.appendChild(makeGrip(visible[position-1],module));
      const box=boxes[MODULES[module]];
      box.hidden=false;
      box.style.flexGrow=widths[module];
      row.appendChild(box);
    });
    for(const [name,box] of Object.entries(boxes)){
      if(!enabled[MODULES.indexOf(name)]) box.hidden=true;
    }
    invalidateCanvases();
  }

  /* ==== reordering and hiding from the panel headers =======================
     `order` also holds the hidden modules, so a drop cannot be two splices on
     it: the visible sequence is rebuilt and written back over the positions
     that were visible, which keeps the hidden ones where they were and keeps
     `order` a permutation. */
  function visibleOrder(){
    return order.filter(module=>enabled[module]);
  }
  function setVisibleOrder(nextVisible){
    let cursor=0;
    order=order.map(module=>(enabled[module]?nextVisible[cursor++]:module));
  }
  // Where the pointer would insert, as a position in the visible sequence.
  // Taken from live box geometry, so a wide panel and a narrow one behave the
  // same instead of the wide one being easier to aim at.
  function dropSlot(clientX){
    const visible=visibleOrder();
    for(let position=0;position<visible.length;position++){
      const rect=boxes[MODULES[visible[position]]].getBoundingClientRect();
      if(clientX<rect.left+rect.width*.5) return position;
    }
    return visible.length;
  }
  function markDrop(slot){
    const visible=visibleOrder();
    for(const module of visible) delete boxes[MODULES[module]].dataset.drop;
    if(slot===null||!visible.length) return;
    if(slot>=visible.length){
      boxes[MODULES[visible[visible.length-1]]].dataset.drop="after";
    }else{
      boxes[MODULES[visible[slot]]].dataset.drop="before";
    }
  }
  let capDrag=null;
  function endCapDrag(commit,clientX){
    if(!capDrag) return;
    const {cap,module,box}=capDrag;
    capDrag=null;
    cap.classList.remove("dragging");
    box.classList.remove("carrying");
    markDrop(null);
    if(!commit) return;
    const visible=visibleOrder();
    const from=visible.indexOf(module);
    let slot=dropSlot(clientX);
    // The slot indexes a list that still contains the dragged panel, so every
    // position past its own is one to the left once it is lifted out.
    if(slot>from) slot-=1;
    if(slot===from||from<0) return;
    const next=visible.slice();
    next.splice(from,1);
    next.splice(clamp(slot,0,next.length),0,module);
    setVisibleOrder(next);
    applySettings(); save();
  }
  for(const [name,box] of Object.entries(boxes)){
    const cap=box.querySelector(".cap");
    if(!cap) continue;
    const hide=document.createElement("button");
    hide.className="capHide"; hide.type="button";
    hide.textContent="\u00d7";
    hide.title="Hide "+name;
    hide.setAttribute("aria-label","Hide the "+name+" panel");
    hide.addEventListener("pointerdown",event=>event.stopPropagation());
    hide.addEventListener("click",()=>{
      // Never hide the last one: an empty rail has no way back to itself.
      if(visibleOrder().length<2) return;
      enabled[MODULES.indexOf(name)]=false;
      applySettings(); save();
    });
    cap.appendChild(hide);

    cap.addEventListener("pointerdown",event=>{
      if(event.pointerType==="mouse"&&event.button!==0) return;
      if(visibleOrder().length<2) return;
      event.preventDefault();
      cap.setPointerCapture(event.pointerId);
      capDrag={pointerId:event.pointerId,cap,box,module:MODULES.indexOf(name),
               startX:event.clientX,moved:false};
    });
    cap.addEventListener("pointermove",event=>{
      if(!capDrag||event.pointerId!==capDrag.pointerId) return;
      // A few pixels of slack, so a click on the header is a click and not a
      // reorder to where it already was.
      if(!capDrag.moved){
        if(Math.abs(event.clientX-capDrag.startX)<4) return;
        capDrag.moved=true;
        cap.classList.add("dragging");
        box.classList.add("carrying");
      }
      markDrop(dropSlot(event.clientX));
    });
    cap.addEventListener("pointerup",event=>{
      if(!capDrag||event.pointerId!==capDrag.pointerId) return;
      endCapDrag(capDrag.moved,event.clientX);
    });
    cap.addEventListener("pointercancel",()=>endCapDrag(false,0));
    cap.addEventListener("lostpointercapture",()=>endCapDrag(false,0));
  }

  function makeGrip(left,right){
    const grip=document.createElement("div");
    grip.className="grip";
    grip.setAttribute("role","separator");
    grip.setAttribute("aria-label","Resize "+MODULES[left]+" and "+MODULES[right]);
    grip.addEventListener("pointerdown",event=>{
      event.preventDefault();
      grip.setPointerCapture(event.pointerId);
      grip.classList.add("dragging");
      const startX=event.clientX, total=widths[left]+widths[right];
      const pairPixels=boxes[MODULES[left]].offsetWidth+boxes[MODULES[right]].offsetWidth;
      const perPixel=pairPixels>0?total/pairPixels:0;
      const drag=moveEvent=>{
        const shift=(moveEvent.clientX-startX)*perPixel;
        const next=Math.round(Math.min(total-MIN_WIDTH,Math.max(MIN_WIDTH,widths[left]+shift)));
        boxes[MODULES[left]].style.flexGrow=next;
        boxes[MODULES[right]].style.flexGrow=total-next;
        grip.dataset.left=next;
      };
      const drop=()=>{
        grip.classList.remove("dragging");
        grip.removeEventListener("pointermove",drag);
        grip.removeEventListener("pointerup",drop);
        grip.removeEventListener("pointercancel",drop);
        const next=Number(grip.dataset.left);
        if(Number.isFinite(next)&&next>=MIN_WIDTH&&next<=MAX_WIDTH){
          widths[left]=next; widths[right]=total-next;
          save();
        }
        invalidateCanvases();
        requestDraw();
      };
      grip.dataset.left=widths[left];
      grip.addEventListener("pointermove",drag);
      grip.addEventListener("pointerup",drop);
      grip.addEventListener("pointercancel",drop);
    });
    return grip;
  }

  function applySettings(){
    MODULES.forEach((name,index)=>{
      chips.children[index].setAttribute("aria-pressed",enabled[index]?"true":"false");
    });
    for(const chip of targetChips.children){
      chip.setAttribute("aria-pressed",Number(chip.dataset.target)===targetLufs?"true":"false");
    }
    document.documentElement.dataset.theme=theme;
    // Chrome changes every canvas's height, so this has to land before the
    // layout pass below retires the fit cache.
    shell.dataset.chrome=chrome;
    readInk();
    buildHeatLut();
    refreshRenderRate();
    layout();
    // The last visible panel says so rather than silently ignoring the click.
    const alone=visibleOrder().length<2;
    for(const box of Object.values(boxes)){
      const hide=box.querySelector(".capHide");
      if(hide) hide.disabled=alone;
    }
    syncChoices(); syncSlots();
    $("modelPath").value=modelPath;
    $("trimInput").value=trimDb;
    $("smoothingInput").value=smoothing;
    $("tiltInput").value=tiltDb;
    $("calibrationInput").value=vuCalibration;
    $("speedInput").value=spectrogramSpeed;
    scheduleObjectSpin();
    requestDraw();
  }
  function numberControl(id,minimum,maximum,assign){
    $(id).addEventListener("change",event=>{
      const value=Number(event.currentTarget.value);
      if(!Number.isFinite(value)) return;
      assign(clamp(Math.round(value),minimum,maximum));
      applySettings(); save();
    });
  }
  numberControl("trimInput",-24,24,value=>{if(trimDb!==value){trimDb=value; resetSpectrumView();}});
  numberControl("smoothingInput",0,100,value=>{smoothing=value;});
  numberControl("tiltInput",-18,18,value=>{tiltDb=value;});
  numberControl("calibrationInput",-36,-6,value=>{vuCalibration=value;});
  numberControl("speedInput",1,4,value=>{spectrogramSpeed=value;});
  const asFlag=value=>value?"1":"0";
  const objectTenths=value=>Math.round(clamp(value*180/Math.PI*10,-3600,3600));
  const save=()=>{
    const state=[
      "v=2","m="+enabled.map(asFlag).join(""),"o="+order.join(""),"w="+widths.join(","),
      "theme="+theme,"map="+colorMap,"fps="+renderFps,"route="+route,"trim="+trimDb,
      "fft="+fftSize,"scale="+spectrumScale,"style="+spectrumStyle,"smooth="+smoothing,
      "tilt="+tiltDb,"hold="+asFlag(spectrumHold),"vu="+vuMode,"cal="+vuCalibration,
      "osc="+oscMode,"cycles="+oscCycles,"timeline="+asFlag(loudnessTimeline),
      "hist="+asFlag(loudnessHistogram),"overs="+asFlag(loudnessOvers),
      "sphist="+spectrogramHistoryMode,"speed="+spectrogramSpeed,"loop="+asFlag(spectrogramLoop),
      "timecode="+asFlag(spectrogramTimecode),"stereo="+stereoStyle,
      "stereocolor="+stereoColor,"points="+pointSize,"wavecolor="+waveformColor,
      "wavestyle="+waveformStyle,"wavegain="+waveformGain,"oscstyle="+oscStyle,
      "oscgain="+oscGain,"trace="+asFlag(spectrumTrace),"hover="+asFlag(hoverReadout),
      "chrome="+chrome,
      "oy="+objectTenths(objectYaw),"op="+objectTenths(objectPitch),
      "oz="+Math.round(clamp(objectZoom,OBJECT_ZOOM_MIN,OBJECT_ZOOM_MAX)*10),
      "ospin="+asFlag(objectSpin),"preset="+preset,"target="+targetLufs
    ];
    if(modelPath&&!/[;\n\r]/.test(modelPath)) state.push("model="+modelPath);
    // Base64 snapshots legitimately end in '='. Semicolon is the one
    // state-field delimiter, so reject it and line breaks — not padding.
    slots.forEach((slot,index)=>{ if(slot&&!/[;\n\r]/.test(slot)) state.push("slot"+index+"="+slot); });
    post("state "+state.join(";"));
  };

  // Pushed by Plugin::editor_script from the same state block the session
  // stores, so the window opens the way it was left.
  window.__neta_settings=config=>{
    if(!config||typeof config!=="object") return;
    if(Array.isArray(config.e)&&config.e.length===MODULES.length){
      enabled=MODULES.map((_,index)=>config.e[index]!==false);
    }
    if(Array.isArray(config.o)&&config.o.length===MODULES.length
       &&config.o.every(value=>Number.isInteger(value)&&value>=0&&value<MODULES.length)
       &&new Set(config.o).size===MODULES.length) order=config.o.slice();
    if(Array.isArray(config.w)&&config.w.length===MODULES.length
       &&config.w.every(value=>Number.isFinite(value)&&value>=MIN_WIDTH&&value<=MAX_WIDTH)) widths=config.w.slice();
    modelPath=typeof config.model==="string"?config.model:"";
    if(Number.isFinite(config.target)&&TARGETS.includes(config.target)) targetLufs=config.target;
    const pick=(value,choices,current)=>choices.includes(value)?value:current;
    theme=pick(config.theme,["midnight","coal","mono"],theme);
    colorMap=pick(config.map,["neta","inferno","viridis"],colorMap);
    renderFps=pick(config.fps,["vsync","60","30","15"],renderFps);
    const nextRoute=pick(config.route,["stereo","left","right","mid","side"],route);
    const nextTrim=Number.isFinite(config.trim)?clamp(Math.round(config.trim),-24,24):trimDb;
    const nextFft=[1024,2048,4096,8192,16384].includes(config.fft)?config.fft:fftSize;
    const nextScale=pick(config.scale,["log","mel","linear"],spectrumScale);
    if(nextRoute!==route||nextTrim!==trimDb||nextFft!==fftSize||nextScale!==spectrumScale){
      resetSpectrumView();
    }
    route=nextRoute; trimDb=nextTrim; fftSize=nextFft; spectrumScale=nextScale;
    spectrumStyle=pick(config.style,["fft","bars","both"],spectrumStyle);
    if(Number.isFinite(config.smooth)) smoothing=clamp(Math.round(config.smooth),0,100);
    if(Number.isFinite(config.tilt)) tiltDb=clamp(Math.round(config.tilt),-18,18);
    spectrumHold=config.hold===true;
    vuMode=pick(config.vu,["vu","rms","peak"],vuMode);
    if(Number.isFinite(config.cal)) vuCalibration=clamp(Math.round(config.cal),-36,-6);
    oscMode=pick(config.osc,["pitch","free"],oscMode);
    oscCycles=pick(config.cycles,["single","multi"],oscCycles);
    oscStyle=pick(config.oscstyle,["overlay","split","sum"],oscStyle);
    waveformStyle=pick(config.wavestyle,["band","bars","line"],waveformStyle);
    if(GAINS.includes(config.wavegain)) waveformGain=config.wavegain;
    if(GAINS.includes(config.oscgain)) oscGain=config.oscgain;
    chrome=pick(config.chrome,["bare","framed"],chrome);
    spectrumTrace=config.trace!==false;
    hoverReadout=config.hover!==false;
    objectSpin=config.ospin!==false;
    if(Number.isFinite(config.oz)){
      objectZoom=clamp(config.oz/10,OBJECT_ZOOM_MIN,OBJECT_ZOOM_MAX);
    }
    loudnessTimeline=config.timeline!==false; loudnessHistogram=config.hist!==false;
    loudnessOvers=config.overs!==false;
    const nextHistory=pick(config.sphist,["off","fast","slow"],spectrogramHistoryMode);
    if(nextHistory!==spectrogramHistoryMode){ spectrogramHistoryMode=nextHistory; clearSpectrogram(); }
    if(Number.isFinite(config.speed)) spectrogramSpeed=clamp(Math.round(config.speed),1,4);
    spectrogramLoop=config.loop!==false; spectrogramTimecode=config.timecode===true;
    stereoStyle=pick(config.stereo,["linear","vectorscope"],stereoStyle);
    stereoColor=pick(config.stereocolor,["static","rgb","multiband"],stereoColor);
    pointSize=pick(config.points,["auto","small","medium","large"],pointSize);
    waveformColor=pick(config.wavecolor,["pink","cyan","amber"],waveformColor);
    if(Number.isFinite(config.oy)) objectYaw=config.oy/10*Math.PI/180;
    if(Number.isFinite(config.op)) objectPitch=clamp(config.op/10*Math.PI/180,-1.18,1.18);
    preset=pick(config.preset,["default","mix","master","scope"],preset);
    if(Array.isArray(config.slots)&&config.slots.length===4) slots=config.slots.map(slot=>typeof slot==="string"?slot:"");
    applySettings();
    if(modelPath) post("msg model "+modelPath);
  };
  window.__neta_model=(name,points)=>{
    setCloud(new Float32Array(points));
    $("objectNote").textContent=name+" · drag to orbit";
    $("modelStatus").textContent=name+" loaded \u2014 "+pointCount.toLocaleString()+" points.";
  };
  window.__neta_model_failed=reason=>{
    $("objectNote").textContent="no model";
    $("modelStatus").textContent=reason;
  };

  /* ==== settings drawer ==================================================
     One tab visible at a time. Every group the old sheet stacked downwards is
     a tab here, which is what lets the drawer stay short enough for the rail
     above it to keep working - the point of a meter's settings is watching the
     meter answer while you change them. */
  const sheet=$("sheet"), gear=$("gear"), shell=document.querySelector(".shell");
  const pages=$("pages"), tabsBar=$("tabs"), tabSpace=tabsBar.querySelector(".tabSpace");
  let settingsTab="layout";
  const tabButtons=[...pages.children].map(page=>{
    const button=document.createElement("button");
    button.className="tab"; button.type="button";
    button.dataset.tab=page.dataset.tab;
    button.setAttribute("role","tab");
    button.textContent=page.dataset.tab;
    button.addEventListener("click",()=>selectTab(page.dataset.tab));
    tabsBar.insertBefore(button,tabSpace);
    return button;
  });
  function selectTab(name){
    settingsTab=name;
    for(const page of pages.children){
      page.dataset.active=page.dataset.tab===name?"true":"false";
    }
    for(const button of tabButtons){
      button.setAttribute("aria-selected",button.dataset.tab===name?"true":"false");
    }
    // A tab is a different amount of content, so the scroll position from the
    // last one means nothing here.
    pages.scrollTop=0;
  }
  selectTab(settingsTab);

  const openSheet=open=>{
    sheet.hidden=!open;
    shell.classList.toggle("tuning",open);
    gear.setAttribute("aria-expanded",open?"true":"false");
    if(open) selectTab(settingsTab); else gear.focus({preventScroll:true});
    // The rail just changed height. Every canvas has to be remeasured before
    // the next frame: drawing new geometry into a stale backing store is
    // exactly what made the panels look torn.
    invalidateCanvases();
    layout();
    requestDraw();
    drawObject(true);
  };
  gear.addEventListener("click",()=>openSheet(sheet.hidden));
  $("sheetClose").addEventListener("click",()=>openSheet(false));
  $("sheetReset").addEventListener("click",()=>{
    enabled=MODULES.map(name=>name!=="visuals");
    order=DEFAULT_ORDER.slice(); widths=DEFAULT_WIDTHS.slice();
    targetLufs=DEFAULT_TARGET;
    applySettings(); save();
  });
  document.addEventListener("keydown",event=>{ if(event.key==="Escape") openSheet(false); });
  $("modelLoad").addEventListener("click",()=>{
    modelPath=$("modelPath").value.trim();
    $("modelStatus").textContent="Reading\u2026";
    post("msg model "+modelPath);
    save();
  });
  $("modelPath").addEventListener("keydown",event=>{
    if(event.key==="Enter") $("modelLoad").click();
  });

  readInk();
  applySettings();
  const resized=()=>{ invalidateCanvases(); requestDraw(); };
  window.addEventListener("resize",resized);
  if(typeof ResizeObserver==="function") new ResizeObserver(resized).observe(row);
  requestDraw();
})();
</script>
</body>
</html>"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_is_self_contained_and_frame_layout_is_bounded() {
        assert!(!HTML.contains("http://"));
        assert!(!HTML.contains("https://"));
        let payload = frame(&[0.0; SCOPE_SLOTS]).expect("fixed 896-slot payload");
        assert!(payload.starts_with("window.__neta_frame&&window.__neta_frame(\""));
        assert!(payload.ends_with("\");"));
        let encoded = payload
            .strip_prefix("window.__neta_frame&&window.__neta_frame(\"")
            .and_then(|text| text.strip_suffix("\");"))
            .expect("base64 frame wrapper");
        assert_eq!(
            base64::decode_config(encoded, STANDARD).unwrap().len(),
            SCOPE_BYTES
        );
        assert!(frame(&[]).is_none());
        assert_eq!(WAVE_MAX - WAVE_MIN, WAVE_POINTS);
        assert_eq!(GONIOMETER, SPECTRUM + SPECTRUM_POINTS);
    }

    #[test]
    fn minimum_geometry_keeps_the_horizontal_rail_legible() {
        assert_eq!(PAGE.width, 1_440);
        assert_eq!(PAGE.height, 440);
        assert_eq!(PAGE.minimum, Some((960, 240)));
        assert!(HTML.contains(".row { display:flex"));
        assert!(HTML.contains(".grip {\n    flex:0 0 8px"));
        assert!(!HTML.contains("grid-template-areas:"));
        assert!(!HTML.contains("@media (max-height:420px)"));
        assert!(HTML.contains("window.addEventListener(\"resize\",resized)"));
    }

    #[test]
    fn meters_are_vertical_and_settings_open_inside_the_available_viewport() {
        assert!(HTML.contains("Four vertical strips share one dB axis"));
        assert!(HTML.contains("const y=db=>floor-norm(db)*plotH"));
        assert!(HTML.contains("Calibrated L/R strips"));
        assert!(HTML.contains("@media (max-height:560px)"));
        // Settings are a drawer inside the shell's grid, never an overlay: the
        // meters have to stay live and visible while they are being tuned, and
        // the drawer's columns have to multiply with the width rather than the
        // groups stacking into a screen you must maximise the window to read.
        assert!(!HTML.contains("position:fixed; inset:0"));
        assert!(!HTML.contains("aria-modal"));
        assert!(HTML.contains(".shell.tuning { grid-template-rows:auto minmax(74px,1fr) minmax(0,auto) auto; }"));
        assert!(HTML.contains("grid-template-columns:repeat(auto-fit,minmax(196px,1fr))"));
        assert!(HTML.contains("shell.classList.toggle(\"tuning\",open)"));
        // Opening or closing it resizes every canvas, so the fit cache has to
        // be retired in the same turn or the next frame draws into stale
        // backing stores and the panels tear.
        assert!(HTML.contains("invalidateCanvases();\n    layout();\n    requestDraw();"));
        // One tab at a time is what keeps the drawer short.
        let tabs = ["layout", "view", "analysis", "loudness", "scopes", "stereo", "presets"];
        for tab in tabs {
            assert!(
                HTML.contains(&format!("data-tab=\"{tab}\"")),
                "settings lost the {tab} tab"
            );
        }
        assert!(HTML.contains("function selectTab(name)"));
    }

    #[test]
    fn rail_restores_user_order_width_and_keeps_visuals_opt_in() {
        assert!(HTML.contains("function makeGrip(left,right)"));
        // The arrow-and-number list is gone: headers drag, dividers resize.
        // Leaving both paths in means two ways to do one thing, and the one in
        // the drawer is the worse one.
        assert!(!HTML.contains("moduleRows"));
        assert!(!HTML.contains("function buildRows()"));
        // Order and visibility are reachable from the rail itself: drag a
        // panel's header to move it, hit its cross to hide it.
        assert!(HTML.contains("function dropSlot(clientX)"));
        assert!(HTML.contains("function setVisibleOrder(nextVisible)"));
        assert!(HTML.contains("cap.setPointerCapture(event.pointerId)"));
        assert!(HTML.contains("class=\"capHide\"") || HTML.contains("hide.className=\"capHide\""));
        // `order` carries the hidden modules too, so a drop has to write the
        // rebuilt visible sequence back over the visible positions and leave
        // the hidden ones where they were.
        assert!(HTML.contains("order.map(module=>(enabled[module]?nextVisible[cursor++]:module))"));
        // Bare chrome is the default look: no panel borders, no fill, and a
        // header only under the pointer. A transparent caption that still ate
        // pointer events would silently break the hover readout and the
        // object's orbit across the top of every canvas.
        assert!(HTML.contains("let theme=\"midnight\"") || HTML.contains("chrome=\"bare\""));
        assert!(HTML.contains(".shell[data-chrome=\"bare\"] .box {"));
        assert!(HTML.contains("opacity:0; pointer-events:none; transition:opacity .12s;"));
        assert!(HTML.contains("opacity:1; pointer-events:auto;"));
        assert!(HTML.contains("shell.dataset.chrome=chrome"));
        assert!(HTML.contains("box.style.flexGrow=widths[module]"));
        assert!(HTML.contains("const visible=order.filter(module=>enabled[module])"));
        assert!(!HTML.contains("loudnessHistory"));
        assert!(HTML.contains("id=\"targetChips\""));
        assert!(HTML.contains("const TARGETS=[-24,-23,-16,-14,-9]"));
        assert!(HTML.contains("let enabled=MODULES.map(name=>name!==\"visuals\")"));
        assert!(HTML.contains("isolated from metering · opt in from settings"));
    }

    #[test]
    fn object_orbits_under_mouse_without_cancelling_auto_spin() {
        assert!(HTML.contains("objectCanvas.addEventListener(\"pointerdown\""));
        assert!(HTML.contains("objectYaw+=(event.clientX-objectX)*.012"));
        // Orbit is an offset on top of programme motion, and switching the spin
        // off has to leave the model where it was dragged rather than snapping
        // the accumulated turn back to zero.
        assert!(HTML.contains("(objectSpin?now*.00036:0)+objectYaw"));
        assert!(HTML.contains("function scheduleObjectSpin()"));
        assert!(HTML.contains("uniform float turn, pitch, scatter, size, aspect, zoom;"));
        assert!(HTML.contains("lostpointercapture"));
        // Zoom has to take the gesture from the page: an unprevented trackpad
        // pinch zooms the whole editor, and a plugin window has no way back.
        assert!(HTML.contains("objectCanvas.addEventListener(\"wheel\""));
        assert!(HTML.contains("{passive:false}"));
        assert!(HTML.contains("OBJECT_ZOOM_MIN=0.5, OBJECT_ZOOM_MAX=4"));
    }

    #[test]
    fn frame_work_is_coalesced_and_spectrogram_avoids_pixel_allocations() {
        assert!(HTML.contains("let RENDER_MS=0"));
        assert!(HTML.contains("function refreshRenderRate()"));
        assert!(HTML.contains("requestAnimationFrame"));
        assert!(HTML.contains("const base64Bytes=new Uint8Array(SCOPE_BYTES)"));
        assert!(HTML.contains("function decodeBase64Frame(encoded)"));
        assert!(HTML.contains("const heatLut=new Uint8ClampedArray"));
        assert!(HTML.contains("const fitCache=new WeakMap"));
        assert!(HTML.contains("if(!spectrumHold){ updateSpectrum(); pushSpectrogram(); }"));
        assert!(HTML.contains("new ResizeObserver(resized).observe(row)"));
        assert!(!HTML.contains("history.shift()"));
        assert!(!HTML.contains("function heat(t)"));
    }

    /// The page reads slots the plugin actually publishes. A drifting index
    /// map draws the wrong number confidently, which is worse than a gap.
    #[test]
    fn the_page_and_the_plugin_agree_on_the_metric_slots() {
        for (name, index) in [
            ("m:0", 0),
            ("s:1", 1),
            ("tp:2", 2),
            ("int:10", 10),
            ("lra:11", 11),
            ("plr:12", 12),
        ] {
            assert!(HTML.contains(name), "the page lost slot {name}");
            assert!(
                index < METRICS_END,
                "slot {index} is outside the metric block"
            );
        }
        assert!(
            HTML.contains("waveMin:288")
                && HTML.contains("oscilloscope:416")
                && HTML.contains("spectrum:672")
                && HTML.contains("gonio:768"),
            "the page's block offsets must match the plugin's"
        );
    }

    /// The spectrum axis is a contract split across two files: the plugin
    /// encodes each band as `(dBFS + 120) / 120`, and the page turns that back
    /// into dB to place its grid, its labels and its tilt. Change the encoding
    /// on one side alone and nothing fails — the panel just draws a plausible
    /// plot against the wrong numbers, which is the failure that is hardest to
    /// notice and worst to trust.
    #[test]
    fn the_page_decodes_the_spectrum_exactly_as_the_plugin_encodes_it() {
        let plugin = include_str!("lib.rs");
        assert!(
            plugin.contains("((level + 120.0) / 120.0).clamp(0.0, 1.0)"),
            "the plugin's spectrum encoding moved"
        );
        assert!(
            HTML.contains("SLOT_FLOOR_DB=-120, SLOT_RANGE_DB=120"),
            "the page no longer decodes the plugin's spectrum encoding"
        );
        // The plotted window has to sit inside what the encoding can carry,
        // otherwise part of the panel is reserved for levels that cannot
        // arrive.
        assert!(
            HTML.contains("SPECTRUM_TOP_DB=0, SPECTRUM_FLOOR_DB=-90"),
            "the page's plotted dB window moved"
        );
        // Decade marks are derived, never literal. They used to be hardcoded
        // at 0.24 / 0.5 / 0.79 of the width, and on a 20 Hz - 20 kHz log rail
        // 1 k belongs at 0.566 and 10 k at 0.900.
        assert!(
            HTML.contains("function bandPosition(hz)")
                && HTML.contains("bandPosition(hz)*w"),
            "the frequency axis must be derived from the analyzer's scale"
        );
    }

    #[test]
    fn settings_expose_p1_controls_and_automatable_actions() {
        for id in [
            "themeChips",
            "mapChips",
            "fpsChips",
            "routeChips",
            "fftChips",
            "scaleChips",
            "styleChips",
            "vuChips",
            "oscChips",
            "loudnessChips",
            "spectrogramChips",
            "stereoStyleChips",
            "presetChips",
            "slotChips",
        ] {
            assert!(HTML.contains(id), "missing setting control {id}");
        }
        assert!(HTML.contains("window.__seco_update=params=>"));
        assert!(HTML.contains("post(\"begin \"+index)"));
        assert!(HTML.contains("function saveSlot(index)"));
        assert!(HTML.contains("function loadSlot(index)"));
        assert!(HTML.contains("!/[;\\n\\r]/.test(slot)"));
    }

    /// The sentinel is the contract between the two files: the plugin sends
    /// a very low number for "no programme yet", and the page must treat it
    /// as absent rather than draw it as a reading.
    #[test]
    fn the_page_rejects_the_unmeasured_sentinel() {
        let threshold: f32 = HTML
            .split("UNMEASURED=")
            .nth(1)
            .and_then(|rest| rest.split(';').next())
            .expect("the page must define UNMEASURED")
            .trim()
            .parse()
            .expect("the page's UNMEASURED must be a number");
        assert!(
            crate::UNMEASURED < threshold,
            "the plugin sends {}, which the page would treat as a real value \
             (its threshold is {threshold})",
            crate::UNMEASURED
        );
    }

    /// The GLSL lives inside JavaScript template literals, so a stray
    /// backtick in a shader — a comment quoting an identifier, say — closes
    /// the literal and takes the whole page down with a syntax error that
    /// points nowhere near the cause.
    #[test]
    fn the_shaders_carry_no_backticks() {
        let mut rest = HTML;
        let mut checked = 0;
        while let Some((_, after)) = rest.split_once("=`") {
            let (shader, remainder) = after
                .split_once('`')
                .expect("unterminated template literal");
            if shader.contains("gl_Position") || shader.contains("gl_FragColor") {
                assert!(
                    !shader.contains('`'),
                    "a shader contains a backtick and would close its template literal"
                );
                checked += 1;
            }
            rest = remainder;
        }
        assert_eq!(checked, 2, "expected to find both shaders");
    }

    /// The chips, the state block and the plugin's module list are three
    /// copies of one order. Disagreement silently rearranges a user's layout.
    #[test]
    fn the_page_and_the_state_block_agree_on_the_module_order() {
        let listed = HTML
            .split("const MODULES=[")
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .expect("the page must list its modules");
        for module in crate::settings::MODULES {
            assert!(
                listed.contains(&format!("\"{module}\"")),
                "the page is missing the {module} module"
            );
        }
        assert_eq!(
            listed.matches('"').count() / 2,
            crate::settings::MODULES.len(),
            "the page and the state block list different modules"
        );
    }
}
