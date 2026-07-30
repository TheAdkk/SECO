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

use std::cell::{Cell, RefCell, UnsafeCell};
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
    CLAP_EXT_GUI, CLAP_EXT_TIMER_SUPPORT, CLAP_WINDOW_API_COCOA, ClapGuiResizeHints, ClapHost,
    ClapHostGui, ClapHostTimerSupport, ClapId, ClapPlugin, ClapPluginGui, ClapPluginTimerSupport,
    ClapWindow,
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
    /// Revision of the last state block successfully handed to the page.
    /// Meter-only plugins use it to avoid rebuilding their same settings
    /// script on every visual refresh.
    last_state_revision: Cell<Option<u64>>,
    /// Current logical size. The host owns the window, so this follows what
    /// `set_size` was last told rather than the page's opening size.
    size: Cell<(u32, u32)>,
    /// A few hosts hand us their embedding view before AppKit has laid it
    /// out. Keep our negotiated non-zero frame in that case, then adopt the
    /// parent once it has real bounds instead of permanently showing a
    /// zero-sized WebView.
    pending_parent_layout: Cell<bool>,
    /// A host may synchronously answer `request_resize()` with `set_size()`.
    /// Keep that nested callback from recursively requesting the same size.
    resize_request_active: Cell<bool>,
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
    answer: unsafe fn(*const ClapPlugin, &str),
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
                    EditorMsg::Param(msg) => (self.ivars().enqueue)(self.ivars().plugin, msg),
                    EditorMsg::State(block) => {
                        (self.ivars().publish_state)(self.ivars().plugin, block.as_bytes())
                    }
                    EditorMsg::Message(request) => {
                        (self.ivars().answer)(self.ivars().plugin, request)
                    }
                }
            };
        }
    }
);

/// Runs a page request through [`Plugin::editor_message`] and evaluates the
/// answer back in the page.
///
/// # Safety
///
/// `plugin` must be a live instance, and this must run on the main thread
/// (WebKit delivers script messages there).
unsafe fn answer_editor_message<P: Plugin>(plugin: *const ClapPlugin, request: &str) {
    let Some(answer) = P::editor_message(request) else {
        return;
    };
    // SAFETY: fn contract of gui_slot (main thread, serialized with every
    // other gui callback).
    let Some(handle) = (unsafe { gui_slot::<P>(plugin) }).as_ref() else {
        return;
    };
    // SAFETY (objc2 contract): main thread; completion handler omitted.
    unsafe {
        handle
            .webview
            .evaluateJavaScript_completionHandler(&NSString::from_str(&answer), None);
    }
}

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
        can_resize: can_resize::<P>,
        get_resize_hints: get_resize_hints::<P>,
        adjust_size: adjust_size::<P>,
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
        answer: answer_editor_message::<P>,
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
    let webview =
        unsafe { WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &config) };
    // Resize with whatever container the host puts us in.
    webview.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    // SAFETY (objc2 contract): plain HTML string load, no base URL.
    unsafe { webview.loadHTMLString_baseURL(&NSString::from_str(page.html), None) };

    // Refresh timer via clap.timer-support: the host calls on_timer() on the
    // main thread. Each plugin asks for its own bounded refresh rate; a page
    // can still choose a lower local draw rate. No timer -> no live updates,
    // degraded not broken.
    let host = unsafe { instance::shared::<P>(plugin) }.host;
    let (host_timer, timer_id) = register_refresh_timer::<P>(host);

    *slot = Some(GuiHandle {
        webview,
        controller,
        host,
        host_timer,
        timer_id,
        last_script: RefCell::new(None),
        last_state_revision: Cell::new(None),
        size: Cell::new((page.width, page.height)),
        pending_parent_layout: Cell::new(false),
        resize_request_active: Cell::new(false),
    });
    true
}

