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

use crate::custom::CustomCurve;
use crate::library;

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
pub(crate) fn script(params: &[f64], state: &[u8]) -> Option<String> {
    let rate = (params.get(crate::PARAM_RATE).copied().unwrap_or(0.0).round().max(0.0) as usize)
        .min(crate::RATE_BEATS.len() - 1);
    // One snippet per rate, built at most once: the adapter asks for this at
    // the editor's refresh rate and only pushes it when it changes.
    static SHAPES: [OnceLock<String>; crate::RATE_BEATS.len()] =
        [const { OnceLock::new() }; crate::RATE_BEATS.len()];
    let shapes = SHAPES[rate].get_or_init(|| build_shapes(rate));

    // The drawn curve rides along, from the same block the audio thread
    // parses. It changes as the user draws, so it cannot be cached — but it
    // is sixteen numbers, and the adapter drops the push when the whole
    // snippet is unchanged.
    let custom = CustomCurve::parse(state).to_wire();
    // The page redraws the drawn curve itself, so it needs the same entry
    // fade width the shipped shapes were sampled with.
    let attack = (crate::ATTACK_SECONDS
        / (crate::RATE_BEATS[rate] * 60.0 / REFERENCE_BPM)) as f32;
    // The skin rides along too: it is a preference read from disk, so the
    // page cannot know it until the plugin says so, and this snippet is
    // already the one the adapter only pushes when something changed.
    Some(format!(
        "{shapes}window.__seco_custom && window.__seco_custom([{custom}], {attack:.5});\
         window.__seco_skin && window.__seco_skin(\"{}\");",
        current_skin()
    ))
}

/// The scope buckets, as a `window.__seco_scope([...])` call.
///
/// Sent every refresh without a change comparison — the point of it is that
/// it changes. Two decimals is more resolution than a waveform a few
/// hundred pixels wide can show, and keeps the push under a kilobyte.
pub(crate) fn frame(scope: &[f32]) -> Option<String> {
    if scope.is_empty() {
        return None;
    }
    let mut points = String::with_capacity(scope.len() * 5);
    for (index, value) in scope.iter().enumerate() {
        if index > 0 {
            points.push(',');
        }
        points.push_str(&format!("{value:.2}"));
    }
    Some(format!("window.__seco_scope && window.__seco_scope([{points}]);"))
}

/// The skins the page ships, and the one used when nothing is remembered.
///
/// Checked here rather than trusted from the page: the value is written to
/// disk and pushed back into a stylesheet selector, so it stays a closed
/// set of identifiers we chose.
const SKINS: [&str; 5] = ["tianguis", "miedo", "azucar", "rockola", "zape"];

/// Loud, gold-on-navy, and the reason this plugin has skins at all.
const DEFAULT_SKIN: &str = "tianguis";

/// The chosen skin, read from disk once and kept here after.
///
/// The editor asks for it at its refresh rate; going to the filesystem
/// thirty times a second to answer would be silly. Main thread only, so the
/// lock is never contended by anything that matters.
static SKIN: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn current_skin() -> String {
    let mut cached = SKIN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if cached.is_none() {
        let stored = library::read_skin().filter(|name| SKINS.contains(&name.as_str()));
        *cached = Some(stored.unwrap_or_else(|| DEFAULT_SKIN.to_owned()));
    }
    cached.clone().unwrap_or_else(|| DEFAULT_SKIN.to_owned())
}

fn choose_skin(name: &str) -> bool {
    if !SKINS.contains(&name) {
        return false;
    }
    let mut cached = SKIN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    *cached = Some(name.to_owned());
    library::write_skin(name)
}

/// Answers a request from the page: the curve library on disk.
///
/// Requests are `list`, `save <name>|<points>` and `delete <name>`. There is
/// deliberately no `load`: the answer to `list` already carries every
/// curve's points, so the page loads one by sending it back as a state
/// block — the single path that also marks the session dirty.
pub(crate) fn message(text: &str) -> Option<String> {
    let (verb, rest) = text.split_once(' ').unwrap_or((text, ""));
    match verb {
        "list" => Some(presets_js()),
        "save" => {
            let (name, points) = rest.split_once('|')?;
            library::save(name, CustomCurve::parse(points.as_bytes()));
            // The list comes back either way: if the save failed, the page
            // shows what is actually on disk rather than what it hoped.
            Some(presets_js())
        }
        "delete" => {
            library::delete(rest);
            Some(presets_js())
        }
        // A skin is a preference, so it is remembered next to the library
        // rather than in the session: it follows the person, not the
        // project. Nothing is echoed back — the page already applied it.
        "skin" => {
            choose_skin(rest);
            None
        }
        _ => None,
    }
}

/// The library as a `window.__seco_presets([...])` call, each entry
/// carrying its points so the page can draw the shape rather than just name
/// it.
fn presets_js() -> String {
    let mut list = String::from("[");
    for (index, entry) in library::list().into_iter().enumerate() {
        if index > 0 {
            list.push(',');
        }
        list.push_str(&format!(
            "{{n:\"{}\",p:[{}]}}",
            escape(&entry.name),
            entry.curve.to_wire()
        ));
    }
    list.push(']');
    format!("window.__seco_presets && window.__seco_presets({list});")
}

