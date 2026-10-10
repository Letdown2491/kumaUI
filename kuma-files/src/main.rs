use std::io::Write as _;
use std::path::PathBuf;

use gpui::{App, AppContext, WindowBounds, WindowOptions, TitlebarOptions, px, size, SharedString};
use gpui_platform::application;

mod browser;
mod epub;
mod icons;
mod video;
mod input;
mod theme;
mod viewer;

use browser::Browser;
use viewer::Hosts;

/// The activation socket in the session runtime dir: the first
/// instance listens here, later ones hand over their dir and exit.
fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(runtime) => PathBuf::from(runtime).join("kuma-files.sock"),
        None => PathBuf::from(format!("/tmp/kuma-files-{}.sock", unsafe { libc::getuid() })),
    }
}

/// A path that arrived over the activation socket: a file opens in
/// the viewer, a dir lists in the manager, none (a bare poke) just
/// brings the manager forward.
struct Activation {
    dir: Option<PathBuf>,
}

/// Either we own the single-instance socket or another instance
/// already does. In the handed-over case this process exits after
/// sending the request, so the return value is always Some here.
fn become_single_instance(dir: Option<PathBuf>) -> Option<std::os::unix::net::UnixListener> {
    let path = socket_path();

    // someone else's session: send the request over and leave
    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&path) {
        let dir = dir.unwrap_or_default();
        let _ = writeln!(stream, "{}", dir.display());
        std::process::exit(0);
    }

    // stale socket from a crashed instance (connect refused): remove
    // and rebind
    let _ = std::fs::remove_file(&path);
    match std::os::unix::net::UnixListener::bind(&path) {
        Ok(listener) => {
            log::info!("single instance: listening on {}", path.display());
            Some(listener)
        }
        Err(err) => {
            // cannot listen and cannot hand over: run anyway, the
            // worst case is two instances like before
            log::error!("single instance bind {}: {err}", path.display());
            None
        }
    }
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

    if std::env::var("WAYLAND_DISPLAY").is_err() && std::env::var("DISPLAY").is_err() {
        eprintln!("kuma-files: no Wayland/X11 session detected");
        std::process::exit(1);
    }

    // an explicit CLI arg: a dir lists it, a file opens it in the
    // viewer. Nothing passed means "reopen where I was last time"
    // (falls back to home)
    let mut dir = std::env::args().nth(1).map(PathBuf::from);
    let file = dir.take_if(|path| path.is_file());

    // second launch while one is already up: hand the request over
    // and exit before any GPU work happens
    let listener = become_single_instance(dir.clone().or(file.clone()));

    application()
        .with_assets(icons::Assets)
        .run(move |cx: &mut App| {
            cx.set_global(Hosts::default());
            match file.clone() {
                // the standalone open surface: no manager behind it
                Some(path) => {
                    log::info!("koguma viewer open, {}", path.display());
                    viewer::open_or_focus(path, cx);
                }
                None => {
                    let bounds = gpui::Bounds::centered(None, size(px(960.), px(640.)), cx);
                    let handle = match cx.open_window(
                        WindowOptions {
                            window_bounds: Some(WindowBounds::Windowed(bounds)),
                            // the Wayland app_id: the dock matches it against
                            // the desktop-file stem for icon and grouping
                            app_id: Some("kuma-files".into()),
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
                        Ok(handle) => handle,
                        Err(err) => {
                            log::error!("open_window failed: {err:#}");
                            return;
                        }
                    };
                    cx.global_mut::<Hosts>().browser = Some(handle);
                }
            }
            cx.activate(true);

            if let Some(listener) = listener {
                pump_activations(listener, cx);
            }
        });
}

/// Accept loop on a plain thread (blocking is fine there), forwarded
/// through an async channel so the UI only wakes per activation. A
/// file activation opens the viewer, a dir activation opens the
/// manager (growing it lazily in a viewer-only process), a bare poke
/// brings the manager forward.
fn pump_activations(listener: std::os::unix::net::UnixListener, cx: &mut App) {
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<Activation>();
    std::thread::Builder::new()
        .name("kuma-files-activate".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(stream) => stream,
                    Err(_) => return,
                };
                use std::io::Read as _;
                // one small message per connection: a client that
                // connects and never writes would otherwise stall
                // every later activation, and an unbounded read would
                // let it balloon the pump's memory
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
                let mut line = String::new();
                if (&mut stream).take(4096).read_to_string(&mut line).is_err() {
                    continue;
                }
                let text = line.trim();
                let dir = (!text.is_empty()).then(|| PathBuf::from(text));
                if tx.unbounded_send(Activation { dir }).is_err() {
                    return;
                }
            }
        })
        .expect("spawn activation pump");

    cx.spawn(async move |cx| {
        use futures::StreamExt;
        while let Some(activation) = rx.next().await {
            cx.update(|cx| {
                match activation.dir {
                    // a file lands in the viewer, replacing whatever
                    // it showed
                    Some(path) if path.is_file() => viewer::open_or_focus(path, cx),
                    // a dir request opens in a new tab of the
                    // manager (created here if this process is
                    // viewer-only)
                    Some(dir) => {
                        if let Some(handle) = viewer::ensure_browser(cx) {
                            let _ = handle.update(cx, |browser, window, cx| {
                                browser.open_dir_in_new_tab(dir, cx);
                                window.activate_window();
                            });
                        }
                    }
                    // a bare poke brings the manager forward
                    None => {
                        if let Some(handle) = viewer::ensure_browser(cx) {
                            let _ = handle.update(cx, |_, window, _| window.activate_window());
                        }
                    }
                }
            });
        }
    })
    .detach();
}
