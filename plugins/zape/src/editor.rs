//! Zape's editor: the page, and the curves it draws.
//!
//! The framework (`seco-clap`'s `clap.gui`) owns the window, the lifecycle
//! and the parameter plumbing; everything Zape-specific — the markup, the
//! knob, the curve display — lives here.
//!
//! Layout follows the genre convention (Kickstart, Volumeshaper): the rate
//! divisions across the top, one big Mix knob, the active curve drawn large,
//! and the shipped shapes as pickable tiles underneath.

use std::sync::OnceLock;

use seco_core::EditorPage;

use seco_dsp::duck;

/// The editor Zape declares as `Plugin::EDITOR`. Fixed logical size (cocoa
/// is logical-pixel, ext/gui.h:56-57).
pub(crate) const PAGE: EditorPage = EditorPage { html: HTML, width: 760, height: 470 };

/// Resolution of the curves sent to the page. Independent of the audio
/// table's own size: this is drawing, not sound.
const CURVE_POINTS: usize = 256;

/// The shipped shapes sampled into a `window.__seco_shapes([...])` call.
///
/// The points come from the same `duck::tables()` the plugin plays, through
/// the same `gain()` — including the dip entry fade, which is a fixed
/// duration in seconds and therefore a different *width* at every rate.
/// That is why the drawing depends on the Rate parameter: at 1/16 the entry
/// really does take a bigger slice of the cycle than at 1/1.
///
/// Tempo is not available on this path, so the drawing assumes
/// [`REFERENCE_BPM`]; the shape at the session's real tempo differs only in
/// how wide that entry looks.
pub(crate) fn script(params: &[f64]) -> Option<String> {
    let rate = (params.get(crate::PARAM_RATE).copied().unwrap_or(0.0).round().max(0.0) as usize)
        .min(crate::RATE_BEATS.len() - 1);
    // One snippet per rate, built at most once: the adapter asks for this at
    // the editor's refresh rate and only pushes it when it changes.
    static SHAPES: [OnceLock<String>; crate::RATE_BEATS.len()] =
        [const { OnceLock::new() }; crate::RATE_BEATS.len()];
    Some(SHAPES[rate].get_or_init(|| build_shapes(rate)).clone())
}

/// Tempo assumed when drawing. The plugin uses the host's.
const REFERENCE_BPM: f64 = 120.0;

fn build_shapes(rate: usize) -> String {
    let cycle_seconds = crate::RATE_BEATS[rate] * 60.0 / REFERENCE_BPM;
    let attack = (crate::ATTACK_SECONDS / cycle_seconds) as f32;
    let mut shapes = String::from("[");
    for (index, shape) in duck::tables().iter().enumerate() {
        if index > 0 {
            shapes.push(',');
        }
        shapes.push('[');
        for i in 0..=CURVE_POINTS {
            if i > 0 {
                shapes.push(',');
            }
            let phase = (i as f32 / CURVE_POINTS as f32).min(0.999_999);
            shapes.push_str(&format!("{:.3}", shape.gain(phase, attack)));
        }
        shapes.push(']');
    }
    shapes.push(']');
    format!("window.__seco_shapes && window.__seco_shapes({shapes});")
}

