//! Neta's WebKit compatibility editor.
//!
//! WGPU is Neta's native renderer architecture. This page is deliberately a
//! small compatibility skin for hosts using SECO's current macOS WebKit GUI;
//! it consumes the same bounded analysis vocabulary and never speaks to DSP
//! state directly.
//!
//! # Shape
//!
//! One row of resizable boxes, left to right, and every meter draws
//! horizontally inside its own box: bars run left to right, time runs left
//! to right, frequency runs left to right. Stacking full-width lanes was the
//! other option and it reads as one undifferentiated block — a row of boxes
//! gives each module an edge, and lets the user spend width on the module
//! they are actually watching. The object opens the row and the visual
//! closes it, so the measurements sit together in the middle.
//!
//! Order, widths and which boxes are on all live in the plugin's state
//! block; see `settings`.
//!
//! Everything it draws comes from 256 `f32` slots, which is a tighter budget
//! than a meter would choose. The drawing works with that rather than around
//! it: the spectrogram is painted at its native 80×64 and scaled up by the
//! canvas, and the stereometer accumulates frames instead of plotting 16
//! lonely dots.

use std::fmt::Write;

use seco_core::EditorPage;

pub(crate) const PAGE: EditorPage = EditorPage {
    html: HTML,
    width: 1_180,
    height: 720,
};

const METRICS: usize = 13;
const WAVE_MIN: usize = 16;
const WAVE_MAX: usize = 80;
const WAVE_POINTS: usize = 64;
const SPECTRUM: usize = 144;
const SPECTRUM_POINTS: usize = 80;
const GONIOMETER: usize = 224;
const GONIOMETER_POINTS: usize = 16;
const REQUIRED_SCOPE_SLOTS: usize = GONIOMETER + GONIOMETER_POINTS * 2;

const _: () = {
    assert!(METRICS <= WAVE_MIN);
    assert!(WAVE_MIN + WAVE_POINTS == WAVE_MAX);
    assert!(WAVE_MAX <= SPECTRUM);
    assert!(SPECTRUM + SPECTRUM_POINTS == GONIOMETER);
    assert!(REQUIRED_SCOPE_SLOTS == 256);
};

/// Serializes the fixed visualization slots into one small UI-thread script.
pub(crate) fn frame(scope: &[f32]) -> Option<String> {
    if scope.len() < REQUIRED_SCOPE_SLOTS {
        return None;
    }
    let mut values = String::with_capacity(REQUIRED_SCOPE_SLOTS * 7);
    for (index, value) in scope[..REQUIRED_SCOPE_SLOTS].iter().enumerate() {
        if index > 0 {
            values.push(',');
        }
        // `RtContext::set_scope` has already rejected non-finite values.
        let _ = write!(values, "{value:.4}");
    }
    Some(format!(
        "window.__neta_frame&&window.__neta_frame([{values}]);"
    ))
}

const HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<style>
  :root {
    /* Midnight Kids: near-black violet, hot pink for anything you read,
       cyan for anything that is on or is data, and two alarm colours that
       appear nowhere else so they always mean the same thing. */
    --void:#171320; --panel:#211a2e; --well:#0f0c16;
    --line:#33273f; --edge:#463656;
    --pink:#ff3d8a; --pink-dim:#a2537f; --pink-faint:#5e3a52;
    --cyan:#5ddce9; --cyan-dim:#3a8a94;
    --amber:#ffc247; --red:#ff4d5e;
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
    height:100%; padding:10px; display:grid; gap:8px;
    grid-template-rows:auto minmax(0,1fr) auto;
  }

  /* ---- header ---- */
  .top { display:flex; align-items:center; gap:12px; height:26px; }
  .brand { font-size:19px; font-weight:700; letter-spacing:.34em; color:var(--pink); }
  .brand i { font-style:normal; color:var(--cyan); }
  .rule { flex:1; height:1px; background:var(--line); }
  .headline { font-size:11px; letter-spacing:.1em; color:var(--pink-dim); }
  .headline b { color:var(--cyan); font-weight:400; }
  .gear {
    padding:3px 10px; font-size:10px; letter-spacing:.12em; text-transform:uppercase;
    color:var(--pink); border:1px solid var(--line); background:var(--panel);
  }
  .gear:hover, .gear[aria-expanded="true"] { background:var(--cyan); color:var(--void); border-color:var(--cyan); }

  /* ---- the row of module boxes ---- */
  .row { display:flex; min-width:0; min-height:0; }
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
  .cap span { color:var(--pink-faint); overflow:hidden; text-overflow:ellipsis; }
  .note { padding:4px 7px; border-top:1px solid var(--line); font-size:9px;
          letter-spacing:.06em; color:var(--pink-faint); min-height:20px;
          overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
  canvas { display:block; width:100%; height:100%; min-height:0; background:var(--well); }

  /* The divider between two boxes. Wider than it looks, because a 1px
     grab target on a plugin window is a coin toss. */
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

  /* ---- settings sheet ---- */
  .sheet {
    position:fixed; inset:0; z-index:5; padding:34px 26px 20px;
    background:rgba(13,10,18,.94); overflow:auto;
  }
  .sheet[hidden] { display:none; }
  .sheetGrid { display:grid; gap:14px; grid-template-columns:repeat(auto-fit,minmax(250px,1fr)); }
  .group { border:1px solid var(--line); background:var(--panel); padding:10px 12px 12px; }
  .group h2 { margin:0 0 9px; font-size:10px; font-weight:400;
              letter-spacing:.18em; text-transform:uppercase; color:var(--pink-dim); }
  .chips { display:flex; flex-wrap:wrap; gap:5px; }
  .chip {
    padding:3px 9px; font-size:11px; letter-spacing:.04em;
    color:var(--pink); border:1px solid var(--line); background:var(--well);
  }
  .chip[aria-pressed="true"] { background:var(--cyan); color:var(--void); border-color:var(--cyan); }
  .chip:hover { border-color:var(--cyan); }
  .rows { display:grid; gap:4px; }
  .rowItem { display:flex; align-items:center; gap:6px; font-size:11px; }
  .rowItem span { flex:1; color:var(--pink); }
  .rowItem small { color:var(--pink-faint); font-size:10px; min-width:34px; text-align:right; }
  .move { padding:1px 8px; border:1px solid var(--line); background:var(--well); color:var(--pink); }
  .move:hover { border-color:var(--cyan); color:var(--cyan); }
  .move:disabled { opacity:.3; cursor:default; }
  .field { display:flex; gap:6px; margin-top:2px; }
  .field input {
    flex:1; min-width:0; font:inherit; font-size:11px; padding:4px 7px;
    color:var(--cyan); background:var(--well); border:1px solid var(--line);
    user-select:text; -webkit-user-select:text;
  }
  .field input:focus { outline:none; border-color:var(--cyan); }
  .go { padding:4px 12px; font-size:11px; color:var(--void); background:var(--pink); }
  .go:hover { background:var(--cyan); }
  .hint { margin:8px 0 0; font-size:10px; line-height:1.5; color:var(--pink-faint); }
  .hint b { color:var(--pink-dim); font-weight:400; }
  .sheetFoot { margin-top:14px; display:flex; justify-content:space-between; gap:10px; }
  .reset { padding:5px 14px; font-size:11px; letter-spacing:.1em; text-transform:uppercase;
           color:var(--pink); border:1px solid var(--line); }
  .reset:hover { border-color:var(--cyan); color:var(--cyan); }
  .close { padding:5px 16px; font-size:11px; letter-spacing:.1em; text-transform:uppercase;
           color:var(--void); background:var(--cyan); }

  @media (prefers-reduced-motion:reduce) { .spin { animation:none; } }
  @media (max-width:860px) {
    .shell { grid-template-columns:150px minmax(240px,1fr) 150px; }
    .brand { font-size:15px; letter-spacing:.2em; }
  }
</style>
</head>
<body>
<main class="shell">
  <header class="top">
    <div class="brand">NE<i>TA</i></div>
    <div class="rule"></div>
    <div class="headline">integrated <b id="hint">listening</b></div>
    <button class="gear" id="gear" aria-expanded="false" aria-controls="sheet">settings</button>
  </header>

  <div class="row" id="row">
    <section class="box" data-module="loudness"><div class="cap"><b>loudness</b><span>LUFS</span></div><canvas id="loudness"></canvas></section>
    <section class="box" data-module="spectrum"><div class="cap"><b>spectrum</b><span>20&#8202;Hz&#8202;–&#8202;20&#8202;k</span></div><canvas id="spectrum"></canvas></section>
    <section class="box" data-module="spectrogram"><div class="cap"><b>spectrogram</b><span>history</span></div><canvas id="spectrogram"></canvas></section>
    <section class="box" data-module="waveform"><div class="cap"><b>waveform</b><span>envelope</span></div><canvas id="waveform"></canvas></section>
    <section class="box" data-module="stereo"><div class="cap"><b>stereo</b><span>L / R</span></div><canvas id="stereo"></canvas></section>
    <section class="box" data-module="object">
      <div class="cap"><b>object</b><span id="objectPoints"></span></div>
      <canvas id="object"></canvas>
      <div class="note" id="objectNote">no model</div>
    </section>
    <section class="box" data-module="visuals">
      <div class="cap"><b>visuals</b><span>spectrum</span></div>
      <canvas id="visuals"></canvas>
      <div class="note">driven by the mix</div>
    </section>
  </div>

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

<div class="sheet" id="sheet" hidden role="dialog" aria-modal="true" aria-label="Settings">
  <div class="sheetGrid">
    <section class="group">
      <h2>Enable / disable modules</h2>
      <div class="chips" id="moduleChips"></div>
      <p class="hint">A hidden module stops drawing. Measurement carries on either way.</p>
    </section>
    <section class="group">
      <h2>Order and width</h2>
      <div class="rows" id="moduleRows"></div>
      <p class="hint">Drag the divider between two boxes to change their widths.</p>
    </section>
    <section class="group">
      <h2>3D model</h2>
      <div class="field">
        <input id="modelPath" type="text" spellcheck="false" placeholder="/path/to/model.obj" aria-label="Path to an OBJ file">
        <button class="go" id="modelLoad">Load</button>
      </div>
      <p class="hint" id="modelStatus">Wavefront <b>.obj</b> up to 96&#8239;MB. Neta samples it down to a
        point cloud and spins it with the mix.</p>
    </section>
  </div>
  <div class="sheetFoot">
    <button class="reset" id="sheetReset">Reset layout</button>
    <button class="close" id="sheetClose">Close settings</button>
  </div>
</div>

<script>
(() => {
  "use strict";
  const I={m:0,s:1,tp:2,corr:3,width:4,l:5,r:6,mid:7,side:8,psr:9,int:10,lra:11,plr:12,
           waveMin:16,waveMax:80,spectrum:144,gonio:224};
  // The plugin sends this when a gated value has no programme behind it yet.
  const UNMEASURED=-150;
  const BANDS=80, WAVE_POINTS=64, GONIO_POINTS=16;
  const TOP_DB=0, BOTTOM_DB=-60, TARGET_LUFS=-14, TP_CEILING=-1;
  const MODULES=["loudness","spectrum","spectrogram","waveform","stereo","object","visuals"];

  const $=id=>document.getElementById(id);
  const clamp=(x,a,b)=>Math.max(a,Math.min(b,x));
  const real=x=>Number.isFinite(x)&&x>UNMEASURED;
  const norm=db=>clamp((db-BOTTOM_DB)/(TOP_DB-BOTTOM_DB),0,1);
  const post=m=>{ try{ window.webkit.messageHandlers.seco.postMessage(m); }catch(_){} };

  const DEFAULT_ORDER=[5,0,1,2,3,4,6], DEFAULT_WIDTHS=[130,150,200,190,170,130,130];
  const MIN_WIDTH=20, MAX_WIDTH=2000;

  let v=new Array(256).fill(0);
  let enabled=MODULES.map(()=>true);
  let order=DEFAULT_ORDER.slice(), widths=DEFAULT_WIDTHS.slice(), modelPath="";
  let clock=0;

  // Resolved once instead of per draw: seven canvases times a dozen colour
  // lookups a frame is a forced style recalculation for no benefit.
  let ink={};
  const readInk=()=>{
    const style=getComputedStyle(document.documentElement);
    for(const name of ["void","panel","well","line","edge","pink","pink-dim","pink-faint",
                       "cyan","cyan-dim","amber","red"]){
      ink[name]=style.getPropertyValue("--"+name).trim();
    }
  };

  function fit(canvas){
    const dpr=Math.min(devicePixelRatio||1,2), rect=canvas.getBoundingClientRect();
    const w=Math.max(1,Math.round(rect.width*dpr)), h=Math.max(1,Math.round(rect.height*dpr));
    if(canvas.width!==w||canvas.height!==h){ canvas.width=w; canvas.height=h; }
    const ctx=canvas.getContext("2d");
    ctx.clearRect(0,0,w,h);
    ctx.fillStyle=ink.well; ctx.fillRect(0,0,w,h);
    return [ctx,w,h,dpr];
  }
  const mono=(ctx,px)=>{ ctx.font=px+'px ui-monospace,Menlo,monospace'; ctx.textBaseline='middle'; };

  /* ==== loudness =========================================================
     Four horizontal runways sharing one dB axis, so a bar that reaches
     further is louder — the comparison the panel exists to make. */
  const BARS=[
    {key:"m",  label:"M",  idx:I.m,  colour:()=>ink.pink},
    {key:"s",  label:"S",  idx:I.s,  colour:()=>ink.pink},
    {key:"i",  label:"I",  idx:I.int,colour:()=>ink.cyan},
    {key:"tp", label:"TP", idx:I.tp, colour:x=>x>TP_CEILING?ink.red:ink["cyan-dim"]},
  ];
  const TICKS=[-60,-48,-36,-24,-18,-12,-6,0];
  const holds={};

  function drawLoudness(){
    const [ctx,w,h,dpr]=fit($("loudness"));
    const left=20*dpr, right=40*dpr, bottom=14*dpr;
    const plotW=w-left-right;
    // Four bars want four bars' worth of height, not the whole column. The
    // block is centred so a tall box reads as space around the meter rather
    // than as a meter stretched out of shape.
    const plotH=Math.min(h-bottom-8*dpr,BARS.length*30*dpr);
    const top=Math.max(6*dpr,(h-bottom-plotH)/2);
    if(plotW<=0||plotH<=0) return;
    const x=db=>left+norm(db)*plotW;
    const floor=top+plotH;

    mono(ctx,8.5*dpr); ctx.textAlign="center";
    // Labels are dropped, not shrunk, when the column is narrow: eight
    // overlapping numbers say less than three legible ones.
    let lastLabel=-1e9;
    for(const db of TICKS){
      const tx=x(db);
      ctx.strokeStyle=ink.line; ctx.lineWidth=1;
      ctx.beginPath(); ctx.moveTo(tx,top); ctx.lineTo(tx,floor); ctx.stroke();
      const text=String(db), width=ctx.measureText(text).width;
      if(tx-width/2>lastLabel+4*dpr){
        ctx.fillStyle=ink["pink-faint"]; ctx.fillText(text,tx,floor+7*dpr);
        lastLabel=tx+width/2;
      }
    }
    // The delivery target. Unlabelled here: the header names it, and a
    // number in the plot would sit on whichever bar happens to be at -14.
    const tx=x(TARGET_LUFS);
    ctx.setLineDash([3*dpr,3*dpr]); ctx.strokeStyle=ink.amber; ctx.lineWidth=1*dpr;
    ctx.beginPath(); ctx.moveTo(tx,top); ctx.lineTo(tx,floor); ctx.stroke();
    ctx.setLineDash([]);

    const rowH=plotH/BARS.length, thick=Math.min(rowH*0.5,13*dpr);
    BARS.forEach((bar,index)=>{
      const y=top+rowH*(index+0.5), value=v[bar.idx], live=real(value);
      ctx.fillStyle="rgba(255,255,255,.035)";
      ctx.fillRect(left,y-thick/2,plotW,thick);
      if(live){
        const colour=bar.colour(value), end=x(clamp(value,BOTTOM_DB,TOP_DB));
        const gradient=ctx.createLinearGradient(left,0,end,0);
        gradient.addColorStop(0,"rgba(255,255,255,.05)");
        gradient.addColorStop(1,colour);
        ctx.fillStyle=gradient; ctx.fillRect(left,y-thick/2,end-left,thick);
        // Peak hold: a meter you glance at has to remember the spike you
        // were not looking at.
        const hold=holds[bar.key]||(holds[bar.key]={v:BOTTOM_DB,t:0}), now=performance.now();
        if(value>=hold.v||now-hold.t>1400){ hold.v=value; hold.t=now; }
        ctx.fillStyle=colour;
        ctx.fillRect(x(clamp(hold.v,BOTTOM_DB,TOP_DB))-1.5*dpr,y-thick/2,1.5*dpr,thick);
      }
      mono(ctx,9*dpr);
      ctx.fillStyle=live?ink.pink:ink["pink-faint"]; ctx.textAlign="left";
      ctx.fillText(bar.label,3*dpr,y);
      ctx.fillStyle=live?ink.cyan:ink["pink-faint"]; ctx.textAlign="right";
      ctx.fillText(live?value.toFixed(1):"—",w-4*dpr,y);
    });
  }

  /* ==== spectrum ========================================================= */
  function drawSpectrum(){
    const [ctx,w,h,dpr]=fit($("spectrum"));
    const top=15*dpr, floor=h-3*dpr, span=floor-top;
    if(span<=0) return;
    ctx.beginPath(); ctx.moveTo(0,floor);
    for(let i=0;i<BANDS;i++){
      const x=i*w/(BANDS-1), level=clamp(v[I.spectrum+i]||0,0,1);
      ctx.lineTo(x,floor-level*span);
    }
    ctx.lineTo(w,floor); ctx.closePath();
    const fill=ctx.createLinearGradient(0,floor,0,top);
    fill.addColorStop(0,"rgba(93,220,233,.05)");
    fill.addColorStop(1,"rgba(255,61,138,.45)");
    ctx.fillStyle=fill; ctx.fill();
    ctx.strokeStyle=ink.cyan; ctx.lineWidth=1.2*dpr; ctx.stroke();
    // Decade marks, because a spectrum without frequencies is a texture.
    mono(ctx,8*dpr); ctx.textAlign="center"; ctx.fillStyle=ink["pink-faint"];
    for(const [label,at] of [["100",0.24],["1k",0.5],["10k",0.79]]){
      ctx.fillText(label,at*w,h-8*dpr);
    }
  }

  /* ==== spectrogram ======================================================
     Painted at its true resolution — one pixel per band per row — into an
     80x64 offscreen, then scaled by the canvas. Per screen pixel instead is
     ~2M ramp lookups a frame, which drops the whole editor to single-digit
     fps; this is ~5k, and the bilinear scale looks smoother anyway. */
  const ROWS=64, history=[];
  const RAMP=[[0,[14,10,22]],[.2,[54,28,84]],[.42,[168,42,120]],
              [.62,[255,61,138]],[.82,[255,180,120]],[1,[224,250,255]]];
  function heat(t){
    t=clamp(t,0,1);
    for(let i=1;i<RAMP.length;i++){
      if(t<=RAMP[i][0]){
        const [t0,c0]=RAMP[i-1],[t1,c1]=RAMP[i], k=(t-t0)/((t1-t0)||1);
        return [c0[0]+(c1[0]-c0[0])*k|0, c0[1]+(c1[1]-c0[1])*k|0, c0[2]+(c1[2]-c0[2])*k|0];
      }
    }
    return RAMP[RAMP.length-1][1];
  }
  const off=document.createElement("canvas");
  off.width=ROWS; off.height=BANDS;
  const offCtx=off.getContext("2d"), offImage=offCtx.createImageData(ROWS,BANDS);

  function drawSpectrogram(){
    const [ctx,w,h]=fit($("spectrogram"));
    // Time runs left to right and frequency bottom to top, so a column here
    // lines up with the same instant in the lanes above and below.
    const data=offImage.data, rows=history.length;
    for(let band=0;band<BANDS;band++){
      for(let column=0;column<ROWS;column++){
        const row=history[rows-ROWS+column];
        const o=((BANDS-1-band)*ROWS+column)*4;
        const [r,g,b]=row?heat(row[band]):[14,10,22];
        data[o]=r; data[o+1]=g; data[o+2]=b; data[o+3]=255;
      }
    }
    offCtx.putImageData(offImage,0,0);
    ctx.imageSmoothingEnabled=true;
    ctx.drawImage(off,0,0,w,h);
  }

  /* ==== waveform ========================================================= */
  function drawWaveform(){
    const [ctx,w,h,dpr]=fit($("waveform"));
    // Same reasoning as the loudness block: an envelope stretched over 600
    // pixels of column is a shape, not a reading.
    const band=Math.min(h*0.8,190*dpr), mid=h/2, scale=band*0.46;
    ctx.beginPath();
    for(let i=0;i<WAVE_POINTS;i++){
      const x=i*w/(WAVE_POINTS-1), top=mid-clamp(v[I.waveMax+i]||0,-1,1)*scale;
      i?ctx.lineTo(x,top):ctx.moveTo(x,top);
    }
    for(let i=WAVE_POINTS-1;i>=0;i--){
      ctx.lineTo(i*w/(WAVE_POINTS-1),mid-clamp(v[I.waveMin+i]||0,-1,1)*scale);
    }
    ctx.closePath();
    ctx.fillStyle="rgba(255,61,138,.22)"; ctx.fill();
    ctx.strokeStyle=ink.pink; ctx.lineWidth=1.1*dpr; ctx.stroke();
    ctx.strokeStyle=ink.line; ctx.lineWidth=1;
    ctx.beginPath(); ctx.moveTo(0,mid); ctx.lineTo(w,mid); ctx.stroke();
  }

  /* ==== stereo ===========================================================
     A horizontal unipolar stereometer, the shape MiniMeters uses: position
     across the lane is where the energy sits between the channels, height is
     how much of it there is. Sixteen points a frame is a scatter, so they
     accumulate into a decaying histogram instead. */
  const PAN_BINS=96, pan=new Float32Array(PAN_BINS), panScratch=new Float32Array(PAN_BINS);
  function pushStereo(){
    for(let i=0;i<PAN_BINS;i++) pan[i]*=0.88;
    for(let i=0;i<GONIO_POINTS;i++){
      const side=v[I.gonio+i*2]||0, m=v[I.gonio+i*2+1]||0;
      const energy=Math.hypot(m,side);
      if(energy<1e-4) continue;
      // -1 is all in one channel, +1 all in the other, 0 is centred.
      const position=clamp(side/(Math.abs(m)+Math.abs(side)),-1,1);
      const bin=clamp(Math.round((position+1)*0.5*(PAN_BINS-1)),0,PAN_BINS-1);
      pan[bin]=Math.min(1,pan[bin]+energy);
    }
    // Sixteen points landing in ninety-six bins is a picket fence, and a
    // picket fence is not a stereo image. One blur pass a frame turns the
    // same samples into the distribution they are drawn from.
    for(let i=0;i<PAN_BINS;i++){
      const before=pan[i-1]??pan[i], after=pan[i+1]??pan[i];
      panScratch[i]=before*0.25+pan[i]*0.5+after*0.25;
    }
    pan.set(panScratch);
  }
  function drawStereo(){
    const [ctx,w,h,dpr]=fit($("stereo"));
    const top=15*dpr, floor=h-17*dpr, span=floor-top;
    if(span<=0) return;
    ctx.strokeStyle=ink.line; ctx.lineWidth=1;
    for(const at of [0,0.25,0.5,0.75,1]){
      ctx.beginPath(); ctx.moveTo(at*w,top); ctx.lineTo(at*w,floor); ctx.stroke();
    }
    const barW=w/PAN_BINS;
    for(let i=0;i<PAN_BINS;i++){
      const level=clamp(pan[i],0,1);
      if(level<=0.001) continue;
      // Centre energy is mono, edge energy is wide: colouring by position
      // makes a too-wide mix visible without reading the correlation number.
      const distance=Math.abs(i/(PAN_BINS-1)*2-1);
      ctx.fillStyle=distance>0.72?ink.pink:ink.cyan;
      ctx.globalAlpha=0.35+0.65*level;
      ctx.fillRect(i*barW,floor-level*span,Math.max(1,barW),level*span);
    }
    ctx.globalAlpha=1;
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
    const glow=ctx.createRadialGradient(cx,cy,0,cx,cy,Math.max(core,1));
    glow.addColorStop(0,"rgba(255,61,138,"+(0.35+loud*0.5).toFixed(2)+")");
    glow.addColorStop(1,"rgba(255,61,138,0)");
    ctx.fillStyle=glow; ctx.beginPath(); ctx.arc(cx,cy,Math.max(core,1),0,Math.PI*2); ctx.fill();

    ctx.lineWidth=Math.max(1,1.4*dpr); ctx.lineCap="round";
    for(let i=0;i<BANDS;i++){
      const level=clamp(v[I.spectrum+i]||0,0,1);
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
  const attributes={};

  // `mediump` is declared in both stages on purpose: a uniform shared
  // between them must agree, and a vertex shader defaults to `highp`, so
  // omitting this here links fine on some drivers and fails on others with
  // "Precisions of uniform 'scatter' differ between VERTEX and FRAGMENT".
  const VERTEX=`
    precision mediump float;
    attribute vec3 position;
    uniform float turn, scatter, size, aspect, zoom;
    varying float depth;
    void main(){
      float c=cos(turn), s=sin(turn);
      vec3 p=vec3(position.x*c+position.z*s, position.y, -position.x*s+position.z*c);
      // Tipped forward so a flat model is seen three-quarters from above
      // rather than edge-on, which is the difference between a car and a
      // smear.
      float pitch=0.42;
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
    for(const name of ["turn","scatter","size","aspect","zoom"]){
      attributes[name]=gl.getUniformLocation(program,name);
    }
    buffer=gl.createBuffer();
    gl.enable(gl.BLEND); gl.blendFunc(gl.SRC_ALPHA,gl.ONE);
    setCloud(defaultCloud());
    return gl;
  }
  /// A Fibonacci sphere, so the panel shows what it is for before a model
  /// is chosen rather than sitting empty.
  function defaultCloud(){
    const total=1400, points=new Float32Array(total*3), golden=Math.PI*(3-Math.sqrt(5));
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
  function drawObject(){
    if(!initGl()) return;
    const dpr=Math.min(devicePixelRatio||1,2), rect=objectCanvas.getBoundingClientRect();
    const w=Math.max(1,Math.round(rect.width*dpr)), h=Math.max(1,Math.round(rect.height*dpr));
    if(objectCanvas.width!==w||objectCanvas.height!==h){ objectCanvas.width=w; objectCanvas.height=h; }
    gl.viewport(0,0,w,h);
    gl.clearColor(0.059,0.047,0.086,1); gl.clear(gl.COLOR_BUFFER_BIT);
    if(!pointCount) return;
    const loud=real(v[I.m])?norm(v[I.m]):0;
    gl.uniform1f(attributes.turn,clock*0.012);
    gl.uniform1f(attributes.scatter,loud);
    gl.uniform1f(attributes.size,Math.max(1.4,2.1*dpr));
    const aspect=w/h;
    gl.uniform1f(attributes.aspect,aspect);
    // The cloud is normalised to +/-1 on its longest axis, so this is the
    // largest zoom that still fits that axis inside the narrower dimension.
    gl.uniform1f(attributes.zoom,Math.min(1.7,Math.max(0.3,aspect*2.6)));
    gl.drawArrays(gl.POINTS,0,pointCount);
  }

  /* ==== readouts ========================================================= */
  function readout(id,value,digits,unit,hot){
    const el=$(id);
    if(!real(value)){ el.textContent="listening"; el.className="pending"; return; }
    el.textContent=value.toFixed(digits);
    if(unit){ const em=document.createElement("em"); em.textContent=unit; el.appendChild(em); }
    el.className=hot?"hot":"";
  }

  /* ==== frame ============================================================ */
  const DRAW={loudness:drawLoudness,spectrum:drawSpectrum,spectrogram:drawSpectrogram,
              waveform:drawWaveform,stereo:drawStereo,object:drawObject,visuals:drawVisuals};

  function draw(){
    MODULES.forEach((name,index)=>{ if(enabled[index]) DRAW[name](); });
    readout("int",v[I.int],1,"LUFS");
    readout("lra",v[I.lra],1,"LU");
    readout("psr",v[I.psr],1,"LU");
    readout("plr",v[I.plr],1,"LU");
    const tp=v[I.tp];
    readout("tp",tp,1,"dBTP",real(tp)&&tp>TP_CEILING);
    const corr=Number.isFinite(v[I.corr])?v[I.corr]:0;
    const c=$("corr");
    c.textContent=(corr>=0?"+":"")+corr.toFixed(2);
    c.className=corr<0?"hot":"";
    $("width").textContent=(Number.isFinite(v[I.width])?v[I.width]:0).toFixed(2);
    const integrated=v[I.int];
    $("hint").textContent=real(integrated)
      ? integrated.toFixed(1)+" LUFS · target "+TARGET_LUFS
      : "listening";
  }

  window.__neta_frame=next=>{
    v=next; clock++;
    const row=new Array(BANDS);
    for(let i=0;i<BANDS;i++) row[i]=clamp(v[I.spectrum+i]||0,0,1);
    history.push(row); if(history.length>ROWS) history.shift();
    pushStereo();
    draw();
  };

  /* ==== settings ========================================================= */
  const chips=$("moduleChips"), rows=$("moduleRows"), row=$("row");
  const boxes={};
  for(const box of row.querySelectorAll(".box")) boxes[box.dataset.module]=box;

  MODULES.forEach((name,index)=>{
    const chip=document.createElement("button");
    chip.className="chip"; chip.type="button"; chip.textContent=name;
    chip.addEventListener("click",()=>{ enabled[index]=!enabled[index]; applySettings(); save(); });
    chips.appendChild(chip);
  });

  function move(from,to){
    if(to<0||to>=order.length) return;
    const [module]=order.splice(from,1);
    order.splice(to,0,module);
    applySettings(); save();
  }
  function buildRows(){
    rows.textContent="";
    order.forEach((module,position)=>{
      const item=document.createElement("div");
      item.className="rowItem";
      const label=document.createElement("span");
      label.textContent=MODULES[module];
      const size=document.createElement("small");
      size.textContent=widths[module];
      const back=document.createElement("button");
      back.className="move"; back.type="button"; back.textContent="\u25c0";
      back.setAttribute("aria-label","Move "+MODULES[module]+" left");
      back.disabled=position===0;
      back.addEventListener("click",()=>move(position,position-1));
      const forward=document.createElement("button");
      forward.className="move"; forward.type="button"; forward.textContent="\u25b6";
      forward.setAttribute("aria-label","Move "+MODULES[module]+" right");
      forward.disabled=position===order.length-1;
      forward.addEventListener("click",()=>move(position,position+1));
      item.append(label,size,back,forward);
      rows.appendChild(item);
    });
  }

  // Rebuilt rather than patched: the visible boxes change with both the
  // order and the on/off flags, and a grip is only meaningful between two
  // boxes that are actually next to each other.
  function layout(){
    row.textContent="";
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
      // Weights are relative, so a pixel of travel is worth however much of
      // the pair's own share of the row it represents.
      const pairPixels=boxes[MODULES[left]].offsetWidth+boxes[MODULES[right]].offsetWidth;
      const perPixel=pairPixels>0?total/pairPixels:0;
      const drag=move=>{
        const shift=(move.clientX-startX)*perPixel;
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
          buildRows(); save();
        }
        draw();
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
    layout(); buildRows();
    $("modelPath").value=modelPath;
    draw();
  }
  const save=()=>post("state m="+enabled.map(on=>on?"1":"0").join("")+
                      ";o="+order.join("")+
                      ";w="+widths.join(",")+
                      (modelPath&&!/[;=]/.test(modelPath)?";model="+modelPath:""));

  // Pushed by Plugin::editor_script from the same state block the session
  // stores, so the window opens the way it was left.
  window.__neta_settings=(flags,savedOrder,savedWidths,path)=>{
    if(Array.isArray(flags)) enabled=MODULES.map((_,i)=>flags[i]!==false);
    if(Array.isArray(savedOrder)&&savedOrder.length===MODULES.length) order=savedOrder.slice();
    if(Array.isArray(savedWidths)&&savedWidths.length===MODULES.length) widths=savedWidths.slice();
    modelPath=typeof path==="string"?path:"";
    applySettings();
    if(modelPath) post("msg model "+modelPath);
  };
  window.__neta_model=(name,points)=>{
    setCloud(new Float32Array(points));
    $("objectNote").textContent=name;
    $("modelStatus").textContent=name+" loaded \u2014 "+pointCount.toLocaleString()+" points.";
  };
  window.__neta_model_failed=reason=>{
    $("objectNote").textContent="no model";
    $("modelStatus").textContent=reason;
  };

  const sheet=$("sheet"), gear=$("gear");
  const openSheet=open=>{
    sheet.hidden=!open;
    gear.setAttribute("aria-expanded",open?"true":"false");
    if(open) $("modelPath").focus();
  };
  gear.addEventListener("click",()=>openSheet(sheet.hidden));
  $("sheetClose").addEventListener("click",()=>openSheet(false));
  $("sheetReset").addEventListener("click",()=>{
    enabled=MODULES.map(()=>true);
    order=DEFAULT_ORDER.slice(); widths=DEFAULT_WIDTHS.slice();
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
  window.addEventListener("resize",draw);
  draw();
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
        assert!(frame(&vec![0.0; REQUIRED_SCOPE_SLOTS]).is_some());
        assert!(frame(&[]).is_none());
        assert_eq!(WAVE_MAX - WAVE_MIN, WAVE_POINTS);
        assert_eq!(GONIOMETER, SPECTRUM + SPECTRUM_POINTS);
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
            assert!(index < METRICS, "slot {index} is outside the metric block");
        }
        assert!(
            HTML.contains("waveMin:16") && HTML.contains("spectrum:144") && HTML.contains("gonio:224"),
            "the page's block offsets must match the plugin's"
        );
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
            let (shader, remainder) = after.split_once('`').expect("unterminated template literal");
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