/// Names come from a text field and are pasted into a JS string literal.
/// `library::sanitize` already reduced them to letters, digits, space, dash
/// and underscore, so this is the belt to that pair of braces.
fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', " ")
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
  /* ------------------------------------------------------------------ *
   * Structure first, skins second. Every colour a skin can change is a
   * variable here — including the ones the canvas drawing reads back
   * through getComputedStyle, so a skin never has to touch JavaScript.
   * ------------------------------------------------------------------ */
  :root {
    --outline: 3px;
    --radius: 12px;
    --shadow-x: 3px;
    --shadow-y: 3px;
  }
  html, body {
    margin: 0; height: 100%; background: var(--bg); color: var(--text);
    font: var(--font);
    user-select: none; -webkit-user-select: none;
    -webkit-font-smoothing: antialiased;
  }
  body {
    display: flex; flex-direction: column; padding: 16px;
    box-sizing: border-box; gap: 12px; position: relative;
    background-image: var(--bg-image); background-size: var(--bg-size);
  }
  .bar { display: flex; align-items: center; gap: 7px; }
  .brand {
    font: var(--brand-font); letter-spacing: 0.04em; margin-right: auto;
    color: var(--brand-color); -webkit-text-stroke: var(--brand-stroke);
    paint-order: stroke fill; transform: var(--brand-tilt);
    background: var(--brand-fill); -webkit-background-clip: var(--brand-clip);
    -webkit-text-fill-color: var(--brand-fill-color);
  }
  .seg { display: flex; gap: 5px; }
  .pill {
    padding: 7px 12px; border-radius: 999px; background: var(--panel);
    color: var(--text); cursor: pointer; font-weight: 800;
    letter-spacing: 0.03em; border: var(--outline) solid var(--ink);
    box-shadow: var(--shadow-x) var(--shadow-y) 0 var(--shadow);
    transition: transform 60ms, box-shadow 60ms, background 90ms;
  }
  .pill:hover { background: var(--panel-hi); }
  .pill:active {
    transform: translate(var(--shadow-x), var(--shadow-y));
    box-shadow: 0 0 0 var(--shadow);
  }
  .pill.on { background: var(--accent); color: var(--on-accent); }
  .main { display: flex; gap: 16px; flex: 1; min-height: 0; }
  .knobwrap {
    width: 150px; display: flex; flex-direction: column;
    align-items: center; justify-content: center; gap: 9px;
  }
  #knob { cursor: ns-resize; touch-action: none; filter: var(--knob-shadow); }
  #knob .track { stroke: var(--knob-track); }
  #knob .arc { stroke: var(--accent); }
  #knob .body { fill: var(--knob-body); stroke: var(--ink); stroke-width: var(--outline); }
  #knob .dot { fill: var(--accent); stroke: var(--ink); stroke-width: 2; }
  .knoblabel { font-weight: 800; letter-spacing: 0.1em; font-size: 12px; }
  .mascot { display: none; width: 76px; height: 128px; }
  .knoblabel b { color: var(--accent-text); }
  .display {
    flex: 1; background: var(--display-bg); border-radius: var(--radius);
    padding: 10px; box-sizing: border-box; display: flex; min-width: 0;
    border: var(--outline) solid var(--ink);
    box-shadow: var(--shadow-x) var(--shadow-y) 0 var(--shadow);
  }
  #curve { width: 100%; height: 100%; display: block; }
  body.drawable #curve { cursor: crosshair; }
  .shelf { display: flex; flex-direction: column; gap: 6px; }
  .caption {
    display: flex; justify-content: space-between; font-size: 11px;
    letter-spacing: 0.12em; text-transform: uppercase;
  }
  .caption b { color: var(--accent-text); }
  .tiles { display: grid; grid-template-columns: repeat(8, 1fr); gap: 6px; }
  .tile {
    background: var(--panel); border-radius: calc(var(--radius) - 4px);
    padding: 4px; cursor: pointer; border: var(--outline) solid var(--ink);
    box-shadow: var(--shadow-x) var(--shadow-y) 0 var(--shadow);
    transition: transform 60ms, box-shadow 60ms, background 90ms;
  }
  .tile:hover { background: var(--panel-hi); }
  .tile:active {
    transform: translate(var(--shadow-x), var(--shadow-y));
    box-shadow: 0 0 0 var(--shadow);
  }
  .tile canvas { width: 100%; height: 30px; display: block; }
  .tile.on { background: var(--accent); }
  body.bypassed .display, body.bypassed .tiles, body.bypassed .knobwrap { opacity: 0.35; }

  /* The curve library and the skin picker, over everything while open. */
  .browser {
    position: absolute; inset: 12px; z-index: 5; display: none;
    flex-direction: column; gap: 12px; padding: 15px; box-sizing: border-box;
    background: var(--overlay); border-radius: var(--radius);
    border: var(--outline) solid var(--ink);
    box-shadow: 6px 6px 0 var(--shadow);
  }
  body.browsing .browser { display: flex; }
  .browser .head { display: flex; align-items: center; gap: 10px; }
  .browser .head span {
    font-weight: 800; letter-spacing: 0.12em; font-size: 12px;
    margin-right: auto; color: var(--accent-text);
  }
  .cards {
    display: grid; grid-template-columns: repeat(4, 1fr); gap: 9px;
    overflow-y: auto; flex: 1; align-content: start;
  }
  .card {
    position: relative; background: var(--panel); padding: 6px;
    border-radius: calc(var(--radius) - 4px); cursor: pointer;
    border: var(--outline) solid var(--ink);
    box-shadow: var(--shadow-x) var(--shadow-y) 0 var(--shadow);
  }
  .card:hover { background: var(--panel-hi); }
  .card canvas { width: 100%; height: 42px; display: block; }
  .card .label {
    display: block; font-size: 11px; margin-top: 3px; text-align: center;
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
  .card .kill {
    position: absolute; top: -9px; right: -7px; font-size: 11px; line-height: 1;
    background: var(--panel); border: var(--outline) solid var(--ink);
    border-radius: 999px; padding: 2px 6px;
  }
  .card .kill.armed { background: #e23b3b; color: #fff; }
  .empty { opacity: 0.6; font-size: 12px; padding: 8px 2px; }
  .saverow { display: flex; gap: 9px; }
  .saverow input {
    flex: 1; background: var(--field); border: var(--outline) solid var(--ink);
    border-radius: 999px; color: var(--text); padding: 8px 13px;
    font: inherit; outline: none;
  }

  /* Skin picker: same overlay family, anchored under the bar. */
  .skins {
    position: absolute; top: 58px; right: 16px; z-index: 6; display: none;
    flex-direction: column; gap: 7px; padding: 12px; box-sizing: border-box;
    background: var(--overlay); border-radius: var(--radius);
    border: var(--outline) solid var(--ink);
    box-shadow: 6px 6px 0 var(--shadow); min-width: 190px;
  }
  body.skinning .skins { display: flex; }
  .skinrow {
    display: flex; align-items: center; gap: 9px; cursor: pointer;
    padding: 6px 9px; border-radius: 999px; border: var(--outline) solid transparent;
    font-weight: 800; font-size: 12px; letter-spacing: 0.04em;
  }
  .skinrow:hover { background: var(--panel); }
  .skinrow.on { background: var(--panel); border-color: var(--ink); }
  .swatch { display: flex; gap: 3px; }
  .swatch i {
    width: 11px; height: 11px; border-radius: 3px; display: block;
    border: 2px solid var(--ink); box-sizing: border-box;
  }

  /* ------------------------------------------------------------------ *
   * Skins. Palettes and attitude only — no marks, no characters, no
   * badges. The names are ours.
   * ------------------------------------------------------------------ */

  /* Tianguis: the burned-DVD-cover look. Chrome bevels, gold on navy,
     glossy buttons and a sparkle or two. Loud on purpose. */
  body[data-skin="tianguis"] {
    --font: 800 13px/1.2 "Avenir Next", "Futura", -apple-system, sans-serif;
    --bg: #0a1f4d; --panel: linear-gradient(#3f6dbf, #14275c);
    --panel-hi: linear-gradient(#5b8ede, #1b3a7a);
    --ink: #041030; --shadow: #041030; --accent: #ffcf3f; --accent-text: #ffd964;
    --on-accent: #2a1c00; --text: #eaf2ff; --overlay: #0d2559; --field: #081a42;
    --display-bg: #050f2b; --knob-track: #1b3a7a; --knob-body: #12295e;
    --knob-shadow: drop-shadow(3px 4px 0 #041030);
    --brand-font: 900 italic 32px/1 "Avenir Next", sans-serif;
    --brand-color: #ffd964; --brand-stroke: 4px #041030; --brand-tilt: rotate(-4deg) skewX(-6deg);
    --brand-fill: linear-gradient(#ffffff 0%, #fff2b0 38%, #e5a800 52%, #ffffff 100%);
    --brand-clip: text; --brand-fill-color: transparent;
    --bg-image:
      radial-gradient(circle at 12% 18%, #ffffff22 0 2px, transparent 3px),
      radial-gradient(circle at 78% 8%, #ffffff1f 0 2px, transparent 3px),
      radial-gradient(circle at 92% 72%, #ffffff1a 0 2px, transparent 3px),
      radial-gradient(circle at 30% 88%, #ffffff14 0 2px, transparent 3px),
      linear-gradient(160deg, #14387f 0%, #0a1f4d 55%, #061634 100%);
    --bg-size: auto;
    --curve: #ffcf3f; --curve-fill: rgba(255, 207, 63, 0.18);
    --dry-line: #35508f; --guide: #1d3572;
    --wave-dry: rgba(190, 214, 255, 0.16); --wave-wet: rgba(226, 238, 255, 0.5);
    --tile-line: #cddcff; --tile-line-on: #2a1c00;
  }
  /* Everything below is the 2008 part: a gloss highlight on the top half
     of every control, a bevel under it, and a plaque behind the header —
     the grammar of a fan page from when tables had borders. */
  body[data-skin="tianguis"] .pill,
  body[data-skin="tianguis"] .tile,
  body[data-skin="tianguis"] .card,
  body[data-skin="tianguis"] .saverow input {
    background-image: linear-gradient(#ffffff55, #ffffff10 46%, #00000022 54%, #00000033);
    box-shadow: var(--shadow-x) var(--shadow-y) 0 var(--shadow),
                inset 0 1px 0 #ffffff70, inset 0 -2px 4px #00000055;
  }
  body[data-skin="tianguis"] .pill.on,
  body[data-skin="tianguis"] .tile.on {
    background-image: linear-gradient(#fff6c8, #ffcf3f 46%, #e0a800 54%, #ffe08a);
  }
  body[data-skin="tianguis"] .bar {
    background: linear-gradient(#4d7fd6, #16306e 48%, #0d2154 52%, #2a4f9e);
    border: 3px solid var(--ink); border-radius: 999px;
    padding: 6px 10px; box-shadow: 3px 3px 0 var(--ink), inset 0 1px 0 #ffffff66;
  }
  body[data-skin="tianguis"] .brand {
    /* The shadow has to be a filter, not text-shadow: the letters are a
       clipped gradient, so a text-shadow paints *through* them. */
    filter: drop-shadow(0 3px 0 #00112f) drop-shadow(0 0 7px #7fb0ff66);
  }
  body[data-skin="tianguis"] .display {
    position: relative; overflow: hidden;
    box-shadow: var(--shadow-x) var(--shadow-y) 0 var(--shadow),
                inset 0 2px 10px #000000cc, 0 0 0 2px #4d7fd6 inset;
  }
  /* The glass reflection over the screen. Never in the way of the pointer:
     the curve underneath is draggable. */
  body[data-skin="tianguis"] .display::after {
    content: ""; position: absolute; inset: 0; pointer-events: none;
    background: linear-gradient(105deg, #ffffff1c 0 34%, transparent 36%),
                radial-gradient(120% 60% at 50% -20%, #9fc4ff26, transparent 70%);
  }
  body[data-skin="tianguis"] .mascot { display: block; }
  body[data-skin="tianguis"] .mascot .cap { fill: #e8b53a; stroke: var(--ink); stroke-width: 3; }
  body[data-skin="tianguis"] .mascot .glass { fill: #7a4a12; stroke: var(--ink); stroke-width: 3; }
  body[data-skin="tianguis"] .mascot .shine { fill: #ffffff44; }
  body[data-skin="tianguis"] .mascot .label {
    fill: #f6efdc; stroke: var(--ink); stroke-width: 3;
  }
  body[data-skin="tianguis"] .mascot .labeltext {
    fill: #0a1f4d; font: 900 italic 9px "Avenir Next", sans-serif;
    letter-spacing: 0.04em;
  }
  body[data-skin="tianguis"] .mascot .sparkles { fill: #ffe9a8; }
  body[data-skin="tianguis"] .skinrow.on,
  body[data-skin="tianguis"] .browser,
  body[data-skin="tianguis"] .skins {
    background-image: linear-gradient(#ffffff18, #00000022);
  }

  /* Miedo: sickly farmhouse palette — bruised purple, sour teal, dusty
     pink. Thin nervous outlines, everything slightly off-square. */
  body[data-skin="miedo"] {
    --font: 700 13px/1.2 "Avenir Next", -apple-system, sans-serif;
    --outline: 2px; --radius: 10px; --shadow-x: 4px; --shadow-y: 4px;
    --bg: #241a33; --panel: #372a4a; --panel-hi: #453458;
    --ink: #0f0a17; --shadow: #0f0a17; --accent: #4fb3a5; --accent-text: #7fd8cb;
    --on-accent: #0f0a17; --text: #ded3e8; --overlay: #2c2140; --field: #1b1428;
    --display-bg: #150f20; --knob-track: #453458; --knob-body: #2b2040;
    --knob-shadow: drop-shadow(4px 4px 0 #0f0a17);
    --brand-font: 900 30px/1 "Avenir Next", sans-serif;
    --brand-color: #d98cae; --brand-stroke: 3px #0f0a17; --brand-tilt: rotate(-2deg) skewY(-2deg);
    --brand-fill: none; --brand-clip: border-box; --brand-fill-color: currentColor;
    --bg-image: radial-gradient(#ffffff0a 1px, transparent 1.2px); --bg-size: 7px 7px;
    --curve: #4fb3a5; --curve-fill: rgba(79, 179, 165, 0.16);
    --dry-line: #4a3a5e; --guide: #322544;
    --wave-dry: rgba(217, 140, 174, 0.15); --wave-wet: rgba(232, 216, 240, 0.42);
    --tile-line: #b9a7c9; --tile-line-on: #0f0a17;
  }
  body[data-skin="miedo"] .tile:nth-child(odd) { transform: rotate(-0.8deg); }
  body[data-skin="miedo"] .tile:nth-child(even) { transform: rotate(0.6deg); }

  /* Azúcar: candy pinks and mints over hard black ink, the way a
     Saturday-morning show fills a screen. */
  body[data-skin="azucar"] {
    --font: 800 13px/1.2 "Avenir Next", -apple-system, sans-serif;
    --outline: 3px; --radius: 16px;
    --bg: #ffe3f1; --panel: #ffffff; --panel-hi: #ffd0e6;
    --ink: #17121a; --shadow: #17121a; --accent: #ff4fa3; --accent-text: #d81f77;
    --on-accent: #ffffff; --text: #2b2230; --overlay: #fff6fb; --field: #ffffff;
    --display-bg: #1a1420; --knob-track: #ffd0e6; --knob-body: #ffffff;
    --knob-shadow: drop-shadow(4px 4px 0 #17121a);
    --brand-font: 900 33px/1 "Avenir Next", sans-serif;
    --brand-color: #ff4fa3; --brand-stroke: 5px #17121a; --brand-tilt: rotate(-3deg);
    --brand-fill: none; --brand-clip: border-box; --brand-fill-color: currentColor;
    --bg-image:
      radial-gradient(circle at 20% 30%, #8ef0d055 0 60px, transparent 61px),
      radial-gradient(circle at 85% 70%, #7ec8ff55 0 70px, transparent 71px),
      radial-gradient(#17121a14 1.4px, transparent 1.5px);
    --bg-size: auto, auto, 10px 10px;
    --curve: #ff4fa3; --curve-fill: rgba(255, 79, 163, 0.2);
    --dry-line: #4b3d55; --guide: #2e2536;
    --wave-dry: rgba(142, 240, 208, 0.22); --wave-wet: rgba(126, 200, 255, 0.55);
    --tile-line: #17121a; --tile-line-on: #ffffff;
  }

  /* Rockola: diner black, banana yellow and orange, chrome-free and
     unapologetically loud about it. */
  body[data-skin="rockola"] {
    --font: 800 13px/1.2 "Avenir Next", "Futura", -apple-system, sans-serif;
    --outline: 4px; --radius: 14px; --shadow-x: 4px; --shadow-y: 4px;
    --bg: #14110c; --panel: #24201a; --panel-hi: #322c22;
    --ink: #000000; --shadow: #ff7a1a; --accent: #ffd400; --accent-text: #ffd400;
    --on-accent: #14110c; --text: #f5ead2; --overlay: #1c1813; --field: #0e0c08;
    --display-bg: #0b0906; --knob-track: #322c22; --knob-body: #24201a;
    --knob-shadow: drop-shadow(4px 4px 0 #ff7a1a);
    --brand-font: 900 italic 33px/1 "Avenir Next", sans-serif;
    --brand-color: #ffd400; --brand-stroke: 5px #000000; --brand-tilt: rotate(-4deg);
    --brand-fill: none; --brand-clip: border-box; --brand-fill-color: currentColor;
    --bg-image:
      repeating-linear-gradient(45deg, #ffffff08 0 10px, transparent 10px 20px);
    --bg-size: auto;
    --curve: #ffd400; --curve-fill: rgba(255, 122, 26, 0.18);
    --dry-line: #55492f; --guide: #2a2419;
    --wave-dry: rgba(245, 234, 210, 0.14); --wave-wet: rgba(255, 200, 120, 0.45);
    --tile-line: #d8c9a4; --tile-line-on: #14110c;
  }

  /* Zape: what the plugin looked like before it had skins. */
  body[data-skin="zape"] {
    --font: 500 13px/1.2 -apple-system, sans-serif;
    --outline: 0px; --radius: 10px; --shadow-x: 0px; --shadow-y: 0px;
    --bg: #131313; --panel: #1e1e1e; --panel-hi: #2a2a2a;
    --ink: transparent; --shadow: transparent; --accent: #ffd400;
    --accent-text: #ffd400; --on-accent: #101010; --text: #9a9a9a;
    --overlay: #0e0e0e; --field: #1e1e1e;
    --display-bg: #0b0b0b; --knob-track: #242424; --knob-body: #1e1e1e;
    --knob-shadow: none;
    --brand-font: 800 22px/1 -apple-system, sans-serif;
    --brand-color: #ffd400; --brand-stroke: 0; --brand-tilt: none;
    --brand-fill: none; --brand-clip: border-box; --brand-fill-color: currentColor;
    --bg-image: none; --bg-size: auto;
    --curve: #ffd400; --curve-fill: rgba(255, 212, 0, 0.08);
    --dry-line: #2f2f2f; --guide: #1f1f1f;
    --wave-dry: rgba(190, 190, 190, 0.13); --wave-wet: rgba(225, 225, 225, 0.42);
    --tile-line: #6a6a6a; --tile-line-on: #101010;
  }
</style>
</head>
<body>
  <div class="bar">
    <div class="brand">ZAPE</div>
    <div class="seg" id="rate"></div>
    <div class="pill" id="curves">CURVES</div>
    <div class="pill" id="skin-button">SKIN</div>
    <div class="pill" id="bypass">BYPASS</div>
  </div>
  <div class="skins" id="skins"></div>
  <div class="browser">
    <div class="head">
      <span>CURVE LIBRARY</span>
      <div class="pill" id="browser-close">CLOSE</div>
    </div>
    <div class="cards" id="cards"></div>
    <div class="saverow">
      <input id="preset-name" maxlength="48" placeholder="name this curve" spellcheck="false">
      <div class="pill" id="preset-save">SAVE</div>
    </div>
  </div>
  <div class="main">
    <div class="knobwrap">
      <svg id="knob" width="132" height="132" viewBox="0 0 132 132">
        <circle class="track" cx="66" cy="66" r="52" fill="none" stroke-width="10"
                stroke-dasharray="245 327" stroke-linecap="round" transform="rotate(135 66 66)"/>
        <circle class="arc" id="arc" cx="66" cy="66" r="52" fill="none" stroke-width="10"
                stroke-dasharray="0 327" stroke-linecap="round" transform="rotate(135 66 66)"/>
        <circle class="body" cx="66" cy="66" r="38"/>
        <circle class="dot" id="dot" cx="66" cy="34" r="5"/>
      </svg>
      <div class="knoblabel">MIX <b id="mixvalue">--</b></div>
      <!-- Decoration, drawn inline so the binary stays the whole plugin.
           Only one skin shows it; the others collapse it to nothing. -->
      <svg class="mascot" viewBox="0 0 62 104" aria-hidden="true">
        <g class="sparkles">
          <path d="M6 18 L8 24 L14 26 L8 28 L6 34 L4 28 L-2 26 L4 24 Z"/>
          <path d="M54 8 L56 13 L61 15 L56 17 L54 22 L52 17 L47 15 L52 13 Z"/>
          <path d="M56 74 L57.5 78 L61.5 79.5 L57.5 81 L56 85 L54.5 81 L50.5 79.5 L54.5 78 Z"/>
        </g>
        <rect class="cap" x="23" y="2" width="16" height="9" rx="2"/>
        <path class="glass" d="M26 11 h10 v14 q14 6 14 20 v49 q0 8 -8 8 h-22 q-8 0 -8 -8 v-49
                               q0 -14 14 -20 z"/>
        <path class="shine" d="M24 28 q-6 5 -6 17 v44 q0 4 3 4 q3 0 3 -4 v-44 q0 -12 6 -17 z"/>
        <rect class="label" x="14" y="58" width="34" height="26" rx="3"/>
        <text class="labeltext" x="31" y="75" text-anchor="middle">ZAPE</text>
      </svg>
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
  // The drawn curve is the last step of the Curve parameter.
  const CURVE_CUSTOM = 15;

  // Skins are CSS: the canvas reads its colours back out of the same
  // variables the stylesheet sets, so a new skin never touches this file.
  const SKINS = [
    { id: 'tianguis', label: 'TIANGUIS', dots: ['#0a1f4d', '#ffcf3f', '#cddcff'] },
    { id: 'miedo', label: 'MIEDO', dots: ['#241a33', '#4fb3a5', '#d98cae'] },
    { id: 'azucar', label: 'AZUCAR', dots: ['#ffe3f1', '#ff4fa3', '#8ef0d0'] },
    { id: 'rockola', label: 'ROCKOLA', dots: ['#14110c', '#ffd400', '#ff7a1a'] },
    { id: 'zape', label: 'ZAPE', dots: ['#131313', '#ffd400', '#9a9a9a'] },
  ];
  const ink = (name) =>
    getComputedStyle(document.body).getPropertyValue('--' + name).trim();

  const post = (m) => window.webkit.messageHandlers.seco.postMessage(m);
  const gesture = (id, plain) => {
    post('begin ' + id); post('set ' + id + ' ' + plain); post('end ' + id);
  };

  // Latest parameter snapshot from the adapter, and the curve tables. Both
  // arrive asynchronously, in either order; every draw tolerates a missing one.
  let params = null, shapes = null, activeMix = 1, dragging = false;
  // The drawn curve: control points, the entry-fade width to draw it with,
  // and whether the pointer is currently editing it.
  let customPoints = null, customAttack = 0, drawing = false, lastSent = 0;
  // Peak input level per bucket, published by the audio thread and drawn
  // behind the curve. Uncoordinated by design — see RtContext::set_scope.
  let scope = null;
  // Repainting sixteen tile canvases at the refresh rate is pointless; they
  // only change when the selection or the drawn curve does.
  let paintedSelection = -1;

  // ---- curve tables, pushed by Plugin::editor_script ---------------------
  window.__seco_shapes = (tables) => {
    shapes = tables;
    buildTiles();
    draw();
  };

  // The drawn curve, from the same state block the audio thread parses.
  // Ignored mid-drag: while the pointer owns it, the page is the authority
  // and the echo would fight it.
  window.__seco_custom = (points, attack) => {
    customAttack = attack;
    if (drawing) return;
    customPoints = points;
    paintedSelection = -1;
    draw();
  };

  // Pushed every refresh: the picture of the audio going through.
  window.__seco_scope = (buckets) => {
    scope = buckets;
    draw();
  };

  const isCustom = () => params && step(params[CURVE]) === CURVE_CUSTOM;

  // The plugin's own interpolation and dip-entry fade, so the line on screen
  // is the gain the audio gets — see custom.rs and DuckShape::gain.
  function customGain(phase) {
    const count = customPoints.length;
    const scaled = Math.min(Math.max(phase, 0), 1) * (count - 1);
    const index = Math.min(Math.floor(scaled), count - 2);
    const blend = 0.5 - 0.5 * Math.cos(Math.PI * (scaled - index));
    const value = customPoints[index] + (customPoints[index + 1] - customPoints[index]) * blend;
    const window = Math.min(customAttack, 0.5);
    const remaining = 1 - phase;
    if (window <= 0 || remaining >= window) return value;
    return value * (0.5 - 0.5 * Math.cos(Math.PI * remaining / window));
  }

  function sampleCustom() {
    if (!customPoints) return null;
    const points = [];
    for (let i = 0; i <= 256; i++) points.push(customGain(Math.min(i / 256, 0.999999)));
    return points;
  }

  // shapes[] carries the shipped curves; the drawn one is appended.
  const shapeAt = (index) => (index === CURVE_CUSTOM ? sampleCustom() : shapes[index]);

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
    // One per shipped shape, plus the drawn one.
    for (let index = 0; index <= CURVE_CUSTOM; index++) {
      const tile = document.createElement('div');
      tile.className = 'tile';
      const canvas = document.createElement('canvas');
      tile.appendChild(canvas);
      tile.addEventListener('click', () => gesture(CURVE, index));
      container.appendChild(tile);
      // Sized after insertion, so the backing store matches the laid-out box.
      canvas.width = canvas.clientWidth * 2;
      canvas.height = canvas.clientHeight * 2;
      tile.__canvas = canvas;
    }
    paintedSelection = -1;
    syncSelection();
  }

  function paintTiles(active) {
    const tiles = document.getElementById('tiles').children;
    for (let i = 0; i < tiles.length; i++) {
      const points = shapeAt(i);
      const ctx = tiles[i].__canvas.getContext('2d');
      ctx.clearRect(0, 0, tiles[i].__canvas.width, tiles[i].__canvas.height);
      // The grey line is unreadable on the selected tile's yellow.
      if (points) stroke(ctx, points, 1, ink(i === active ? 'tile-line-on' : 'tile-line'), 3, false);
    }
  }

  function syncSelection() {
    if (!params) return;
    const curve = params[CURVE], rate = params[RATE], bypass = params[BYPASS];
    const active = curve ? step(curve) : -1;
    const tiles = document.getElementById('tiles').children;
    for (let i = 0; i < tiles.length; i++) {
      tiles[i].classList.toggle('on', i === active);
      if (curve && curve.opts[i]) tiles[i].title = curve.opts[i];
    }
    if (active !== paintedSelection) {
      paintTiles(active);
      paintedSelection = active;
    }
    document.getElementById('curvename').textContent = curve ? curve.t : '--';
    const pills = document.getElementById('rate').children;
    for (let i = 0; i < pills.length; i++) {
      pills[i].classList.toggle('on', !!rate && i === step(rate));
    }
    const bypassed = !!bypass && bypass.v >= 0.5;
    document.getElementById('bypass').classList.toggle('on', bypassed);
    document.body.classList.toggle('bypassed', bypassed);
    document.body.classList.toggle('drawable', isCustom() && !bypassed);
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
    if (!params) return;
    // Unity and silence, so the depth of the duck is readable.
    guides(ctx);
    // The audio first, so the curve is drawn over what it acts on.
    if (scope) waveform(ctx);
    const points = shapeAt(Math.min(step(params[CURVE]), CURVE_CUSTOM));
    if (!points) return;
    // Dry shape behind, mixed shape in front: the front line is literally
    // the gain the audio is multiplied by.
    if (activeMix < 0.999) stroke(ctx, points, 1, ink('dry-line'), 3, false);
    stroke(ctx, points, activeMix, ink('curve'), 5, true);
    if (isCustom() && customPoints) handles(ctx);
  }

  // The grab points, only where they can be grabbed: the first is the beat
  // and the beat is silent, so it is drawn hollow and cannot be moved.
  function handles(ctx) {
    for (let i = 0; i < customPoints.length; i++) {
      const at = plot(ctx, i / (customPoints.length - 1),
                      1 + (customPoints[i] - 1) * activeMix);
      ctx.beginPath();
      ctx.arc(at.x, at.y, 7, 0, Math.PI * 2);
      ctx.fillStyle = i === 0 ? ink('display-bg') : ink('curve');
      ctx.strokeStyle = ink('curve');
      ctx.lineWidth = 3;
      ctx.fill();
      ctx.stroke();
    }
  }

  // Two waveforms, mirrored around the middle so they read as audio rather
  // than as a bar chart: what arrived, and what is leaving. The duck is the
  // gap between them — drawing only the input made the one thing the plugin
  // does invisible. Beat-aligned: bucket i is phase i / count, the same axis
  // as the curve above.
  function waveform(ctx) {
    const half = scope.length / 2;
    // Dry first, as a dim ghost, so the ducked signal sits inside it.
    band(ctx, scope.slice(0, half), ink('wave-dry'));
    band(ctx, scope.slice(half), ink('wave-wet'));
  }

  function band(ctx, values, fill) {
    const middle = ctx.canvas.height / 2;
    const scale = ctx.canvas.height * 0.46;
    ctx.beginPath();
    for (let i = 0; i < values.length; i++) {
      const x = (i / (values.length - 1)) * ctx.canvas.width;
      ctx.lineTo(x, middle - values[i] * scale);
    }
    for (let i = values.length - 1; i >= 0; i--) {
      const x = (i / (values.length - 1)) * ctx.canvas.width;
      ctx.lineTo(x, middle + values[i] * scale);
    }
    ctx.closePath();
    ctx.fillStyle = fill;
    ctx.fill();
  }

  function guides(ctx) {
    ctx.save();
    ctx.strokeStyle = ink('guide');
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
      ctx.fillStyle = ink('curve-fill');
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

  // ---- skins ------------------------------------------------------------
  // The plugin remembers the choice on disk (a preference, not session
  // state) and pushes it back with the shapes; the page only has to apply
  // it and repaint, because every colour lives in CSS.
  window.__seco_skin = (id) => {
    if (document.body.dataset.skin === id) return;
    document.body.dataset.skin = id;
    buildSkinList();
    // Tiles are painted, not styled: their canvases hold the old skin's
    // colours until something repaints them, and the parameter push that
    // would do it a tick later is too slow to look deliberate.
    paintedSelection = -1;
    if (params) syncSelection();
    draw();
  };

  function buildSkinList() {
    const panel = document.getElementById('skins');
    panel.textContent = '';
    SKINS.forEach((skin) => {
      const row = document.createElement('div');
      row.className = 'skinrow' + (document.body.dataset.skin === skin.id ? ' on' : '');
      const swatch = document.createElement('div');
      swatch.className = 'swatch';
      skin.dots.forEach((colour) => {
        const dot = document.createElement('i');
        dot.style.background = colour;
        swatch.appendChild(dot);
      });
      const label = document.createElement('span');
      label.textContent = skin.label;
      row.appendChild(swatch);
      row.appendChild(label);
      row.addEventListener('click', () => {
        window.__seco_skin(skin.id);
        post('msg skin ' + skin.id);
        document.body.classList.remove('skinning');
      });
      panel.appendChild(row);
    });
  }

  document.getElementById('skin-button').addEventListener('click', () => {
    buildSkinList();
    document.body.classList.toggle('skinning');
  });

  // ---- the curve library ------------------------------------------------
  // Saved curves live on disk; the plugin answers `msg` requests with the
  // whole list, points included, so a card can draw the shape rather than
  // just name it. Loading one sends it straight back as a state block —
  // the same path the drawing uses, so the session is marked dirty and the
  // curve travels with the project whether or not the library does.
  const browser = document.querySelector('.browser');

  window.__seco_presets = (list) => {
    const cards = document.getElementById('cards');
    cards.textContent = '';
    if (!list.length) {
      const empty = document.createElement('div');
      empty.className = 'empty';
      empty.textContent = 'Nothing saved yet. Draw a curve, name it, hit SAVE.';
      cards.appendChild(empty);
      return;
    }
    list.forEach((preset) => {
      const card = document.createElement('div');
      card.className = 'card';
      const canvas = document.createElement('canvas');
      const label = document.createElement('span');
      label.className = 'label';
      label.textContent = preset.n;
      const kill = document.createElement('span');
      kill.className = 'kill';
      kill.textContent = '\u00d7';
      card.appendChild(canvas);
      card.appendChild(label);
      card.appendChild(kill);
      cards.appendChild(card);

      canvas.width = canvas.clientWidth * 2;
      canvas.height = canvas.clientHeight * 2;
      // The saved shape, drawn with the same sampling as the big display.
      const saved = customPoints;
      customPoints = preset.p;
      stroke(canvas.getContext('2d'), sampleCustom(), 1, ink('tile-line'), 3, false);
      customPoints = saved;

      card.addEventListener('click', () => {
        customPoints = preset.p.slice();
        paintedSelection = -1;
        draw();
        sendCustom(true);
        // Loading a curve means using it.
        gesture(CURVE, CURVE_CUSTOM);
        document.getElementById('preset-name').value = preset.n;
      });

      // Two clicks to delete: no confirm() — a webview dialog with no
      // delegate behind it goes nowhere.
      kill.addEventListener('click', (event) => {
        event.stopPropagation();
        if (!kill.classList.contains('armed')) {
          kill.classList.add('armed');
          kill.textContent = 'sure?';
          setTimeout(() => {
            kill.classList.remove('armed');
            kill.textContent = '\u00d7';
          }, 2500);
          return;
        }
        post('msg delete ' + preset.n);
      });
    });
  };

  document.getElementById('curves').addEventListener('click', () => {
    document.body.classList.remove('skinning');
    document.body.classList.add('browsing');
    post('msg list');
  });
  document.getElementById('browser-close').addEventListener('click', () => {
    document.body.classList.remove('browsing');
  });
  document.getElementById('preset-save').addEventListener('click', () => {
    const field = document.getElementById('preset-name');
    const name = field.value.trim();
    if (!name || !customPoints) return;
    post('msg save ' + name + '|' + customPoints.map((p) => p.toFixed(3)).join(','));
  });
  document.getElementById('preset-name').addEventListener('keydown', (event) => {
    if (event.key === 'Enter') document.getElementById('preset-save').click();
  });

  // ---- drawing the custom curve -----------------------------------------
  // Sweeping paints: the point nearest the pointer follows it, so a drag
  // across the display draws a shape rather than moving one handle.
  const display = document.getElementById('curve');

  function paint(event) {
    const box = display.getBoundingClientRect();
    const x = (event.clientX - box.left) / box.width;
    const y = 1 - (event.clientY - box.top) / box.height;
    const count = customPoints.length;
    const index = Math.min(count - 1, Math.max(1, Math.round(x * (count - 1))));
    customPoints[index] = Math.min(1, Math.max(0, y));
    paintedSelection = -1;
    draw();
    sendCustom(false);
  }

  // The block goes to the plugin (and marks the session dirty), so a drag
  // is coalesced rather than sent per pointer event.
  function sendCustom(force) {
    const now = performance.now();
    if (!force && now - lastSent < 80) return;
    lastSent = now;
    post('state ' + customPoints.map((p) => p.toFixed(3)).join(','));
  }

  display.addEventListener('pointerdown', (event) => {
    if (!isCustom() || !customPoints) return;
    drawing = true;
    display.setPointerCapture(event.pointerId);
    paint(event);
  });
  display.addEventListener('pointermove', (event) => {
    if (drawing) paint(event);
  });
  const endDraw = (event) => {
    if (!drawing) return;
    drawing = false;
    display.releasePointerCapture(event.pointerId);
    sendCustom(true);
  };
  display.addEventListener('pointerup', endDraw);
  display.addEventListener('pointercancel', endDraw);

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
            // Preview the drawn curve: it is the only mode with an
            // interaction the layout has to make room for.
            let value = if index == crate::PARAM_CURVE {
                crate::CUSTOM_CURVE as f64
            } else {
                desc.range.default_plain()
            };
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
            script(&[2.0, 100.0, crate::CUSTOM_CURVE as f64, 0.0], b"").expect("shapes"),
        );
        // A plausible envelope, so the preview shows the audio picture the
        // way a host would drive it.
        let half = seco_clap::SCOPE_BUCKETS / 2;
        let mut scope = [0.0_f32; seco_clap::SCOPE_BUCKETS];
        // The preview selects the drawn curve, so duck the mock signal with
        // that one — otherwise the picture and the line disagree.
        let shape = CustomCurve::default().shape();
        for index in 0..half {
            let phase = index as f32 / half as f32;
            let level = (0.35 + 0.5 * (phase * 24.0).sin().abs()) * (1.0 - phase * 0.35);
            scope[index] = level;
            // What leaves: the same signal through the curve.
            scope[half + index] = level * shape.gain(phase, 0.01);
        }
        let harness = format!(
            "{harness}<script>{}</script>",
            frame(&scope).expect("frame")
        );
        // A library with a couple of curves, and the browser open, so the
        // preview shows the part with an interaction to check.
        let mock = "<script>\n\
             document.body.classList.add('browsing');\n\
             window.__seco_presets([\n\
             {n:'kick 4x4',p:[0,0.1,0.3,0.5,0.7,0.85,0.95,1,1,1,1,1,1,1,1,1]},\n\
             {n:'reverse swell',p:[0,0.9,0.8,0.7,0.6,0.5,0.45,0.5,0.6,0.7,0.8,0.9,1,1,1,1]},\n\
             {n:'gate 8th',p:[0,0,0,0,1,1,1,1,0,0,0,0,1,1,1,1]}\n\
             ]);\n\
             </script>";
        let harness = format!("{harness}{mock}");
        let page = HTML.replace("</body>", &format!("{harness}</body>"));
        let out = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/zape-editor.html");
        std::fs::write(out, page).expect("write preview");
        println!("wrote {out}");
    }

    /// A skin is a preference kept on disk, and the page is told which one
    /// on every push — it cannot know until the plugin says so.
    #[test]
    fn the_skin_is_remembered_and_pushed_to_the_page() {
        let _library = crate::library::TempLibrary::new("editor-skin");
        // The cache is process-wide; start from a known state.
        *SKIN.lock().unwrap() = None;

        let js = script(&[2.0, 100.0, 0.0, 0.0], b"").expect("script");
        assert!(js.contains(&format!("__seco_skin(\"{DEFAULT_SKIN}\")")), "{js}");

        assert!(message("skin rockola").is_none(), "the page already applied it");
        let js = script(&[2.0, 100.0, 0.0, 0.0], b"").expect("script");
        assert!(js.contains("__seco_skin(\"rockola\")"), "{js}");
        assert_eq!(crate::library::read_skin().as_deref(), Some("rockola"));

        // A name the plugin does not ship goes nowhere: it ends up in a
        // stylesheet selector and in a file.
        message("skin ../../etc/passwd");
        message("skin \"><script>");
        assert_eq!(crate::library::read_skin().as_deref(), Some("rockola"));
        *SKIN.lock().unwrap() = None;
    }

    /// The library requests the page can make, end to end through the disk.
    #[test]
    fn the_page_can_list_save_and_delete_curves() {
        let _library = crate::library::TempLibrary::new("editor-messages");
        let drawn = "0,0.9,0.2,0.4,0.6,0.8,1,1,1,0.5,0.5,0.5,1,1,1,1";

        // Nothing saved: an empty list, not a failure.
        let empty = message("list").expect("list answers");
        assert!(empty.contains("__seco_presets([])"), "{empty}");

        let saved = message(&format!("save kick 4x4|{drawn}")).expect("save answers");
        assert!(saved.contains("kick 4x4"), "{saved}");
        // The answer carries the points, so a card can draw the shape
        // instead of only naming it.
        assert!(saved.contains("0.900"), "the answer must carry the curve: {saved}");

        let deleted = message("delete kick 4x4").expect("delete answers");
        assert!(deleted.contains("__seco_presets([])"), "{deleted}");

        // Anything else is dropped rather than guessed at: this comes from
        // a webview.
        assert!(message("drop everything").is_none());
        assert!(message("save no-separator").is_none());
        assert!(message("").is_none());
    }

    /// A name is pasted into a JS string literal. `library::sanitize`
    /// already reduces it to letters, digits, space, dash and underscore,
    /// and this is the second wall.
    #[test]
    fn preset_names_cannot_break_out_of_the_answer() {
        let _library = crate::library::TempLibrary::new("editor-escape");
        let drawn = "0,0.9,0.2,0.4,0.6,0.8,1,1,1,0.5,0.5,0.5,1,1,1,1";
        message(&format!("save \"];alert(1);[\"|{drawn}")).expect("save answers");
        let listed = message("list").expect("list answers");
        assert!(!listed.contains("alert(1)"), "{listed}");
        assert!(listed.starts_with("window.__seco_presets && window.__seco_presets(["));
    }

    /// Which shape is active is the page's business — the snippet carries
    /// all of them, so mix, curve and bypass must not change it. Rate does
    /// (it changes how wide the dip entry is), and so does the drawn curve.
    #[test]
    fn only_the_rate_and_the_drawn_curve_change_the_snippet() {
        let a = script(&[2.0, 100.0, 0.0, 0.0], b"").expect("shapes");
        let b = script(&[2.0, 0.0, 2.0, 1.0], b"").expect("shapes");
        assert_eq!(a, b);

        let fast = script(&[4.0, 100.0, 0.0, 0.0], b"").expect("shapes");
        assert_ne!(a, fast, "the entry fade must look wider at a faster rate");

        let drawn = crate::custom::CustomCurve::parse(
            b"0,0.9,0.9,0.9,0.9,0.9,1,1,1,1,1,1,1,1,1,1",
        );
        let edited = script(&[2.0, 100.0, 0.0, 0.0], drawn.to_wire().as_bytes())
            .expect("shapes");
        assert_ne!(a, edited, "the page must see the curve the user drew");
    }

    /// What is drawn is what sounds: the points are what `gain()` returns
    /// at the drawn rate, and every shipped shape is sent.
    #[test]
    fn points_come_from_the_played_shapes() {
        let rate = 2; // 1/4
        let js = script(&[rate as f64, 100.0, 0.0, 0.0], b"").expect("shapes");
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
        let js = script(&[2.0, 100.0, 0.0, 0.0], b"").expect("shapes");
        // One per shipped shape, the array around them, and the drawn one.
        assert_eq!(js.matches('[').count(), duck::tables().len() + 2);
    }
}
