/// A plugin's editor: the page and its size.
///
/// The page is a self-contained HTML document served from the binary — no
/// files, no network. The adapter hosts it in whatever webview the platform
/// provides and gives it two entry points:
///
/// - the page posts `"begin <i>"`, `"set <i> <plain>"`, `"end <i>"` strings
///   to the host object the adapter installs, which turn into CLAP parameter
///   gestures;
/// - the adapter calls `window.__seco_update(list)` at the refresh rate with
///   one entry per parameter (`n`ame, display `t`ext, normalized `v`alue,
///   `k`ind, `min`, `max`, `opts`), plus whatever
///   [`Plugin::editor_script`](crate::Plugin::editor_script) returns.
///
/// This type carries no platform types: it is a description, and
/// `seco-core` stays FFI-free.
#[derive(Clone, Copy, Debug)]
pub struct EditorPage {
    /// The complete HTML document.
    pub html: &'static str,
    /// Logical width in pixels. The editor is fixed-size for now.
    pub width: u32,
    /// Logical height in pixels.
    pub height: u32,
}