/// Lowest and highest host-refresh requests we accept. The CLAP header asks
/// hosts to allow 30 Hz; 120 Hz is enough headroom for smooth meters without
/// turning an accidental giant constant into a main-thread busy loop.
const MIN_EDITOR_REFRESH_HZ: u32 = 1;
const MAX_EDITOR_REFRESH_HZ: u32 = 120;

/// Converts a plugin's requested feed rate into a whole millisecond period.
/// Hosts own final cadence; integer milliseconds choose the nearest useful
/// request (30 Hz -> 33 ms, 60 Hz -> 16 ms). Kept pure for regression tests.
fn editor_refresh_period_ms(hz: u32) -> u32 {
    let hz = hz.clamp(MIN_EDITOR_REFRESH_HZ, MAX_EDITOR_REFRESH_HZ);
    1_000_u32 / hz
}

/// Looks up clap.timer-support on the host and registers a bounded plugin
/// refresh timer.
/// SAFETY of the calls: [main-thread] (we are inside a gui callback), and
/// host extension pointers are valid until destroy (host.h:20-25).
fn register_refresh_timer<P: Plugin>(
    host: *const ClapHost,
) -> (*const ClapHostTimerSupport, Option<ClapId>) {
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
    let ok = unsafe {
        ((*vtable).register_timer)(
            host,
            editor_refresh_period_ms(P::EDITOR_REFRESH_HZ),
            &raw mut timer_id,
        )
    };
    if ok {
        (vtable, Some(timer_id))
    } else {
        (std::ptr::null(), None)
    }
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
    plugin: *const ClapPlugin,
    width: *mut u32,
    height: *mut u32,
) -> bool {
    let Some(page) = page::<P>() else {
        return false;
    };
    if width.is_null() || height.is_null() {
        return false;
    }
    // The size the editor is *at*, not the size it opened at — a host that
    // resized us and then asks would otherwise be told to undo the resize.
    // SAFETY: fn contract of gui_slot.
    let (current_width, current_height) = unsafe { gui_slot::<P>(plugin) }
        .as_ref()
        .map_or((page.width, page.height), |handle| handle.size.get());
    // SAFETY: host out-params valid for one write each.
    unsafe {
        width.write(current_width);
        height.write(current_height);
    }
    true
}

/// Clamps a requested size to what the page says it can do.
///
/// A fixed-size page reports its one size for any request, which is what
/// makes `adjust_size` and `set_size` agree with `can_resize`.
fn clamp_to_page(page: EditorPage, width: u32, height: u32) -> (u32, u32) {
    match page.minimum {
        Some((minimum_width, minimum_height)) => {
            (width.max(minimum_width), height.max(minimum_height))
        }
        None => (page.width, page.height),
    }
}

fn frame_for_size((width, height): (u32, u32)) -> NSRect {
    NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(f64::from(width), f64::from(height)),
    )
}

/// Chooses the host's frame only after AppKit has given it a drawable size.
///
/// A zero-sized parent is a transient host layout state, not a valid editor
/// size. Returning the negotiated frame keeps WebKit alive until `show()` or
/// the refresh timer can observe the real parent bounds.
fn parent_frame_or_fallback(parent_bounds: NSRect, fallback: (u32, u32)) -> (NSRect, bool) {
    let size = parent_bounds.size;
    if size.width.is_finite() && size.height.is_finite() && size.width > 0.0 && size.height > 0.0 {
        (parent_bounds, false)
    } else {
        (frame_for_size(fallback), true)
    }
}

/// Returns the smallest size an already-attached responsive editor needs.
///
/// `0×0` is a normal transient AppKit layout state, handled separately by
/// `pending_parent_layout`; asking the host to resize it would create a
/// pointless request during every open.
fn parent_minimum_resize(page: EditorPage, parent_bounds: NSRect) -> Option<(u32, u32)> {
    let (minimum_width, minimum_height) = page.minimum?;
    let size = parent_bounds.size;
    if !size.width.is_finite()
        || !size.height.is_finite()
        || size.width <= 0.0
        || size.height <= 0.0
        || size.width > f64::from(u32::MAX)
        || size.height > f64::from(u32::MAX)
    {
        return None;
    }
    let current = (size.width.ceil() as u32, size.height.ceil() as u32);
    let target = (current.0.max(minimum_width), current.1.max(minimum_height));
    (target != current).then_some(target)
}

