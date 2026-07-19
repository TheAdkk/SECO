//! `clap.gui`: an embedded editor hosted in a WKWebView (macOS/cocoa only).
//!
//! Every plugin-side method here is `[main-thread]` (ext/gui.h:107-212), so
//! the GUI slot in `Instance` follows the same per-callback-class
//! exclusivity model as the plugin state: it is only ever touched from
//! these callbacks, which CLAP serializes on one thread.
//!
//! # Reopen robustness
//!
//! The classic plugin-GUI bug is dying on the second open. The lifecycle
//! here is defensive at every step so any create → set_parent → show →
//! hide → destroy sequence can repeat, in whole or in part:
//! - `create()` on an already-created GUI first destroys the old one.
//! - `destroy()` on a destroyed (or never-created) GUI is a no-op.
//! - `set_parent()` re-parents cleanly (AppKit's `addSubview` moves a view
//!   that already has a superview).
//! - `destroy()` detaches from the parent before dropping the webview.

use std::cell::UnsafeCell;
use std::ffi::{CStr, c_char};
use std::marker::PhantomData;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::NSAutoresizingMaskOptions;
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use objc2_web_kit::{
    WKScriptMessage, WKScriptMessageHandler, WKUserContentController, WKWebView,
    WKWebViewConfiguration,
};
use seco_core::Plugin;

use std::sync::atomic::Ordering::Relaxed;

use crate::ext::params::plain_to_display;
use crate::ffi::{
    CLAP_EXT_TIMER_SUPPORT, CLAP_WINDOW_API_COCOA, ClapGuiResizeHints, ClapHost,
    ClapHostTimerSupport, ClapId, ClapPlugin, ClapPluginGui, ClapPluginTimerSupport, ClapWindow,
};
use crate::instance::{self, Instance};

/// Fixed logical size for Phase 6.1 (cocoa is logical-pixel,
/// ext/gui.h:56-57).
const WIDTH: f64 = 480.0;
const HEIGHT: f64 = 320.0;

