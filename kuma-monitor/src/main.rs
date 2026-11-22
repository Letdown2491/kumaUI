//! kuma-monitor, the spike: a window-aware process table under one
//! CPU graph, observation only (issue #71). It answers three
//! questions: is gpui's list fast enough at a 1s tick, does the niri
//! join hold, and does the owner want it in the dock a week later.


use gpui::App;
use gpui_platform::application;

mod icons;
mod niri;
mod sample;
mod stats;
mod theme;
mod view;

fn main() {
    if !kuma_platform::app::start("kuma-monitor", env!("CARGO_PKG_VERSION")) {
        return;
    }
    kuma_platform::app::require_session("kuma-monitor");

    // second launch while one is already up: a bare poke over the
    // activation socket and exit; the first window comes forward.
    let listener = kuma_platform::activation::become_single_instance("kuma-monitor", "");

    application()
        .with_assets(icons::Assets)
        .run(move |cx: &mut App| {
            // the display name is Kamuy, the Ainu spirit that dwells
            // in the bear: the family's watcher (the binary and app id
            // keep kuma-monitor)
            let Some(handle) = kuma_platform::app::main_window(
                cx,
                "kuma-monitor",
                "Kamuy",
                860.,
                640.,
                |window, cx| view::MonitorView::new(window, cx),
            ) else {
                return;
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