/// Requests a client-area resize through the optional host GUI extension.
///
/// The extension lookup is legal after `plugin.init()` and the returned
/// pointer is valid until `plugin.destroy()` (`host.h:20-25`).
fn request_host_resize(host: *const ClapHost, size: (u32, u32)) -> bool {
    if host.is_null() {
        return false;
    }
    // SAFETY: `host` is live for this plugin instance; extension lookup is
    // [thread-safe] and this runs after init.
    let extension = unsafe { ((*host).get_extension)(host, CLAP_EXT_GUI.as_ptr()) };
    if extension.is_null() {
        return false;
    }
    let gui = extension.cast::<ClapHostGui>();
    // SAFETY: non-null pointer returned for `clap.gui` has this vtable layout;
    // the callback is [thread-safe & !floating] and this editor is embedded.
    unsafe { ((*gui).request_resize)(host, size.0, size.1) }
}

/// Tries a host-mediated recovery for a stale size below the page minimum.
///
/// No `GuiHandle` borrow lives across the host call: some hosts reply
/// synchronously with `set_size()`, which re-enters the adapter.
fn request_clamped_size<P: Plugin>(plugin: *const ClapPlugin, target: (u32, u32)) -> bool {
    let host = {
        // SAFETY: fn contract of gui_slot.
        let slot = unsafe { gui_slot::<P>(plugin) };
        let Some(handle) = slot.as_ref() else {
            return false;
        };
        if handle.resize_request_active.replace(true) {
            return false;
        }
        handle.host
    };

    let accepted = request_host_resize(host, target);

    // SAFETY: fn contract of gui_slot. The host may have re-entered
    // `set_size()`, but the callback has returned before this fresh borrow.
    let slot = unsafe { gui_slot::<P>(plugin) };
    let Some(handle) = slot.as_ref() else {
        return false;
    };
    handle.resize_request_active.set(false);
    if accepted {
        // CLAP explicitly permits a host to accept this request without
        // calling set_size() back, so make the editor match the accepted
        // client size ourselves.
        handle.size.set(target);
        handle.webview.setFrame(frame_for_size(target));
    }
    accepted
}

/// Completes a deferred parent layout without ever replacing a good
/// negotiated frame with a zero-sized one.
fn resolve_pending_parent_layout(handle: &GuiHandle) -> Option<NSRect> {
    if !handle.pending_parent_layout.get() {
        return None;
    }
    // SAFETY: the webview remains attached until `drop_handle()` removes it;
    // this is called only from serialized main-thread GUI callbacks.
    // SAFETY: `superview()` returns an optional retained parent while the
    // webview remains attached on this serialized main-thread path.
    let parent = unsafe { handle.webview.superview() }?;
    let bounds = parent.bounds();
    let (frame, still_pending) = parent_frame_or_fallback(bounds, handle.size.get());
    if !still_pending {
        handle.webview.setFrame(frame);
        handle.pending_parent_layout.set(false);
        return Some(bounds);
    }
    None
}

/// Whether a page made the explicit responsive-layout promise.
fn page_is_resizable(page: Option<EditorPage>) -> bool {
    page.is_some_and(|page| page.minimum.is_some())
}

/// `ext/gui.h:161-163` `[main-thread & !floating]`. A page that declares a
/// minimum is telling us it survives being resized.
unsafe extern "C" fn can_resize<P: Plugin>(_plugin: *const ClapPlugin) -> bool {
    page_is_resizable(page::<P>())
}

