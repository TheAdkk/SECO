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
    /// Logical width in pixels the editor opens at.
    pub width: u32,
    /// Logical height in pixels the editor opens at.
    pub height: u32,
    /// Smallest size the page still reads at, or `None` for an editor the
    /// host must not resize.
    ///
    /// A host asked to resize will clamp to this, so it is a promise about
    /// the page rather than a hint: below it the layout is expected to be
    /// wrong, not merely tight.
    pub minimum: Option<(u32, u32)>,
}
