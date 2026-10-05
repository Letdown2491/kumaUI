use std::path::PathBuf;
use std::io::Write as _;

use gpui::{App, AppContext, WindowBounds, WindowOptions, TitlebarOptions, px, size, SharedString};
use gpui_platform::application;

mod browser;
mod theme;

use browser::Browser;

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
        eprintln!("kuma-files: no Wayland/X11 session detected");
        std::process::exit(1);
    }

    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir())
        .unwrap_or_else(|| PathBuf::from("."));

    application().run(move |cx: &mut App| {
        let bounds = gpui::Bounds::centered(None, size(px(960.), px(640.)), cx);
        match cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("Koguma")),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                log::info!("koguma window open, listing {}", dir.display());
                cx.new(|cx| Browser::new(dir, window, cx))
            },
        ) {
            Ok(_) => {}
            Err(err) => log::error!("open_window failed: {err:#}"),
        }
        cx.activate(true);
    });
}
