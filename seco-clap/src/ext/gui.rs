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

use std::cell::{RefCell, UnsafeCell};
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
use seco_core::{EditorPage, Plugin};

use std::sync::atomic::Ordering::Relaxed;

use crate::ext::params::plain_to_display;
use crate::ffi::{
    CLAP_EXT_TIMER_SUPPORT, CLAP_WINDOW_API_COCOA, ClapGuiResizeHints, ClapHost,
    ClapHostTimerSupport, ClapId, ClapPlugin, ClapPluginGui, ClapPluginTimerSupport, ClapWindow,
};
use crate::instance::gui_queue::{EditorMsg, parse_msg};
use crate::instance::{self, Instance};

/// The plugin's page, or `None` when it declares no editor — in which case
/// every entry point here refuses and the host never sees `clap.gui`
/// (`instance::plugin_get_extension`).
///
/// The page itself belongs to the plugin (`Plugin::EDITOR`): this file owns
/// the window, the lifecycle and the parameter plumbing, and knows nothing
/// about what is drawn.
fn page<P: Plugin>() -> Option<EditorPage> {
    P::EDITOR
}

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
    /// Last snippet `Plugin::editor_script` produced. Re-evaluated only when
    /// it changes, so a plugin can return the same string every tick (the
    /// comparison happens here; the wire stays quiet).
    last_script: RefCell<Option<String>>,
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
    publish_state: unsafe fn(*const ClapPlugin, &[u8]),
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
            unsafe {
                match msg {
                    EditorMsg::Param(msg) => {
                        (self.ivars().enqueue)(self.ivars().plugin, msg)
                    }
                    EditorMsg::State(block) => {
                        (self.ivars().publish_state)(self.ivars().plugin, block.as_bytes())
                    }
                }
            };
        }
    }
);

/// Assembles the gui vtable for a concrete plugin type.
pub(crate) struct GuiImpl<P>(PhantomData<P>);

impl<P: Plugin> GuiImpl<P> {
    pub(crate) const VTABLE: ClapPluginGui = ClapPluginGui {
        is_api_supported: is_api_supported::<P>,
        get_preferred_api,
        create: create::<P>,
        destroy: destroy::<P>,
        set_scale,
        get_size: get_size::<P>,
        can_resize,
        get_resize_hints,
        adjust_size,
        set_size: set_size::<P>,
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

/// `ext/gui.h:108-111` `[main-thread]`. Embedded cocoa only, and only for a
/// plugin that actually declares a page.
unsafe extern "C" fn is_api_supported<P: Plugin>(
    _plugin: *const ClapPlugin,
    api: *const c_char,
    is_floating: bool,
) -> bool {
    if api.is_null() || is_floating || page::<P>().is_none() {
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
    let Some(page) = page::<P>() else {
        return false;
    };
    // clap.gui is [main-thread]; if a host violates that, refuse rather
    // than let AppKit abort us.
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    // Reopen robustness: a second create() without destroy() replaces the
    // old editor instead of leaking or double-attaching it.
    drop_handle(slot);

    let frame = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(f64::from(page.width), f64::from(page.height)),
    );
    // SAFETY (objc2 contract): default-constructing a WKWebViewConfiguration
    // on the main thread, immediately consumed by the webview init below.
    let config = unsafe { WKWebViewConfiguration::new(mtm) };
    // JS -> Rust bridge: the page posts to webkit.messageHandlers.seco.
    let handler = ParamMessageHandler::alloc(mtm).set_ivars(HandlerIvars {
        plugin,
        enqueue: instance::queue_gui_param_change::<P>,
        publish_state: instance::publish_editor_state::<P>,
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
    unsafe { webview.loadHTMLString_baseURL(&NSString::from_str(page.html), None) };

    // Refresh timer via clap.timer-support: the host calls on_timer() on the
    // main thread; 33 ms ~ the 30 Hz the header promises hosts allow
    // (ext/timer-support.h:19). No timer -> no live updates, degraded not
    // broken.
    let host = unsafe { instance::shared::<P>(plugin) }.host;
    let (host_timer, timer_id) = register_refresh_timer(host);

    *slot = Some(GuiHandle {
        webview,
        controller,
        host,
        host_timer,
        timer_id,
        last_script: RefCell::new(None),
    });
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
unsafe extern "C" fn get_size<P: Plugin>(
    _plugin: *const ClapPlugin,
    width: *mut u32,
    height: *mut u32,
) -> bool {
    let Some(page) = page::<P>() else {
        return false;
    };
    if width.is_null() || height.is_null() {
        return false;
    }
    // SAFETY: host out-params valid for one write each.
    unsafe {
        width.write(page.width);
        height.write(page.height);
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
unsafe extern "C" fn set_size<P: Plugin>(
    _plugin: *const ClapPlugin,
    width: u32,
    height: u32,
) -> bool {
    page::<P>().is_some_and(|page| width == page.width && height == page.height)
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
    let inst = unsafe { instance::shared::<P>(plugin) };
    push_params::<P>(handle, inst);
    push_script::<P>(handle, inst);
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
    let inst = unsafe { instance::shared::<P>(plugin) };
    push_params::<P>(handle, inst);
    push_script::<P>(handle, inst);
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

/// Evaluates whatever the plugin wants drawn beyond the parameter values
/// ([`Plugin::editor_script`]), skipping the call into the page when the
/// snippet is unchanged since the last push.
fn push_script<P: Plugin>(handle: &GuiHandle, inst: &Instance<P>) {
    let mut values = [0.0_f64; crate::MAX_PARAMS];
    for (slot, bits) in values.iter_mut().zip(&inst.param_bits).take(P::PARAMS.len()) {
        *slot = f64::from_bits(bits.load(Relaxed));
    }
    // The page draws from the same block the audio thread applies.
    // SAFETY: `[main-thread]` — every gui callback is, and the timer's
    // on_timer with them (ext/timer-support.h:12).
    let script = unsafe {
        inst.plugin_state
            .with_latest(|block| P::editor_script(&values[..P::PARAMS.len()], block))
    };
    let Some(script) = script else {
        return;
    };
    let mut last = handle.last_script.borrow_mut();
    if last.as_deref() == Some(script.as_str()) {
        return;
    }
    // SAFETY (objc2 contract): main thread; completion handler omitted.
    unsafe {
        handle
            .webview
            .evaluateJavaScript_completionHandler(&NSString::from_str(&script), None);
    }
    // Only remember what the page could actually have received: create() ->
    // show() beats WebKit's load, evaluating into a half-loaded page is a
    // silent no-op, and a remembered snippet is never re-sent. Unchanged
    // output on the next tick then repeats the push instead of losing it.
    // SAFETY (objc2 contract): reading a WKWebView property on the main
    // thread.
    if !unsafe { handle.webview.isLoading() } {
        *last = Some(script);
    }
}
