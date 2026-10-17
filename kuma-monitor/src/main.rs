//! kuma-monitor, the spike: a window-aware process table under one
//! CPU graph, observation only (issue #71). It answers three
//! questions: is gpui's list fast enough at a 1s tick, does the niri
//! join hold, and does the owner want it in the dock a week later.

use std::io::Write as _;

use gpui::{App, AppContext, TitlebarOptions, WindowBounds, WindowOptions, px, size, SharedString};
use gpui_platform::application;

mod icons;
mod niri;
mod sample;
mod stats;
mod theme;
mod view;

/// The version line `--version` prints: the crate version plus the
/// commit build.sh stamped in, so a running instance pins to an exact
/// build. The format lives in the platform crate; the version stays a
/// call-site argument because it is this crate's.
pub(crate) fn version_line() -> String {
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
        .is_some_and(|arg| kuma_platform::version::is_version_flag(&arg))
    {
        println!("{}", version_line());
        return;
    }

    if std::env::var("WAYLAND_DISPLAY").is_err() && std::env::var("DISPLAY").is_err() {
        eprintln!("kuma-monitor: no Wayland/X11 session detected");
        std::process::exit(1);
    }

    // second launch while one is already up: a bare poke over the
    // activation socket and exit; the first window comes forward.
    let listener = kuma_platform::activation::become_single_instance("kuma-monitor", "");

    application()
        .with_assets(icons::Assets)
        .run(move |cx: &mut App| {
        let bounds = gpui::Bounds::centered(None, size(px(860.), px(640.)), cx);
        let handle = match cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                app_id: Some("kuma-monitor".into()),
                titlebar: Some(TitlebarOptions {
                    // the display name is Kamuy, the Ainu spirit that
                    // dwells in the bear: the family's watcher (the
                    // binary and app id keep kuma-monitor)
                    title: Some(SharedString::from("Kamuy")),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| view::MonitorView::new(window, cx)),
        ) {
            Ok(handle) => handle,
            Err(err) => {
                log::error!("open_window failed: {err:#}");
                return;
            }
        };
        cx.activate(true);

        if let Some(listener) = listener {
            // every later launch pokes: the running window comes
            // forward, no second Kamuy
            kuma_platform::activation::pump(listener, cx, move |_, cx| {
                let _ = handle.update(cx, |_, window, _| window.activate_window());
                cx.activate(true);
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_names_the_binary() {
        let line = version_line();
        assert!(line.starts_with("kuma-monitor "));
        assert!(line.contains("(g"));
    }

    #[test]
    fn the_changelog_names_this_version() {
        // bumping Cargo.toml without writing the changelog fails here;
        // the parser lives in the platform crate
        let md = include_str!("../../CHANGELOG.md");
        assert_eq!(
            kuma_platform::version::changelog_version(md, "kuma-monitor").as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }
}