/// `ext/gui.h:165-167` `[main-thread & !floating]`.
unsafe extern "C" fn get_resize_hints<P: Plugin>(
    _plugin: *const ClapPlugin,
    hints: *mut ClapGuiResizeHints,
) -> bool {
    if hints.is_null() || !page_is_resizable(page::<P>()) {
        return false;
    }
    // Both axes, no locked aspect ratio: the layout reflows rather than
    // scaling, so a wider window means more room, not bigger pixels.
    // SAFETY: host out-param valid for one write.
    unsafe {
        hints.write(ClapGuiResizeHints {
            can_resize_horizontally: true,
            can_resize_vertically: true,
            preserve_aspect_ratio: false,
            aspect_ratio_width: 0,
            aspect_ratio_height: 0,
        });
    }
    true
}

/// `ext/gui.h:169-175` `[main-thread & !floating]`. The host asks what it
/// would actually get before it commits to a drag.
unsafe extern "C" fn adjust_size<P: Plugin>(
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
    // SAFETY: host in/out-params valid for one read and one write each.
    unsafe {
        let (adjusted_width, adjusted_height) = clamp_to_page(page, width.read(), height.read());
        width.write(adjusted_width);
        height.write(adjusted_height);
    }
    true
}

/// `ext/gui.h:177-181` `[main-thread & !floating]`.
unsafe extern "C" fn set_size<P: Plugin>(
    plugin: *const ClapPlugin,
    width: u32,
    height: u32,
) -> bool {
    let Some(page) = page::<P>() else {
        return false;
    };
    let adjusted = clamp_to_page(page, width, height);
    if adjusted != (width, height) {
        // A known session size bypasses adjust_size() in the CLAP lifecycle.
        // Do not silently take a different size: request the closest legal
        // client area from the host, which may accept without set_size().
        return request_clamped_size::<P>(plugin, adjusted);
    }
    // SAFETY: fn contract of gui_slot.
    let Some(handle) = (unsafe { gui_slot::<P>(plugin) }).as_ref() else {
        // No view yet: remember nothing, but the size is legal.
        return true;
    };
    handle.size.set((width, height));
    // The autoresizing mask keeps the view following its parent, but a host
    // that resizes the parent without telling AppKit — or that asks us
    // first — needs the frame set here too.
    handle.webview.setFrame(frame_for_size((width, height)));
    true
}

/// `ext/gui.h:183-187` `[main-thread & !floating]`. The window handle for
/// cocoa is an `NSView *` (ext/gui.h:75, :83).
unsafe extern "C" fn set_parent<P: Plugin>(
    plugin: *const ClapPlugin,
    window: *const ClapWindow,
) -> bool {
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
    let parent_resize = {
        // SAFETY: fn contract of gui_slot.
        let slot = unsafe { gui_slot::<P>(plugin) };
        let Some(handle) = slot.as_ref() else {
            return false;
        };
        // SAFETY: valid NSView per the host contract above. The reference is
        // used only in this serialized main-thread callback.
        let parent = unsafe { &*parent.cast::<objc2_app_kit::NSView>() };
        let bounds = parent.bounds();
        let (frame, layout_pending) = parent_frame_or_fallback(bounds, handle.size.get());
        // `addSubview` re-parents if we were attached elsewhere. Attach before
        // assigning the frame: a host that has already laid out its parent is
        // filled immediately, while a temporary 0×0 parent retains the editor's
        // negotiated frame until it becomes drawable.
        parent.addSubview(&handle.webview);
        handle.webview.setFrame(frame);
        handle.pending_parent_layout.set(layout_pending);
        page::<P>().and_then(|page| parent_minimum_resize(page, bounds))
    };
    if let Some(target) = parent_resize {
        let _ = request_clamped_size::<P>(plugin, target);
    }
    true
}

/// `ext/gui.h:189-193` `[main-thread & floating]`. We never float.
unsafe extern "C" fn set_transient(_plugin: *const ClapPlugin, _window: *const ClapWindow) -> bool {
    false
}