/// The page: served from the binary, no files, no network.
///
/// It knows Zape's parameter *order* (the CLAP ids, append-only) but not
/// their content: the rate divisions, the curve names and every displayed
/// value come from what the adapter pushes into `__seco_update`.
const HTML: &str = r##"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<style>
  :root {
    --bg: #131313;
    --panel: #1e1e1e;
    --panel-hi: #2a2a2a;
    --accent: #ffd400;
    --text: #9a9a9a;
  }
  html, body {
    margin: 0; height: 100%; background: var(--bg); color: var(--text);
    font: 500 13px/1.2 -apple-system, sans-serif;
    user-select: none; -webkit-user-select: none;
    -webkit-font-smoothing: antialiased;
  }
  body { display: flex; flex-direction: column; padding: 16px; box-sizing: border-box; gap: 12px; }
  .bar { display: flex; align-items: center; gap: 8px; }
  .brand {
    font: 800 22px/1 -apple-system, sans-serif; letter-spacing: 0.14em;
    color: var(--accent); margin-right: auto;
  }
  .seg { display: flex; gap: 4px; }
  .pill {
    padding: 6px 12px; border-radius: 14px; background: var(--panel);
    color: var(--text); cursor: pointer; font-weight: 700; letter-spacing: 0.04em;
    transition: background 90ms, color 90ms;
  }
  .pill:hover { background: var(--panel-hi); }
  .pill.on { background: var(--accent); color: #101010; }
  .main { display: flex; gap: 16px; flex: 1; min-height: 0; }
  .knobwrap {
    width: 150px; display: flex; flex-direction: column;
    align-items: center; justify-content: center; gap: 8px;
  }
  #knob { cursor: ns-resize; touch-action: none; }
  .knoblabel { font-weight: 700; letter-spacing: 0.1em; font-size: 12px; }
  .knoblabel b { color: var(--accent); }
  .display {
    flex: 1; background: #0b0b0b; border-radius: 10px; padding: 10px;
    box-sizing: border-box; display: flex; min-width: 0;
  }
  #curve { width: 100%; height: 100%; display: block; }
  .shelf { display: flex; flex-direction: column; gap: 6px; }
  .caption {
    display: flex; justify-content: space-between; font-size: 11px;
    letter-spacing: 0.12em; text-transform: uppercase;
  }
  .caption b { color: var(--accent); }
  .tiles { display: grid; grid-template-columns: repeat(8, 1fr); gap: 6px; }
  .tile {
    background: var(--panel); border-radius: 6px; padding: 5px;
    cursor: pointer; transition: background 90ms;
  }
  .tile:hover { background: var(--panel-hi); }
  .tile canvas { width: 100%; height: 30px; display: block; }
  .tile.on { background: var(--accent); }
  body.bypassed .display, body.bypassed .tiles, body.bypassed .knobwrap { opacity: 0.35; }
</style>
</head>
<body>
  <div class="bar">
    <div class="brand">ZAPE</div>
    <div class="seg" id="rate"></div>
    <div class="pill" id="bypass">BYPASS</div>
  </div>
  <div class="main">
    <div class="knobwrap">
      <svg id="knob" width="132" height="132" viewBox="0 0 132 132">
        <circle cx="66" cy="66" r="52" fill="none" stroke="#242424" stroke-width="10"
                stroke-dasharray="245 327" stroke-linecap="round" transform="rotate(135 66 66)"/>
        <circle id="arc" cx="66" cy="66" r="52" fill="none" stroke="#ffd400" stroke-width="10"
                stroke-dasharray="0 327" stroke-linecap="round" transform="rotate(135 66 66)"/>
        <circle cx="66" cy="66" r="38" fill="#1e1e1e"/>
        <circle id="dot" cx="66" cy="34" r="4.5" fill="#ffd400"/>
      </svg>
      <div class="knoblabel">MIX <b id="mixvalue">--</b></div>
    </div>
    <div class="display"><canvas id="curve"></canvas></div>
  </div>
  <div class="shelf">
    <div class="caption"><span>CURVE</span><b id="curvename">--</b></div>
    <div class="tiles" id="tiles"></div>
  </div>
<script>
  // Parameter ids are their index in Plugin::PARAMS (append-only contract).
  const RATE = 0, MIX = 1, CURVE = 2, BYPASS = 3;
  const ACCENT = '#ffd400';

  const post = (m) => window.webkit.messageHandlers.seco.postMessage(m);
  const gesture = (id, plain) => {
    post('begin ' + id); post('set ' + id + ' ' + plain); post('end ' + id);
  };

  // Latest parameter snapshot from the adapter, and the curve tables. Both
  // arrive asynchronously, in either order; every draw tolerates a missing one.
  let params = null, shapes = null, activeMix = 1, dragging = false;

  // ---- curve tables, pushed once by Plugin::editor_script ----------------
  window.__seco_shapes = (tables) => {
    shapes = tables;
    buildTiles();
    draw();
  };

  // ---- parameter values, pushed ~30 Hz ----------------------------------
  window.__seco_update = (list) => {
    params = list;
    buildRates();
    const mix = list[MIX];
    if (mix) {
      document.getElementById('mixvalue').textContent = mix.t;
      // While a drag is live the pointer owns the knob: echoing the value
      // back would fight it.
      if (!dragging) { activeMix = mix.v; setKnob(mix.v); }
    }
    syncSelection();
    draw();
  };

  const step = (p) => Math.round(p.v * Math.max(1, p.opts.length - 1));

  // ---- top bar: one pill per rate division ------------------------------
  function buildRates() {
    const container = document.getElementById('rate');
    const rate = params[RATE];
    if (!rate || container.childElementCount === rate.opts.length) return;
    container.textContent = '';
    rate.opts.forEach((label, index) => {
      const pill = document.createElement('div');
      pill.className = 'pill';
      pill.textContent = label;
      pill.addEventListener('click', () => gesture(RATE, index));
      container.appendChild(pill);
    });
  }

  // ---- bottom: one tile per curve shape ---------------------------------
  function buildTiles() {
    const container = document.getElementById('tiles');
    container.textContent = '';
    shapes.forEach((points, index) => {
      const tile = document.createElement('div');
      tile.className = 'tile';
      const canvas = document.createElement('canvas');
      tile.appendChild(canvas);
      tile.addEventListener('click', () => gesture(CURVE, index));
      container.appendChild(tile);
      // Sized after insertion, so the backing store matches the laid-out box.
      canvas.width = canvas.clientWidth * 2;
      canvas.height = canvas.clientHeight * 2;
      tile.__points = points;
      tile.__canvas = canvas;
    });
    syncSelection();
  }

  function syncSelection() {
    if (!params) return;
    const curve = params[CURVE], rate = params[RATE], bypass = params[BYPASS];
    const tiles = document.getElementById('tiles').children;
    const active = curve ? step(curve) : -1;
    for (let i = 0; i < tiles.length; i++) {
      const on = i === active;
      tiles[i].classList.toggle('on', on);
      if (curve && curve.opts[i]) tiles[i].title = curve.opts[i];
      // Repainted on selection: the grey line is unreadable on yellow.
      const ctx = tiles[i].__canvas.getContext('2d');
      ctx.clearRect(0, 0, tiles[i].__canvas.width, tiles[i].__canvas.height);
      stroke(ctx, tiles[i].__points, 1, on ? '#101010' : '#6a6a6a', 3, false);
    }
    document.getElementById('curvename').textContent = curve ? curve.t : '--';
    const pills = document.getElementById('rate').children;
    for (let i = 0; i < pills.length; i++) {
      pills[i].classList.toggle('on', !!rate && i === step(rate));
    }
    const bypassed = !!bypass && bypass.v >= 0.5;
    document.getElementById('bypass').classList.toggle('on', bypassed);
    document.body.classList.toggle('bypassed', bypassed);
  }

  document.getElementById('bypass').addEventListener('click', () => {
    if (!params) return;
    gesture(BYPASS, params[BYPASS].v >= 0.5 ? 0 : 1);
  });

  // ---- the big display: the curve as the audio applies it ---------------
  function draw() {
    const canvas = document.getElementById('curve');
    canvas.width = canvas.clientWidth * 2;
    canvas.height = canvas.clientHeight * 2;
    const ctx = canvas.getContext('2d');
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    if (!shapes || !params) return;
    // Unity and silence, so the depth of the duck is readable.
    guides(ctx);
    const points = shapes[Math.min(step(params[CURVE]), shapes.length - 1)];
    // Dry shape behind, mixed shape in front: the front line is literally
    // the gain the audio is multiplied by.
    if (activeMix < 0.999) stroke(ctx, points, 1, '#2f2f2f', 3, false);
    stroke(ctx, points, activeMix, ACCENT, 5, true);
  }

  function guides(ctx) {
    ctx.save();
    ctx.strokeStyle = '#1f1f1f';
    ctx.lineWidth = 2;
    ctx.setLineDash([6, 8]);
    for (const norm of [0, 0.5, 1]) {
      const y = plot(ctx, 0, norm).y;
      ctx.beginPath();
      ctx.moveTo(0, y);
      ctx.lineTo(ctx.canvas.width, y);
      ctx.stroke();
    }
    ctx.restore();
  }

  // Maps (phase in [0,1], gain in [0,1]) to canvas pixels. The inset keeps
  // the near-vertical drop at the seam inside the frame instead of half of
  // it being clipped by the left edge.
  function plot(ctx, phase, gain) {
    const pad = 6;
    return {
      x: pad + phase * (ctx.canvas.width - 2 * pad),
      y: pad + (1 - gain) * (ctx.canvas.height - 2 * pad),
    };
  }

  // Draws `points` scaled by `mix` — the plugin's own 1 + (p - 1) * mix.
  function stroke(ctx, points, mix, color, width, fill) {
    ctx.beginPath();
    for (let i = 0; i < points.length; i++) {
      const at = plot(ctx, i / (points.length - 1), 1 + (points[i] - 1) * mix);
      i ? ctx.lineTo(at.x, at.y) : ctx.moveTo(at.x, at.y);
    }
    ctx.strokeStyle = color;
    ctx.lineWidth = width;
    ctx.lineJoin = 'round';
    ctx.lineCap = 'round';
    ctx.stroke();
    if (fill) {
      ctx.lineTo(ctx.canvas.width, ctx.canvas.height);
      ctx.lineTo(0, ctx.canvas.height);
      ctx.closePath();
      ctx.fillStyle = 'rgba(255, 212, 0, 0.08)';
      ctx.fill();
    }
  }

  // ---- knob: 270 degrees of arc, vertical drag --------------------------
  const SWEEP = 245;          // arc length of the full 270-degree track
  const CIRCUMFERENCE = 327;  // 2 * pi * 52

  function setKnob(norm) {
    document.getElementById('arc').setAttribute(
      'stroke-dasharray', (SWEEP * norm) + ' ' + CIRCUMFERENCE);
    const angle = (135 + 270 * norm) * Math.PI / 180;
    const dot = document.getElementById('dot');
    dot.setAttribute('cx', 66 + 32 * Math.cos(angle));
    dot.setAttribute('cy', 66 + 32 * Math.sin(angle));
  }

  const knob = document.getElementById('knob');
  let dragFrom = 0, dragValue = 0;
  knob.addEventListener('pointerdown', (event) => {
    if (!params) return;
    dragging = true;
    dragFrom = event.clientY;
    dragValue = params[MIX].v;
    knob.setPointerCapture(event.pointerId);
    post('begin ' + MIX);
  });
  knob.addEventListener('pointermove', (event) => {
    if (!dragging) return;
    // 200 px of travel spans the range; holding shift is four times finer.
    const scale = event.shiftKey ? 800 : 200;
    const norm = Math.min(1, Math.max(0, dragValue + (dragFrom - event.clientY) / scale));
    activeMix = norm;
    setKnob(norm);
    draw();
    const p = params[MIX];
    post('set ' + MIX + ' ' + (p.min + norm * (p.max - p.min)));
  });
  const endDrag = (event) => {
    if (!dragging) return;
    dragging = false;
    knob.releasePointerCapture(event.pointerId);
    post('end ' + MIX);
  };
  knob.addEventListener('pointerup', endDrag);
  knob.addEventListener('pointercancel', endDrag);

  window.addEventListener('resize', draw);
</script>
</body>
</html>"##;

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes the page to `target/zape-editor.html` with a stand-in for the
    /// adapter (the WebKit bridge and one parameter push), so the layout can
    /// be opened in a browser without a host. Parameter gestures log to the
    /// console instead of reaching the plugin.
    ///
    /// The pushed parameters are derived from `Zape::PARAMS` at their
    /// defaults, not hand-written: a preview that invents its own ranges
    /// stops predicting what the editor does in a DAW.
    ///
    /// `cargo test -p zape -- --ignored dump_editor_preview`
    #[test]
    #[ignore = "dev tool: writes target/zape-editor.html"]
    fn dump_editor_preview() {
        use seco_core::{ParamRange, Plugin};

        let mut list = String::from("[");
        for (index, desc) in crate::Zape::PARAMS.iter().enumerate() {
            let value = desc.range.default_plain();
            let (min, max) = (desc.range.min(), desc.range.max());
            let norm = if max > min { (value - min) / (max - min) } else { 0.0 };
            let (kind, text, opts) = match &desc.range {
                ParamRange::Continuous { unit, decimals, .. } => (
                    "c",
                    format!("{:.*}{unit}", *decimals as usize, value),
                    "[]".to_string(),
                ),
                ParamRange::Stepped { labels, .. } => (
                    "s",
                    labels[value as usize].to_string(),
                    format!("{labels:?}"),
                ),
                ParamRange::Toggle { .. } => (
                    "t",
                    if value >= 0.5 { "On" } else { "Off" }.to_string(),
                    "[]".to_string(),
                ),
            };
            if index > 0 {
                list.push(',');
            }
            list.push_str(&format!(
                "{{n:'{}',t:'{text}',v:{norm},k:'{kind}',min:{min},max:{max},opts:{opts}}}",
                desc.name
            ));
        }
        list.push(']');

        let harness = format!(
            "<script>\n\
             window.webkit = {{ messageHandlers: {{ seco: \
             {{ postMessage: (m) => console.log('POST', m) }} }} }};\n\
             {}\n\
             window.__seco_update({list});\n\
             </script>",
            script(&[]).expect("shapes"),
        );
        let page = HTML.replace("</body>", &format!("{harness}</body>"));
        let out = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/zape-editor.html");
        std::fs::write(out, page).expect("write preview");
        println!("wrote {out}");
    }

    /// Which shape is active is the page's business — the snippet carries
    /// all of them, so mix, curve and bypass must not change it. Rate does:
    /// it changes how wide the dip entry is.
    #[test]
    fn only_the_rate_changes_the_snippet() {
        let a = script(&[2.0, 100.0, 0.0, 0.0]).expect("shapes");
        let b = script(&[2.0, 0.0, 2.0, 1.0]).expect("shapes");
        assert_eq!(a, b);
        let fast = script(&[4.0, 100.0, 0.0, 0.0]).expect("shapes");
        assert_ne!(a, fast, "the entry fade must look wider at a faster rate");
    }

    /// What is drawn is what sounds: the points are what `gain()` returns
    /// at the drawn rate, and every shipped shape is sent.
    #[test]
    fn points_come_from_the_played_shapes() {
        let rate = 2; // 1/4
        let js = script(&[rate as f64, 100.0, 0.0, 0.0]).expect("shapes");
        let attack =
            (crate::ATTACK_SECONDS / (crate::RATE_BEATS[rate] * 60.0 / REFERENCE_BPM)) as f32;
        for shape in &duck::tables() {
            for phase in [0.25_f32, 0.5, 0.75] {
                let expected = format!("{:.3}", shape.gain(phase, attack));
                assert!(js.contains(&expected), "{expected} missing from the drawn curves");
            }
        }
    }

    /// One array per shipped curve — the page indexes it with the Curve
    /// parameter's step, so a missing shape would draw the wrong one.
    #[test]
    fn one_array_per_shape() {
        let js = script(&[2.0, 100.0, 0.0, 0.0]).expect("shapes");
        assert_eq!(js.matches('[').count(), duck::tables().len() + 1);
    }
}
