//! App bootstrap: the main-preamble cluster the four binaries would
//! otherwise each own (and have already drifted in). The apps declare
//! what they are; the platform owns how a kuma app starts: logging on,
//! the `--version` first-arg check, the graphical-session gate, and
//! the main window's open (app id, title bar, centered bounds).
//!
//! What stays at the callers: the version string (the platform crate's
//! own version is not the app's), anything between the gate and the
//! window (CLI parsing, activation handoff, env the pty inherits), and
//! the run closure's body.

use gpui::{App, Render, WindowHandle, AppContext as _};

/// The shared preamble: logging on, then the `--version` first-arg
/// check (prints the version line and reports false: the caller
/// returns, nothing else ever ran). No session gate here; the greeter
/// starts from a greetd seat, not a desktop session, so the graphical
/// apps call `require_session` after.
///
/// `name` is the binary's name ("kuma-files"), `version` the caller's
/// `env!("CARGO_PKG_VERSION")`: the line is the platform's format, the
/// version is the app's.
pub fn start(name: &str, version: &str) -> bool {
    crate::logging::init();
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| crate::version::is_version_flag(&arg))
    {
        println!("{}", crate::version::line(name, version));
        return false;
    }
    true
}

/// The graphical-session gate: neither Wayland nor X11 is answering,
/// so no window can open. Prints and exits the process; a headless
/// run of a graphical app is never wanted (the smoke tests run the
/// real session).
pub fn require_session(name: &str) {
    if std::env::var("WAYLAND_DISPLAY").is_err() && std::env::var("DISPLAY").is_err() {
        eprintln!("{name}: no Wayland/X11 session detected");
        std::process::exit(1);
    }
}

/// Open the app's one main window: centered at the given size, the
/// Wayland app id set (the dock matches it against the desktop-file
/// stem for icon and grouping), the family's client-side title bar
/// titled with the display name. A failed open logs and reads None; a
/// kuma app cannot put up a dialog over a window it could not open.
pub fn main_window<T: Render + 'static>(
    cx: &mut App,
    app_id: &str,
    title: &str,
    width: f32,
    height: f32,
    build: impl FnOnce(&mut gpui::Window, &mut gpui::Context<T>) -> T,
) -> Option<WindowHandle<T>> {
    let bounds = gpui::Bounds::centered(None, gpui::size(gpui::px(width), gpui::px(height)), cx);
    let handle = match cx.open_window(
        gpui::WindowOptions {
            window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
            app_id: Some(app_id.into()),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some(gpui::SharedString::from(title)),
                ..Default::default()
            }),
            ..Default::default()
        },
        |window, cx| cx.new(|cx| build(window, cx)),
    ) {
        Ok(handle) => handle,
        Err(err) => {
            log::error!("open_window failed: {err:#}");
            return None;
        }
    };
    Some(handle)
}