/// `ext/gui.h:195-198` `[main-thread & floating]`. We never float.
unsafe extern "C" fn suggest_title(_plugin: *const ClapPlugin, _title: *const c_char) {}

/// `ext/gui.h:200-204` `[main-thread]`.
unsafe extern "C" fn show<P: Plugin>(plugin: *const ClapPlugin) -> bool {
    let parent_resize = {
        // SAFETY: fn contract of gui_slot.
        let slot = unsafe { gui_slot::<P>(plugin) };
        let Some(handle) = slot.as_ref() else {
            return false;
        };
        let parent_resize = resolve_pending_parent_layout(handle)
            .and_then(|bounds| page::<P>().and_then(|page| parent_minimum_resize(page, bounds)));
        handle.webview.setHidden(false);
        // Push once right away; if the page hasn't finished loading this is a
        // harmless no-op and the timer covers it a tick later.
        // SAFETY: live instance per `instance::shared`'s contract.
        let inst = unsafe { instance::shared::<P>(plugin) };
        push_params::<P>(handle, inst);
        push_script::<P>(handle, inst);
        push_frame::<P>(handle, inst);
        parent_resize
    };
    if let Some(target) = parent_resize {
        let _ = request_clamped_size::<P>(plugin, target);
    }
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
    pub(crate) const VTABLE: ClapPluginTimerSupport = ClapPluginTimerSupport {
        on_timer: on_timer::<P>,
    };
    pub(crate) const VTABLE_REF: &'static ClapPluginTimerSupport = &Self::VTABLE;
}

/// `ext/timer-support.h:11-14` `[main-thread]`.
unsafe extern "C" fn on_timer<P: Plugin>(plugin: *const ClapPlugin, timer_id: ClapId) {
    let parent_resize = {
        // SAFETY: fn contract of gui_slot ([main-thread], serialized).
        let slot = unsafe { gui_slot::<P>(plugin) };
        let Some(handle) = slot.as_ref() else {
            return;
        };
        if handle.timer_id != Some(timer_id) {
            return;
        }
        let parent_resize = resolve_pending_parent_layout(handle)
            .and_then(|bounds| page::<P>().and_then(|page| parent_minimum_resize(page, bounds)));
        // SAFETY: live instance per `instance::shared`'s contract.
        let inst = unsafe { instance::shared::<P>(plugin) };
        push_params::<P>(handle, inst);
        push_script::<P>(handle, inst);
        push_frame::<P>(handle, inst);
        parent_resize
    };
    if let Some(target) = parent_resize {
        let _ = request_clamped_size::<P>(plugin, target);
    }
}