/// Phase 6.1 page: a colored rectangle and the plugin name. Inline —
/// served from the binary, no files, no network.
const HTML: &str = r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<style>
  html, body {
    margin: 0;
    height: 100%;
    background: #1d2021;
    font-family: -apple-system, sans-serif;
    user-select: none;
    -webkit-user-select: none;
    color: #ebdbb2;
  }
  .wrap { padding: 24px 28px; }
  h1 { color: #fabd2f; font-size: 28px; letter-spacing: 0.08em; margin: 0 0 18px 0; }
  .row { margin: 14px 0; }
  .head { display: flex; justify-content: space-between; font-size: 14px; margin-bottom: 5px; }
  .name { opacity: 0.8; }
  .value { color: #fabd2f; font-variant-numeric: tabular-nums; }
  input[type=range] { width: 100%; accent-color: #fabd2f; margin: 0; }
  select, input[type=checkbox] { accent-color: #fabd2f; }
  select {
    width: 100%; background: #3c3836; color: #ebdbb2;
    border: none; border-radius: 4px; padding: 4px;
  }
</style>
</head>
<body>
<div class="wrap">
  <h1>Zape</h1>
  <div id="params"></div>
</div>
<script>
  const post = (m) => window.webkit.messageHandlers.seco.postMessage(m);
  const container = document.getElementById("params");
  const rows = new Map();

  function buildRow(i, p) {
    const el = document.createElement("div");
    el.className = "row";
    el.innerHTML = '<div class="head"><span class="name"></span>' +
                   '<span class="value"></span></div>';
    el.querySelector(".name").textContent = p.n;
    let control, dragging = { on: false };
    if (p.k === "s") {
      control = document.createElement("select");
      for (let step = 0; step < p.opts.length; step++) {
        const opt = document.createElement("option");
        opt.value = step;
        opt.textContent = p.opts[step];
        control.appendChild(opt);
      }
      // A select change is an atomic gesture.
      control.addEventListener("change", () => {
        post("begin " + i); post("set " + i + " " + control.value); post("end " + i);
      });
    } else if (p.k === "t") {
      control = document.createElement("input");
      control.type = "checkbox";
      control.addEventListener("change", () => {
        post("begin " + i);
        post("set " + i + " " + (control.checked ? 1 : 0));
        post("end " + i);
      });
    } else {
      control = document.createElement("input");
      control.type = "range";
      control.min = 0; control.max = 1000; control.step = 1;
      control.addEventListener("pointerdown", () => { dragging.on = true; post("begin " + i); });
      control.addEventListener("pointerup", () => { dragging.on = false; post("end " + i); });
      control.addEventListener("input", () => {
        const plain = p.min + (control.value / 1000) * (p.max - p.min);
        post("set " + i + " " + plain);
      });
    }
    el.appendChild(control);
    container.appendChild(el);
    return { value: el.querySelector(".value"), control, kind: p.k, dragging };
  }

  // Pushed by Rust ~30 Hz. While the user is dragging a control we skip
  // refreshing it, so the timer echo never fights the pointer.
  window.__seco_update = (list) => {
    list.forEach((p, i) => {
      let row = rows.get(p.n);
      if (!row) { row = buildRow(i, p); rows.set(p.n, row); }
      row.value.textContent = p.t;
      if (row.dragging.on) return;
      if (row.kind === "s") row.control.value = Math.round(p.v * (p.opts.length - 1));
      else if (row.kind === "t") row.control.checked = p.v >= 0.5;
      else row.control.value = Math.round(p.v * 1000);
    });
  };
</script>
</body>
</html>"#;

/// The live editor. Main-thread-only by construction (`Retained<WKWebView>`
/// is `!Send`), which matches clap.gui's threading contract.
pub(crate) struct GuiHandle {
    webview: Retained<WKWebView>,
    /// Kept to remove the script message handler on destroy — the content
    /// controller retains its handlers, and unhooking before teardown means
    /// no message can ever reach a dying plugin.
    controller: Retained<WKUserContentController>,
    /// Host + its timer vtable, kept to unregister on destroy. Null/None if
    /// the host lacks clap.timer-support (the view then shows open-time
    /// values only).
    host: *const ClapHost,
    host_timer: *const ClapHostTimerSupport,
    timer_id: Option<ClapId>,
}

/// The GUI slot stored on `Instance`. `UnsafeCell` for the same reason as
/// the plugin state: shared `&Instance` everywhere, with exclusivity
/// per callback class — here, the `[main-thread]` gui callbacks.
pub(crate) type GuiSlot = UnsafeCell<Option<GuiHandle>>;

pub(crate) fn empty_slot() -> GuiSlot {
    UnsafeCell::new(None)
}

/// Ivars of the JS->Rust bridge object. The handler cannot be generic
/// (ObjC classes aren't), so it carries a monomorphized enqueue fn pointer
/// picked at create() time.
pub(crate) struct HandlerIvars {
    plugin: *const ClapPlugin,
    enqueue: unsafe fn(*const ClapPlugin, instance::gui_queue::GuiMsg),
}

define_class!(
    /// Receives `webkit.messageHandlers.seco.postMessage(...)` calls from
    /// the page. WebKit delivers these on the main thread, one at a time —
    /// the same serialized class as every other gui callback.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SecoParamMessageHandler"]
    #[ivars = HandlerIvars]
    struct ParamMessageHandler;

    unsafe impl NSObjectProtocol for ParamMessageHandler {}

    unsafe impl WKScriptMessageHandler for ParamMessageHandler {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive(&self, _controller: &WKUserContentController, message: &WKScriptMessage) {
            // SAFETY (objc2 contract): body() returns the message payload.
            let body = unsafe { message.body() };
            let Some(text) = body.downcast_ref::<NSString>().map(|s| s.to_string()) else {
                return;
            };
            let Some(msg) = parse_msg(&text) else {
                return;
            };
            // SAFETY: the handler is unhooked before the gui (and long
            // before the plugin) is destroyed, so `plugin` is live; we are
            // on the main thread (WebKit contract).
            unsafe { (self.ivars().enqueue)(self.ivars().plugin, msg) };
        }
    }
);

/// Wire format from JS, deliberately dumb: "begin <i>", "set <i> <plain>",
/// "end <i>".
fn parse_msg(text: &str) -> Option<instance::gui_queue::GuiMsg> {
    use instance::gui_queue::GuiMsg;
    let mut parts = text.split_ascii_whitespace();
    let verb = parts.next()?;
    let index: usize = parts.next()?.parse().ok()?;
    match verb {
        "begin" => Some(GuiMsg::GestureBegin(index)),
        "end" => Some(GuiMsg::GestureEnd(index)),
        "set" => {
            let value: f64 = parts.next()?.parse().ok()?;
            value.is_finite().then_some(GuiMsg::Set(index, value))
        }
        _ => None,
    }
}

/// Assembles the gui vtable for a concrete plugin type.
pub(crate) struct GuiImpl<P>(PhantomData<P>);

impl<P: Plugin> GuiImpl<P> {
    pub(crate) const VTABLE: ClapPluginGui = ClapPluginGui {
        is_api_supported,
        get_preferred_api,
        create: create::<P>,
        destroy: destroy::<P>,
        set_scale,
        get_size,
        can_resize,
        get_resize_hints,
        adjust_size,
        set_size,
        set_parent: set_parent::<P>,
        set_transient,
        suggest_title,
        show: show::<P>,
        hide: hide::<P>,
    };
    pub(crate) const VTABLE_REF: &'static ClapPluginGui = &Self::VTABLE;
}

/// Borrows the GUI slot. SAFETY contract: call only from clap.gui
/// callbacks — all `[main-thread]`, serialized by the host — so the `&mut`
/// through the UnsafeCell is exclusive.
unsafe fn gui_slot<'a, P: Plugin>(plugin: *const ClapPlugin) -> &'a mut Option<GuiHandle> {
    // SAFETY: live instance per `instance::shared`'s contract; exclusivity
    // per this function's contract.
    unsafe { &mut *instance::shared::<P>(plugin).gui.get() }
}

/// `ext/gui.h:108-111` `[main-thread]`. Embedded cocoa only.
unsafe extern "C" fn is_api_supported(
    _plugin: *const ClapPlugin,
    api: *const c_char,
    is_floating: bool,
) -> bool {
    if api.is_null() || is_floating {
        return false;
    }
    // SAFETY: host passes one of its NUL-terminated api strings.
    (unsafe { CStr::from_ptr(api) }) == CLAP_WINDOW_API_COCOA
}

/// `ext/gui.h:113-120` `[main-thread]`. The api pointer must be one of the
/// CLAP constants, not a copy (ext/gui.h:115-116).
unsafe extern "C" fn get_preferred_api(
    _plugin: *const ClapPlugin,
    api: *mut *const c_char,
    is_floating: *mut bool,
) -> bool {
    if api.is_null() || is_floating.is_null() {
        return false;
    }
    // SAFETY: host out-params valid for one write each.
    unsafe {
        api.write(CLAP_WINDOW_API_COCOA.as_ptr());
        is_floating.write(false);
    }
    true
}

/// `ext/gui.h:122-135` `[main-thread]`.
unsafe extern "C" fn create<P: Plugin>(
    plugin: *const ClapPlugin,
    api: *const c_char,
    is_floating: bool,
) -> bool {
    // SAFETY: fn contract of gui_slot (this is a [main-thread] gui callback).
    let slot = unsafe { gui_slot::<P>(plugin) };
    // SAFETY: as in is_api_supported.
    if is_floating || api.is_null() || unsafe { CStr::from_ptr(api) } != CLAP_WINDOW_API_COCOA {
        return false;
    }
    // clap.gui is [main-thread]; if a host violates that, refuse rather
    // than let AppKit abort us.
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    // Reopen robustness: a second create() without destroy() replaces the
    // old editor instead of leaking or double-attaching it.
    drop_handle(slot);

    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, HEIGHT));
    // SAFETY (objc2 contract): default-constructing a WKWebViewConfiguration
    // on the main thread, immediately consumed by the webview init below.
    let config = unsafe { WKWebViewConfiguration::new(mtm) };
    // JS -> Rust bridge: the page posts to webkit.messageHandlers.seco.
    let handler = ParamMessageHandler::alloc(mtm).set_ivars(HandlerIvars {
        plugin,
        enqueue: instance::queue_gui_param_change::<P>,
    });
    // SAFETY (objc2 contract): plain NSObject init on the allocated object.
    let handler: Retained<ParamMessageHandler> = unsafe { msg_send![super(handler), init] };
    // SAFETY (objc2 contract): the controller retains the handler; name
    // must match the JS side.
    let controller = unsafe { config.userContentController() };
    unsafe {
        controller.addScriptMessageHandler_name(
            ProtocolObject::from_ref(&*handler),
            &NSString::from_str("seco"),
        );
    }
    // SAFETY (objc2 contract): initWithFrame:configuration: on a freshly
    // allocated WKWebView with a valid configuration.
    let webview = unsafe {
        WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &config)
    };
    // Resize with whatever container the host puts us in.
    webview.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    // SAFETY (objc2 contract): plain HTML string load, no base URL.
    unsafe { webview.loadHTMLString_baseURL(&NSString::from_str(HTML), None) };

    // Refresh timer via clap.timer-support: the host calls on_timer() on the
    // main thread; 33 ms ~ the 30 Hz the header promises hosts allow
    // (ext/timer-support.h:19). No timer -> no live updates, degraded not
    // broken.
    let host = unsafe { instance::shared::<P>(plugin) }.host;
    let (host_timer, timer_id) = register_refresh_timer(host);

    *slot = Some(GuiHandle { webview, controller, host, host_timer, timer_id });
    true
}

/// Looks up clap.timer-support on the host and registers a ~30 Hz timer.
/// SAFETY of the calls: [main-thread] (we are inside a gui callback), and
/// host extension pointers are valid until destroy (host.h:20-25).
fn register_refresh_timer(host: *const ClapHost) -> (*const ClapHostTimerSupport, Option<ClapId>) {
    if host.is_null() {
        return (std::ptr::null(), None);
    }
    // SAFETY: live host per the factory contract; get_extension is
    // [thread-safe] and we are past plugin.init() (host.h:20-25).
    let ext = unsafe { ((*host).get_extension)(host, CLAP_EXT_TIMER_SUPPORT.as_ptr()) };
    if ext.is_null() {
        return (std::ptr::null(), None);
    }
    let vtable = ext.cast::<ClapHostTimerSupport>();
    let mut timer_id: ClapId = crate::ffi::CLAP_INVALID_ID;
    // SAFETY: valid vtable from the host; out-param is ours.
    let ok = unsafe { ((*vtable).register_timer)(host, 33, &raw mut timer_id) };
    if ok { (vtable, Some(timer_id)) } else { (std::ptr::null(), None) }
}

/// Detaches, unregisters the refresh timer, and drops the editor if
/// present. Idempotent.
fn drop_handle(slot: &mut Option<GuiHandle>) {
    if let Some(handle) = slot.take() {
        // Unhook JS->Rust first: after this line no message can reach the
        // plugin, whatever the page does while tearing down.
        // SAFETY (objc2 contract): removing by the name registered in
        // create(); main thread.
        unsafe {
            handle
                .controller
                .removeScriptMessageHandlerForName(&NSString::from_str("seco"));
        }
        if let Some(timer_id) = handle.timer_id {
            if !handle.host_timer.is_null() {
                // SAFETY: [main-thread] (gui callback); vtable + host valid
                // until plugin destroy; id came from register_timer.
                unsafe { ((*handle.host_timer).unregister_timer)(handle.host, timer_id) };
            }
        }
        handle.webview.removeFromSuperview();
    }
}

/// `ext/gui.h:137-139` `[main-thread]`. No-op if never created — hosts may
/// call destroy defensively.
unsafe extern "C" fn destroy<P: Plugin>(plugin: *const ClapPlugin) {
    // SAFETY: fn contract of gui_slot.
    let slot = unsafe { gui_slot::<P>(plugin) };
    drop_handle(slot);
}

/// `ext/gui.h:141-152` `[main-thread]`. Cocoa is logical-size: "don't call
/// set_scale" (ext/gui.h:56) — return false, call ignored.
unsafe extern "C" fn set_scale(_plugin: *const ClapPlugin, _scale: f64) -> bool {
    false
}

/// `ext/gui.h:154-159` `[main-thread]`.
unsafe extern "C" fn get_size(
    _plugin: *const ClapPlugin,
    width: *mut u32,
    height: *mut u32,
) -> bool {
    if width.is_null() || height.is_null() {
        return false;
    }
    // SAFETY: host out-params valid for one write each.
    unsafe {
        width.write(WIDTH as u32);
        height.write(HEIGHT as u32);
    }
    true
}

/// `ext/gui.h:161-163` `[main-thread & !floating]`. Fixed size in 6.1.
unsafe extern "C" fn can_resize(_plugin: *const ClapPlugin) -> bool {
    false
}

/// `ext/gui.h:165-167` `[main-thread & !floating]`.
unsafe extern "C" fn get_resize_hints(
    _plugin: *const ClapPlugin,
    _hints: *mut ClapGuiResizeHints,
) -> bool {
    false
}

/// `ext/gui.h:169-175` `[main-thread & !floating]`. Not resizable.
unsafe extern "C" fn adjust_size(
    _plugin: *const ClapPlugin,
    _width: *mut u32,
    _height: *mut u32,
) -> bool {
    false
}

/// `ext/gui.h:177-181` `[main-thread & !floating]`. Only our fixed size
/// "fits".
unsafe extern "C" fn set_size(_plugin: *const ClapPlugin, width: u32, height: u32) -> bool {
    width == WIDTH as u32 && height == HEIGHT as u32
}

/// `ext/gui.h:183-187` `[main-thread & !floating]`. The window handle for
/// cocoa is an `NSView *` (ext/gui.h:75, :83).
unsafe extern "C" fn set_parent<P: Plugin>(
    plugin: *const ClapPlugin,
    window: *const ClapWindow,
) -> bool {
    // SAFETY: fn contract of gui_slot.
    let slot = unsafe { gui_slot::<P>(plugin) };
    let Some(handle) = slot.as_ref() else {
        return false;
    };
    if window.is_null() {
        return false;
    }
    // SAFETY: host passes a valid clap_window for the api it negotiated.
    let window = unsafe { &*window };
    if window.api.is_null()
        // SAFETY: api is a NUL-terminated constant.
        || unsafe { CStr::from_ptr(window.api) } != CLAP_WINDOW_API_COCOA
    {
        return false;
    }
    // SAFETY: for cocoa the union member is the NSView pointer
    // (ext/gui.h:83); the host guarantees it is a valid NSView for the
    // duration of the embedding.
    let parent = unsafe { window.handle.cocoa };
    if parent.is_null() {
        return false;
    }
    let parent = parent.cast::<objc2_app_kit::NSView>();
    // SAFETY: valid NSView per the host contract above; main thread per
    // [main-thread]. addSubview re-parents if we were attached elsewhere.
    unsafe {
        handle.webview.setFrame((*parent).bounds());
        (*parent).addSubview(&handle.webview);
    }
    true
}

/// `ext/gui.h:189-193` `[main-thread & floating]`. We never float.
unsafe extern "C" fn set_transient(
    _plugin: *const ClapPlugin,
    _window: *const ClapWindow,
) -> bool {
    false
}

/// `ext/gui.h:195-198` `[main-thread & floating]`. We never float.
unsafe extern "C" fn suggest_title(_plugin: *const ClapPlugin, _title: *const c_char) {}

/// `ext/gui.h:200-204` `[main-thread]`.
unsafe extern "C" fn show<P: Plugin>(plugin: *const ClapPlugin) -> bool {
    // SAFETY: fn contract of gui_slot.
    let slot = unsafe { gui_slot::<P>(plugin) };
    let Some(handle) = slot.as_ref() else {
        return false;
    };
    handle.webview.setHidden(false);
    // Push once right away; if the page hasn't finished loading this is a
    // harmless no-op and the timer covers it a tick later.
    // SAFETY: live instance per `instance::shared`'s contract.
    push_params::<P>(handle, unsafe { instance::shared::<P>(plugin) });
    true
}

/// `ext/gui.h:206-211` `[main-thread]`.
unsafe extern "C" fn hide<P: Plugin>(plugin: *const ClapPlugin) -> bool {
    // SAFETY: fn contract of gui_slot.
    let slot = unsafe { gui_slot::<P>(plugin) };
    let Some(handle) = slot.as_ref() else {
        return false;
    };
    handle.webview.setHidden(true);
    true
}

/// Timer callbacks — the GUI's refresh clock. `on_timer` shares the gui
/// slot's exclusivity class: it is `[main-thread]`
/// (ext/timer-support.h:12), serialized with every other gui callback.
pub(crate) struct TimerImpl<P>(PhantomData<P>);

impl<P: Plugin> TimerImpl<P> {
    pub(crate) const VTABLE: ClapPluginTimerSupport =
        ClapPluginTimerSupport { on_timer: on_timer::<P> };
    pub(crate) const VTABLE_REF: &'static ClapPluginTimerSupport = &Self::VTABLE;
}

/// `ext/timer-support.h:11-14` `[main-thread]`.
unsafe extern "C" fn on_timer<P: Plugin>(plugin: *const ClapPlugin, timer_id: ClapId) {
    // SAFETY: fn contract of gui_slot ([main-thread], serialized).
    let slot = unsafe { gui_slot::<P>(plugin) };
    let Some(handle) = slot.as_ref() else {
        return;
    };
    if handle.timer_id != Some(timer_id) {
        return;
    }
    // SAFETY: live instance per `instance::shared`'s contract.
    push_params::<P>(handle, unsafe { instance::shared::<P>(plugin) });
}

/// Reads the SAME atomics the host's get_value reads (`param_bits` — one
/// source of truth) and pushes them into the page. Main thread: the format!
/// allocations here never touch the audio path.
fn push_params<P: Plugin>(handle: &GuiHandle, inst: &Instance<P>) {
    use seco_core::ParamRange;
    let mut json = String::from("[");
    for (index, desc) in P::PARAMS.iter().enumerate() {
        let value = f64::from_bits(inst.param_bits[index].load(Relaxed));
        let text = plain_to_display(&desc.range, value).unwrap_or_else(|| "?".to_string());
        let (min, max) = (desc.range.min(), desc.range.max());
        let norm =
            if max > min { ((value - min) / (max - min)).clamp(0.0, 1.0) } else { 0.0 };
        let (kind, opts) = match &desc.range {
            ParamRange::Continuous { .. } => ("c", String::from("[]")),
            ParamRange::Stepped { labels, .. } => {
                let mut list = String::from("[");
                for (li, label) in labels.iter().enumerate() {
                    if li > 0 {
                        list.push(',');
                    }
                    list.push_str(&format!("\"{}\"", escape_js(label)));
                }
                list.push(']');
                ("s", list)
            }
            ParamRange::Toggle { .. } => ("t", String::from("[]")),
        };
        if index > 0 {
            json.push(',');
        }
        json.push_str(&format!(
            "{{\"n\":\"{}\",\"t\":\"{}\",\"v\":{norm:.4},\"k\":\"{kind}\",\"min\":{min},\"max\":{max},\"opts\":{opts}}}",
            escape_js(desc.name),
            escape_js(&text),
        ));
    }
    json.push(']');
    let call = format!("window.__seco_update && window.__seco_update({json});");
    // SAFETY (objc2 contract): main thread; completion handler omitted, we
    // don't need the result (a not-yet-loaded page just ignores the call).
    unsafe {
        handle
            .webview
            .evaluateJavaScript_completionHandler(&NSString::from_str(&call), None);
    }
}

/// Minimal JS string escaping for our own descriptor/label text.
fn escape_js(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}
