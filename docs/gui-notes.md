# CLAP GUI notes — Phase 6.0 reconnaissance

Read from `reference/clap` @ `195b42a` (tag 1.2.10), plus cited web sources.
No code was written for this phase.

## 1. `ext/gui.h` — the classic GUI extension

- Extension ID: `CLAP_EXT_GUI = "clap.gui"` (`gui.h:47`).
- **Two modes**: embedded (plugin window inside the host's window) or
  floating. "The Embedding protocol is by far the most common, supported by
  all hosts to date, and a plugin author should support at least that case"
  (`gui.h:16-17`).
- **Full lifecycle, quoted from the header's own walkthrough**
  (`gui.h:19-33`):
  1. `is_api_supported()` — 2. `create()` — then, embedded:
  `set_scale()` → `can_resize()` → `set_size()`/`get_size()` →
  `set_parent()` — finally `show()` / `hide()` … `destroy()`.
- **Resize, two directions** (`gui.h:35-45`): plugin-initiated via
  `clap_host_gui.request_resize()` (host may accept without calling
  `set_size` back); host/drag-initiated via `adjust_size(new) → set_size()`,
  only if `can_resize()`.
- **Threading: every plugin-side method is `[main-thread]`**
  (`gui.h:107-212`); some additionally `& !floating` or `& floating`.
  Host-side `request_resize`/`request_show`/`request_hide`/`closed` are
  `[thread-safe]` (`gui.h:214-245`). No GUI call ever touches the audio
  thread — the GUI fits SECO's aliasing model as another main-thread-only
  callback class.
- **macOS**: `CLAP_WINDOW_API_COCOA = "cocoa"` uses **logical size — do not
  call `set_scale()`** (`gui.h:56-57`). `clap_window` is a tagged union;
  for cocoa the payload is `clap_nsview` = `void *` — **an `NSView *`**
  (`gui.h:74-89`). `set_parent()` hands us that NSView to embed into
  (`gui.h:183-187`).

## 2. `ext/draft/webview.h` — CLAP's own webview mechanism

Exists, and it inverts ownership: **the HOST owns the webview**, the plugin
only serves content and messages.

- ID `"clap.webview/3"`, still in `draft/` (`webview.h:6`); introduced
  around CLAP 1.2.7.
- Plugin provides: `get_uri()` (start page; `data:`/`file:`/relative URIs),
  `get_resource()` (streams assets by path + MIME), `receive()` (messages
  from JS via `window.parent.postMessage`, ArrayBuffer). Host provides
  `send()` (plugin→JS as `MessageEvent`) (`webview.h:25-73`).
- There is even a `CLAP_WINDOW_API_WEBVIEW` for gui.h sizing interplay
  (`webview.h:8-10`).

**Verdict for this project: not usable today.** Two blockers:

1. Mainstream host adoption ≈ none. The WebCLAP ecosystem uses it, and
   signalsmith's example plugins pair it with "a helper for native
   platforms" — which exists precisely because ordinary hosts (REAPER,
   Bitwig) don't provide the host-side webview.
2. Our Ableton route is VST3-via-clap-wrapper, and the wrapper transposes
   CLAP features to VST3 equivalents; a draft host-side webview has no VST3
   equivalent to transpose to. Classic `clap.gui` does: VST3's `IPlugView`.

Worth revisiting if hosts adopt it: it would delete all of our
platform-window code.

## 3. The workable route: classic `clap.gui` + self-hosted WKWebView

The industry-standard shape for web UIs in plugins: on `create()` we build a
`WKWebView`; on `set_parent()` we attach it as a subview of the host's
NSView; HTML/CSS/JS is plain and embedded in the binary (served inline; no
HTTP, no files). Messaging:

- JS → Rust: `WKUserContentController` script message handlers
  (`webkit.messageHandlers.<name>.postMessage(...)`).
- Rust → JS: `evaluateJavaScript` (async, fine for UI updates).

Production evidence this route holds: NovoNotes' WRAC template (WebView +
Rust Audio + CLAP) is open source and "used in our production environment,
serving 15K+ of our plugin users".

ObjC interop layer: the `objc2` family (`objc2`, `objc2-foundation`,
`objc2-app-kit`, `objc2-web-kit`) — typed bindings, not a GUI framework;
the UI itself stays plain HTML/CSS/JS. Hand-rolling the ObjC runtime FFI
was considered and rejected: unlike C ABI mirrors, ObjC calls involve
selector registration, encodings and autorelease semantics where typed
bindings remove whole bug classes.

Known fragilities to design around (in order of likelihood):

1. **Keyboard focus**: hosts intercept key events; text input inside plugin
   webviews is a classic pain. Mitigation: mouse-only UI (a curve editor
   needs no typing).
2. **Weight**: WKWebView spawns helper processes; expect tens of MB per
   open editor and a first-paint latency of ~100-300 ms. Acceptable for a
   portfolio plugin; documented.
3. **Threading**: WebKit APIs are main-thread-only — which is exactly what
   `clap.gui` guarantees, so no impedance.
4. **VST3/Ableton**: the wrapper presents our NSView through `IPlugView`;
   WKWebView-based commercial plugins run in Live today. Verified
   empirically in phase 6.1 (pluginval instantiates editors) before any
   Ableton test.

## 4. Fallback if the webview proves too fragile

Hand-drawn NSView: CoreGraphics drawing + mouse events via `objc2`
subclassing, no webview, no helper processes, no JS bridge. Costs: the
curve editor (hit-testing, dragging, rendering) becomes hand-written Rust,
roughly 3-5× the UI code. Kept as plan B; nothing in the phase 6 plumbing
(gui vtable, shared curve buffer) changes between the two.

## 5. Constraints mapping (unchanged from the phase brief)

- GUI is one more extension in seco-clap behind an off-by-default `gui`
  feature; the no-gui `.clap` stays byte-identical (release check, as in
  the VST3 phase).
- The edited curve crosses to the audio thread through a lock-free
  single-writer/single-reader buffer (triple-buffer protocol, to be raced
  under miri like the params model). Points→`CurveTable` conversion happens
  on the main thread, never in `process()`; the allocation detector stays
  silent.
- GUI state lives main-thread-only, in the same per-callback-class
  exclusivity pattern the aliasing model already uses.
