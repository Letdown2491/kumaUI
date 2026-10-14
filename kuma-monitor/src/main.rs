//! kuma-monitor, the spike: a window-aware process table under one
//! CPU graph, observation only (issue #71). It answers three
//! questions: is gpui's list fast enough at a 1s tick, does the niri
//! join hold, and does the owner want it in the dock a week later.

use std::io::Write as _;

use gpui::{App, AppContext, TitlebarOptions, WindowBounds, WindowOptions, px, size, SharedString};
use gpui_platform::application;

mod niri;
mod sample;
mod theme;
mod view;

/// The version line `--version` prints: the crate version plus the
/// commit build.sh stamped in, so a running instance pins to an exact
/// build. The format lives in the platform crate; the version stays a
/// call-site argument because it is this crate's.
fn version_line() -> String {
    kuma_platform::version::line("kuma-monitor", env!("CARGO_PKG_VERSION"))
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default())
        .format(|buf, record| {
            writeln!(
                buf,
                "[{} {:<5} {}] {}",
                buf.timestamp_millis(),
                record.level(),
                record.target(),
                record.args()
            )
        })
        .init();

    // --version prints and exits: no session, no window, any tty
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--version" || arg == "-V")
    {
        println!("{}", version_line());
        return;
    }

    if std::env::var("WAYLAND_DISPLAY").is_err() && std::env::var("DISPLAY").is_err() {
        eprintln!("kuma-monitor: no Wayland/X11 session detected");
        std::process::exit(1);
    }

    application().run(move |cx: &mut App| {
        let bounds = gpui::Bounds::centered(None, size(px(860.), px(560.)), cx);
        let _ = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                app_id: Some("kuma-monitor".into()),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("kuma-monitor")),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| view::MonitorView::new(window, cx)),
        );
        cx.activate(true);
    });
}