/// Reads the SAME atomics the host's get_value reads (`param_bits` — one
/// source of truth) and pushes them into the page. Main thread: the format!
/// allocations here never touch the audio path.
fn push_params<P: Plugin>(handle: &GuiHandle, inst: &Instance<P>) {
    // Meter-only plugins such as Neta expose no automatable parameters.
    // Avoid a pointless cross-process WebKit evaluation every refresh.
    if P::PARAMS.is_empty() {
        return;
    }
    use seco_core::ParamRange;
    let mut json = String::from("[");
    for (index, desc) in P::PARAMS.iter().enumerate() {
        let value = f64::from_bits(inst.param_bits[index].load(Relaxed));
        let text = plain_to_display(&desc.range, value).unwrap_or_else(|| "?".to_string());
        let (min, max) = (desc.range.min(), desc.range.max());
        let norm = if max > min {
            ((value - min) / (max - min)).clamp(0.0, 1.0)
        } else {
            0.0
        };
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
    let state_revision = inst.plugin_state.latest_revision();
    if P::PARAMS.is_empty() && handle.last_state_revision.get() == Some(state_revision) {
        return;
    }
    let mut values = [0.0_f64; crate::MAX_PARAMS];
    for (slot, bits) in values
        .iter_mut()
        .zip(&inst.param_bits)
        .take(P::PARAMS.len())
    {
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
        // SAFETY (objc2 contract): main thread.
        if !unsafe { handle.webview.isLoading() } {
            handle.last_state_revision.set(Some(state_revision));
        }
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
        handle.last_state_revision.set(Some(state_revision));
    }
}

/// Pushes the plugin's per-refresh snippet — the picture of the audio.
///
/// Deliberately *not* deduplicated: scope buckets change every block, so
/// comparing would cost more than sending (`Plugin::editor_frame`).
fn push_frame<P: Plugin>(handle: &GuiHandle, inst: &Instance<P>) {
    let mut buckets = [0.0_f32; crate::MAX_SCOPE_BUCKETS];
    for (slot, published) in buckets[..P::SCOPE_SLOTS]
        .iter_mut()
        .zip(&inst.scope[..P::SCOPE_SLOTS])
    {
        *slot = f32::from_bits(published.load(Relaxed));
    }
    let Some(frame) = P::editor_frame(&buckets[..P::SCOPE_SLOTS]) else {
        return;
    };
    // SAFETY (objc2 contract): main thread; completion handler omitted.
    unsafe {
        handle
            .webview
            .evaluateJavaScript_completionHandler(&NSString::from_str(&frame), None);
    }
}

#[cfg(test)]
mod resize_tests {
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    use super::{
        EditorPage, clamp_to_page, editor_refresh_period_ms, frame_for_size, page_is_resizable,
        parent_frame_or_fallback, parent_minimum_resize,
    };

    const FIXED: EditorPage = EditorPage {
        html: "",
        width: 760,
        height: 470,
        minimum: None,
    };
    const RESPONSIVE: EditorPage = EditorPage {
        html: "",
        width: 1_180,
        height: 720,
        minimum: Some((640, 300)),
    };

    #[test]
    fn fixed_page_only_accepts_its_opening_size() {
        assert!(!page_is_resizable(Some(FIXED)));
        assert_eq!(clamp_to_page(FIXED, 1, 1), (760, 470));
        assert_eq!(clamp_to_page(FIXED, 2_000, 2_000), (760, 470));
    }

    #[test]
    fn responsive_page_clamps_only_its_minimum() {
        assert!(page_is_resizable(Some(RESPONSIVE)));
        assert!(!page_is_resizable(None));
        assert_eq!(clamp_to_page(RESPONSIVE, 1, 1), (640, 300));
        assert_eq!(clamp_to_page(RESPONSIVE, 1_600, 900), (1_600, 900));
    }

    #[test]
    fn zero_sized_parent_keeps_the_negotiated_frame() {
        let parent = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 720.0));
        let (frame, pending) = parent_frame_or_fallback(parent, (1_180, 720));

        assert!(pending);
        assert_eq!(frame, frame_for_size((1_180, 720)));
    }

    #[test]
    fn drawable_parent_is_filled_verbatim() {
        let parent = NSRect::new(NSPoint::new(7.0, 11.0), NSSize::new(900.0, 480.0));
        let (frame, pending) = parent_frame_or_fallback(parent, (1_180, 720));

        assert!(!pending);
        assert_eq!(frame, parent);
    }

    #[test]
    fn parent_smaller_than_minimum_requests_only_missing_axis() {
        let parent = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(900.0, 240.0));

        assert_eq!(parent_minimum_resize(RESPONSIVE, parent), Some((900, 300)));
    }

    #[test]
    fn zero_sized_parent_never_requests_a_resize() {
        let parent = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 240.0));

        assert_eq!(parent_minimum_resize(RESPONSIVE, parent), None);
    }

    #[test]
    fn editor_refresh_rate_is_bounded_and_uses_whole_milliseconds() {
        assert_eq!(editor_refresh_period_ms(0), 1_000);
        assert_eq!(editor_refresh_period_ms(30), 33);
        assert_eq!(editor_refresh_period_ms(60), 16);
        assert_eq!(editor_refresh_period_ms(120), 8);
        assert_eq!(editor_refresh_period_ms(10_000), 8);
    }
}
