use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;

use gpui::{App, AppContext, WindowBounds, WindowOptions, TitlebarOptions, px, size, SharedString};
use gpui_platform::application;

mod browser;
mod epub;
mod icons;
mod video;
mod videopane;
mod input;
mod theme;
mod viewer;

use browser::Browser;
use viewer::Hosts;

/// The version line `--version` prints and the boot log carries: the
/// crate version plus the commit build.sh stamped in (unstamped builds
/// read unknown), so a running instance pins to an exact build. The
/// format lives in the platform crate; the version stays a call-site
/// argument because it is this crate's.
pub(crate) fn version_line() -> String {
    kuma_platform::version::line("kuma-files", env!("CARGO_PKG_VERSION"))
}

/// The sidebar's short tag: the version alone, the name is already
/// in the title bar.
pub(crate) fn version_tag() -> String {
    kuma_platform::version::tag(env!("CARGO_PKG_VERSION"))
}

fn main() {
    kuma_platform::logging::init();

    // --version prints and exits: no session, no socket handoff, no
    // window, works from any tty
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| kuma_platform::version::is_version_flag(&arg))
    {
        println!("{}", version_line());
        return;
    }

    log::info!("koguma {}", version_line());

    if std::env::var("WAYLAND_DISPLAY").is_err() && std::env::var("DISPLAY").is_err() {
        eprintln!("kuma-files: no Wayland/X11 session detected");
        std::process::exit(1);
    }

    // an explicit CLI arg: a dir lists it, a file opens it in the
    // viewer. Nothing passed means "reopen where I was last time"
    // (falls back to home). %u hands over either a plain path or a
    // file:// URI, so both convert to a real path here, before the
    // socket handoff: activation lines carry paths and the pump
    // never parses URIs.
    let mut dir = std::env::args().nth(1).map(|arg| arg_to_path(&arg));
    let file = dir.take_if(|path| path.is_file());

    // second launch while one is already up: hand the request over
    // and exit before any GPU work happens. The request line is the
    // dir or file to open, bare for "bring me forward".
    let request = dir
        .clone()
        .or(file.clone())
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let listener = kuma_platform::activation::become_single_instance("kuma-files", &request);

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
                // a file lands in the viewer, a dir opens in a new
                // tab of the manager, a bare poke brings it forward
                kuma_platform::activation::pump(listener, cx, |text, cx| {
                    let path = (!text.is_empty()).then(|| PathBuf::from(text));
                    match path {
                        Some(path) if path.is_file() => viewer::open_or_focus(path, cx),
                        Some(dir) => {
                            if let Some(handle) = viewer::ensure_browser(cx) {
                                let _ = handle.update(cx, |browser, window, cx| {
                                    browser.open_dir_in_new_tab(dir, cx);
                                    window.activate_window();
                                });
                            }
                        }
                        None => {
                            if let Some(handle) = viewer::ensure_browser(cx) {
                                let _ = handle.update(cx, |_, window, _| window.activate_window());
                            }
                        }
                    }
                });
            }
        });
}

/// The argv contract: `%u` hands over a plain path or a `file://`
/// URI (the x-scheme-handler/file claim invites the URI form), so
/// both convert to a real path here. A file URI with an empty or
/// localhost host percent-decodes into a path; anything else rides
/// as-is into the dir branch, where a bad path fails with an honest
/// status, exactly like a nonexistent directory argument.
fn arg_to_path(arg: &str) -> PathBuf {
    let Some(rest) = arg.strip_prefix("file://") else {
        return PathBuf::from(arg);
    };
    // a file URI is host/path: only the local forms name a path
    let Some((host, path)) = rest.split_once('/') else {
        return PathBuf::from(arg);
    };
    if !host.is_empty() && host != "localhost" {
        return PathBuf::from(arg);
    }
    // split_once consumed the path's own leading slash: put it back
    let mut decoded = percent_decode(path);
    decoded.insert(0, b'/');
    PathBuf::from(OsStr::from_bytes(&decoded))
}

/// Percent-decode a URI path: `%XX` byte runs become raw bytes, so
/// escaped UTF-8 and spaces survive. A lone `%` is not an escape and
/// rides; `+` stays literal, that escape belongs to form encoding.
fn percent_decode(path: &str) -> Vec<u8> {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push(high * 16 + low);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_names_the_binary() {
        let line = version_line();
        assert!(line.starts_with("kuma-files "));
        assert!(line.contains("(g"));
    }

    #[test]
    fn version_tag_is_short() {
        let tag = version_tag();
        assert!(tag.starts_with('v'));
        assert!(!tag.contains("kuma-files"));
        assert!(tag.contains("(g"));
    }

    #[test]
    fn the_changelog_names_this_version() {
        // bumping Cargo.toml without writing the changelog fails here;
        // the parser lives in the platform crate
        let md = include_str!("../../CHANGELOG.md");
        assert_eq!(
            kuma_platform::version::changelog_version(md, "kuma-files").as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn arg_to_path_passes_plain_paths_through() {
        assert_eq!(arg_to_path("/tmp/a.png"), PathBuf::from("/tmp/a.png"));
        assert_eq!(arg_to_path("relative/dir"), PathBuf::from("relative/dir"));
        assert_eq!(arg_to_path(""), PathBuf::from(""));
    }

    #[test]
    fn arg_to_path_decodes_local_file_uris() {
        // the gio form: empty host
        assert_eq!(
            arg_to_path("file:///home/m/My%20File.png"),
            PathBuf::from("/home/m/My File.png")
        );
        // the RFC form: localhost
        assert_eq!(
            arg_to_path("file://localhost/home/m/x.jpg"),
            PathBuf::from("/home/m/x.jpg")
        );
        // escaped multibyte UTF-8 survives the byte-level decode
        assert_eq!(
            arg_to_path("file:///home/m/caf%C3%A9.png"),
            PathBuf::from("/home/m/café.png")
        );
        // a lone % is not an escape: it rides
        assert_eq!(
            arg_to_path("file:///tmp/100%.png"),
            PathBuf::from("/tmp/100%.png")
        );
        // a truncated escape rides too
        assert_eq!(
            arg_to_path("file:///tmp/x%2.png"),
            PathBuf::from("/tmp/x%2.png")
        );
    }

    #[test]
    fn arg_to_path_leaves_foreign_hosts_and_schemes_alone() {
        // not a local file: rides as-is, the dir branch reports it
        assert_eq!(
            arg_to_path("file://nas/share/x.png"),
            PathBuf::from("file://nas/share/x.png")
        );
        assert_eq!(
            arg_to_path("https://example.com/x.png"),
            PathBuf::from("https://example.com/x.png")
        );
        // a host with no path at all
        assert_eq!(
            arg_to_path("file://localhost"),
            PathBuf::from("file://localhost")
        );
    }
}
