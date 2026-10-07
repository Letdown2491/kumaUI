//! kuma-term, the terminal spike: alacritty_terminal behind an engine seam,
//! rendered by gpui. One window, one shell, no tabs, no config yet.

mod encoder;
mod palette;
mod term;
mod view;

use std::io::Write as _;

use gpui::{App, AppContext, TitlebarOptions, WindowBounds, WindowOptions, px, size, SharedString};
use gpui_platform::application;

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

    if std::env::var("WAYLAND_DISPLAY").is_err() && std::env::var("DISPLAY").is_err() {
        eprintln!("kuma-term: no Wayland/X11 session detected");
        std::process::exit(1);
    }

    application().run(|cx: &mut App| {
        let bounds = gpui::Bounds::centered(None, size(px(920.), px(620.)), cx);
        let _ = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                app_id: Some("kuma-term".into()),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("kuma-term")),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| view::TerminalView::new(window, cx)),
        );
        cx.activate(true);
    });
}
