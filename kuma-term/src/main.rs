//! kuma-term, the terminal spike: alacritty_terminal behind an engine seam,
//! rendered by gpui. One window, one shell, no tabs, no config yet.

mod encoder;
mod font;
mod glyphs;
mod palette;
mod term;
mod theme;
mod view;

/// Serializes the tests that mutate `KUMA_TERM_COMMAND` and then spawn
/// an engine: the env read happens inside the spawn, so both steps must
/// hold the lock or the tests race each other's fixture.
#[cfg(test)]
pub(crate) static PTY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

use std::io::Write as _;

use gpui::{App, AppContext, TitlebarOptions, WindowBounds, WindowOptions, px, size, SharedString};
use gpui_platform::application;

fn main() {
    // the PTY child inherits this process's environment, so anything set
    // here is how the shell recognizes kuma-term (prompt snippets key on
    // it). Sound: no other thread exists yet.
    unsafe { std::env::set_var("KUMA_TERM", "1") };

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

    // `-e` consumes the rest of argv as the command to run instead of
    // the shell: the kitty/alacritty convention kuma-launch relies on
    // (`kuma-term -e <held script>`). Stateless on purpose: the
    // KUMA_TERM_COMMAND env hook (the tests') would persist into child
    // processes, and a kuma-term opened inside a kuma-launch'd
    // kuma-term would re-run the verb script instead of giving a shell.
    let command = parse_args(&std::env::args().collect::<Vec<_>>()).unwrap_or_else(|err| {
        eprintln!("kuma-term: {err}");
        std::process::exit(1);
    });

    application().run(move |cx: &mut App| {
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
            |window, cx| cx.new(|cx| view::TerminalView::new(command, window, cx)),
        );
        cx.activate(true);
    });
}

/// The `-e` convention: everything after it is the command, and
/// nothing after it is parsed as options. No `-e` means the login
/// shell; `-e` with nothing after it is an error, not a silent shell.
fn parse_args(args: &[String]) -> Result<Option<Vec<String>>, String> {
    let Some(position) = args.iter().position(|arg| arg == "-e") else {
        return Ok(None);
    };
    let command = &args[position + 1..];
    if command.is_empty() {
        return Err("-e needs a command after it".to_string());
    }
    Ok(Some(command.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn parse_args_takes_everything_after_e() {
        // the kuma-launch shape: one script, maybe with arguments
        assert_eq!(
            parse_args(&argv(&["kuma-term", "-e", "/bin/verb"])).unwrap(),
            Some(vec!["/bin/verb".to_string()])
        );
        assert_eq!(
            parse_args(&argv(&["kuma-term", "-e", "nvim", "foo.txt"])).unwrap(),
            Some(vec!["nvim".to_string(), "foo.txt".to_string()])
        );
    }

    #[test]
    fn parse_args_without_e_means_the_shell() {
        assert_eq!(parse_args(&argv(&["kuma-term"])).unwrap(), None);
        // the flag works at any position: argv[0] is just another
        // string to scan
        assert_eq!(
            parse_args(&argv(&["kuma-term", "--something", "-e", "sh"])).unwrap(),
            Some(vec!["sh".to_string()])
        );
    }

    #[test]
    fn parse_args_errors_on_a_bare_e() {
        assert!(parse_args(&argv(&["kuma-term", "-e"])).is_err());
    }
}
