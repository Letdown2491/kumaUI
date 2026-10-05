use std::io::Write as _;
use std::path::PathBuf;

use gpui::{App, AppContext, WindowBounds, WindowOptions, TitlebarOptions, px, size, SharedString};
use gpui_platform::application;

mod browser;
mod icons;
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

    // an explicit CLI dir wins over the saved session; nothing passed
    // means "reopen where I was last time" (falls back to home)
    let dir = std::env::args().nth(1).map(PathBuf::from);

    application()
        .with_assets(icons::Assets)
        .run(move |cx: &mut App| {
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
                    log::info!(
                        "koguma window open, {}",
                        dir.as_ref().map_or_else(
                            || "restoring saved session".to_string(),
                            |d| format!("listing {}", d.display()),
                        )
                    );
                    cx.new(|cx| Browser::new(dir.clone(), window, cx))
                },
            ) {
                Ok(_) => {}
                Err(err) => log::error!("open_window failed: {err:#}"),
            }
            cx.activate(true);
        });
}
