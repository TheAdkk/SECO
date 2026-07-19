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

use objc2::{MainThreadMarker, MainThreadOnly};
use objc2::rc::Retained;
use objc2_app_kit::NSAutoresizingMaskOptions;
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use objc2_web_kit::{WKWebView, WKWebViewConfiguration};
use seco_core::Plugin;

use crate::ffi::{
    CLAP_WINDOW_API_COCOA, ClapGuiResizeHints, ClapPlugin, ClapPluginGui, ClapWindow,
};
use crate::instance;

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
    display: grid;
    place-items: center;
    background: #1d2021;
    font-family: -apple-system, sans-serif;
    user-select: none;
    -webkit-user-select: none;
  }
  h1 {
    color: #fabd2f;
    font-size: 64px;
    letter-spacing: 0.08em;
    margin: 0;
  }
</style>
</head>
<body><h1>Zape</h1></body>
</html>"#;

/// The live editor. Main-thread-only by construction (`Retained<WKWebView>`
/// is `!Send`), which matches clap.gui's threading contract.
pub(crate) struct GuiHandle {
    webview: Retained<WKWebView>,
}

/// The GUI slot stored on `Instance`. `UnsafeCell` for the same reason as
/// the plugin state: shared `&Instance` everywhere, with exclusivity
/// per callback class — here, the `[main-thread]` gui callbacks.
pub(crate) type GuiSlot = UnsafeCell<Option<GuiHandle>>;

pub(crate) fn empty_slot() -> GuiSlot {
    UnsafeCell::new(None)
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

    *slot = Some(GuiHandle { webview });
    true
}

/// Detaches and drops the editor if present. Idempotent.
fn drop_handle(slot: &mut Option<GuiHandle>) {
    if let Some(handle) = slot.take() {
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
