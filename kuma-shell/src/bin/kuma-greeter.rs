//! The kuma greeter. The UI half of the login screen: a full-screen
//! view with a username field, a masked password field, and the
//! session picker, driving the greetd client in `kuma_shell::greeter`.
//! A successful login quits the process: greetd starts the session
//! once its greeter is gone.
//!
//! `--headless USER` keeps the phase-one smoke path (password on
//! stdin, no UI) for the VT2 dress rehearsal and scripting.

use std::io::BufRead;
use std::sync::Arc;

use gpui::layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions};
use gpui::{

    prelude::*, div, px, rgba, rgb, App, AppContext, Context, Focusable, FocusHandle,
    KeyDownEvent, Render, Window, WindowKind, WindowOptions,
};
use kuma_shell::greeter::{self, GreetdClient, LoginOutcome};
use kuma_shell::icons::KumaAssets;
use kuma_shell::theme;
use zeroize::Zeroize;

use gpui_platform::application;

/// The version line `--version` prints: the crate version plus the
/// commit build.sh stamped in, so a pinned image answers to an exact
/// build. The format lives in the platform crate; the version stays a
/// call-site argument because it is this crate's.
pub(crate) fn version_line() -> String {
    kuma_platform::version::line("kuma-greeter", env!("CARGO_PKG_VERSION"))
}

/// `--version` is a first-argument flag, like every binary in the
/// family: a version spelling in a later position belongs to whatever
/// it follows (a headless rehearsal's session command), not to us.
pub(crate) fn wants_version(args: &[String]) -> bool {
    args.first()
        .is_some_and(|arg| kuma_platform::version::is_version_flag(arg))
}

fn main() {
    kuma_platform::logging::init();

    let args: Vec<String> = std::env::args().skip(1).collect();

    // --version prints and exits: no greetd socket, no compositor, no tty
    if wants_version(&args) {
        println!("{}", version_line());
        return;
    }

    if args.first().map(String::as_str) == Some("--headless") {
        std::process::exit(headless(&args[1..]));
    }

    application().with_assets(KumaAssets).run(|cx: &mut App| {
        log::info!("kuma-greeter starting (pid {})", std::process::id());
        let state = cx.new(GreeterState::new);
        // a failed window must not panic: greetd holds the VT and the
        // service decides whether to retry; exiting nonzero is honest
        match cx.open_window(window_options(), |window, cx| {
            cx.new(|cx| GreeterView::new(state, window, cx))
        }) {
            Ok(_) => {}
            Err(err) => {
                log::error!("opening the greeter window failed: {err:#}");
                std::process::exit(1);
            }
        }
    });
}

/// The phase-one smoke path: username as the argument, password on
/// stdin, session command optionally after it.
fn headless(args: &[String]) -> i32 {
    let mut args = args.iter();
    let Some(username) = args.next().cloned() else {
        eprintln!("usage: kuma-greeter --headless USERNAME [session command...]");
        return 2;
    };
    let cmd: Vec<String> = args.cloned().collect();

    // the session command: caller's choice, else the first installed
    // wayland session, else the niri default
    let default = {
        let sessions =
            greeter::wayland_sessions(std::path::Path::new("/usr/share/wayland-sessions"));
        sessions.first().map(|s| s.exec.clone())
    };
    let cmd: Vec<String> = if cmd.is_empty() {
        default
            .unwrap_or_else(|| "niri-session".to_string())
            .split_whitespace()
            .map(String::from)
            .collect()
    } else {
        cmd
    };
    let cmd: Vec<&str> = cmd.iter().map(String::as_str).collect();

    // the password comes from stdin, one line, never echoed back; a
    // closed stdin is a usage error, not a panic
    let mut password = String::new();
    if std::io::stdin().lock().read_line(&mut password).unwrap_or(0) == 0 {
        eprintln!("kuma-greeter: no password on stdin");
        return 2;
    }
    let password = password.trim_end_matches(['\n', '\r']);

    let mut client = match GreetdClient::connect() {
        Ok(client) => client,
        Err(err) => {
            eprintln!("kuma-greeter: {err:#}");
            return 1;
        }
    };
    match greeter::login(&mut client, &username, password, &cmd) {
        Ok(LoginOutcome::Started) => {
            println!("session started: {cmd:?}");
            0
        }
        Ok(LoginOutcome::AuthFailed(description)) => {
            eprintln!("kuma-greeter: login failed: {description}");
            1
        }
        Err(err) => {
            eprintln!("kuma-greeter: {err:#}");
            1
        }
    }
}

fn window_options() -> WindowOptions {
    // layer shell, the lock screen's shape: fullscreen on the output,
    // exclusive keyboard. The shell's windows all ride this path; the
    // plain toplevel path dies on niri with a wp_viewport protocol
    // error (gpui's viewport gets a value niri rejects)
    WindowOptions {
        titlebar: None,
        app_id: Some("kuma-greeter".into()),
        window_background: gpui::WindowBackgroundAppearance::Opaque,
        window_bounds: Some(gpui::WindowBounds::Windowed(gpui::Bounds {
            origin: gpui::point(px(0.), px(0.)),
            size: gpui::size(px(0.), px(0.)),
        })),
        kind: WindowKind::LayerShell(LayerShellOptions {
            namespace: "kuma-greeter".into(),
            layer: Layer::Overlay,
            exclusive_zone: Some(px(-1.)),
            anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
            keyboard_interactivity: KeyboardInteractivity::Exclusive,
            ..Default::default()
        }),
        ..Default::default()
    }
}

// ---------- state ----------

/// Which field takes the typing: 0 is the username, 1 the password.
const USERNAME: usize = 0;
const PASSWORD: usize = 1;

struct GreeterState {
    wallpaper: Option<std::sync::Arc<gpui::RenderImage>>,
    username: String,
    password: String,
    focus: usize,
    busy: bool,
    /// The last attempt's failure message (shown under the fields).
    message: Option<gpui::SharedString>,
    sessions: Vec<greeter::Session>,
    session_index: usize,
    session_open: bool,
    /// The clock, redrawn on a tick.
    clock: String,
}

impl GreeterState {
    fn new(cx: &mut Context<Self>) -> Self {
        // the wallpaper and theme come from the same settings file the
        // shell reads; under the production greeter user the file may
        // not exist and Settings::load falls back to defaults
        let settings = cx.new(|_| kuma_shell::settings::Settings::load());
        settings.update(cx, |settings, cx| settings.refresh_theme(cx));
        let wallpaper_path = settings.read(cx).background.current_path();

        // decoded off-thread; the screen shows the dark base until it lands
        cx.spawn(async move |this, cx| {
            let image = cx
                .background_spawn(async move {
                    kuma_shell::imaging::decode_file(&wallpaper_path).map(Arc::new)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.wallpaper = image;
                cx.notify();
            });
        })
        .detach();

        let sessions =
            greeter::wayland_sessions(std::path::Path::new("/usr/share/wayland-sessions"));
        let clock = chrono::Local::now().format("%H:%M").to_string();

        // minute tick: the clock stays honest without repainting per frame
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(10))
                    .await;
                let alive = this
                    .update(cx, |this, cx| {
                        this.clock = chrono::Local::now().format("%H:%M").to_string();
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        })
        .detach();

        Self {
            wallpaper: None,
            username: String::new(),
            password: String::new(),
            focus: USERNAME,
            busy: false,
            message: None,
            sessions,
            session_index: 0,
            session_open: false,
            clock,
        }
    }

    /// Key routing: Enter logs in, Tab moves between the fields,
    /// printable characters fill the focused one.
    fn handle_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let keystroke = &event.keystroke;
        match keystroke.key.as_str() {
            "enter" => {
                self.session_open = false;
                self.attempt(cx);
            }
            "tab" => {
                self.focus = if self.focus == USERNAME { PASSWORD } else { USERNAME };
                self.session_open = false;
                cx.notify();
            }
            "escape" => {
                self.session_open = false;
                cx.notify();
            }
            "backspace" => {
                self.focused_field_mut().pop();
                self.message = None;
                cx.notify();
            }
            // paste into the focused field: a 40-char vault password
            // is typed once, by the password manager
            "v" if keystroke.modifiers.control || keystroke.modifiers.platform => {
                if let Some(text) = kuma_shell::paste::pasted_text(cx) {
                    self.focused_field_mut().push_str(&text);
                    self.message = None;
                    cx.notify();
                }
            }
            // kill line: a fat-fingered paste starts over
            "u" if keystroke.modifiers.control || keystroke.modifiers.platform => {
                if !self.focused_field_mut().is_empty() {
                    self.focused_field_mut().clear();
                    self.message = None;
                    cx.notify();
                }
            }
            // printable text into the focused field: shift+a and
            // caps-lock+a both report key "a" with key_char "A", so
            // the character, not the key name, is what lands (the
            // lock screen's rule)
            _ if !keystroke.modifiers.control
                && !keystroke.modifiers.alt
                && !keystroke.modifiers.platform
                && !keystroke.modifiers.function =>
            {
                if let Some(character) = keystroke.key_char.as_deref() {
                    self.focused_field_mut().push_str(character);
                    self.message = None;
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    fn focused_field_mut(&mut self) -> &mut String {
        if self.focus == USERNAME {
            &mut self.username
        } else {
            &mut self.password
        }
    }

    fn attempt(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.password.is_empty() {
            return;
        }
        let username = self.username.trim().to_string();
        if username.is_empty() {
            self.message = Some("who is logging in?".into());
            cx.notify();
            return;
        }
        let exec = self
            .sessions
            .get(self.session_index)
            .map(|s| s.exec.clone())
            .unwrap_or_else(|| "niri-session".to_string());
        let cmd: Vec<String> = exec.split_whitespace().map(String::from).collect();

        let password = std::mem::take(&mut self.password);
        self.busy = true;
        self.message = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let cmd: Vec<&str> = cmd.iter().map(String::as_str).collect();
                    let mut client = GreetdClient::connect()?;
                    // the attempt's last consumer is here: scrub the
                    // heap copy before the String frees (the lock
                    // screen's discipline; a greeter runs pre-auth,
                    // where freed heap is the exposure)
                    let mut password = password;
                    let result = greeter::login(&mut client, &username, &password, &cmd);
                    password.zeroize();
                    result
                })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(LoginOutcome::Started) => {
                    log::info!("greeter: session started, handing back to greetd");
                    cx.quit();
                }
                Ok(LoginOutcome::AuthFailed(description)) => {
                    this.busy = false;
                    this.message = Some(short_auth_failure(&description).into());
                    cx.notify();
                }
                Err(err) => {
                    log::error!("greeter: login attempt failed: {err:#}");
                    this.busy = false;
                    this.message = Some("something went wrong talking to greetd".into());
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

/// PAM's failure text can be long and technical: the greeter shows
/// greetd's own line, the log gets the context around it.
fn short_auth_failure(description: &str) -> String {
    let description = description.trim();
    if description.is_empty() {
        "wrong password".to_string()
    } else {
        description.to_string()
    }
}

// ---------- view ----------

struct GreeterView {
    state: gpui::Entity<GreeterState>,
    focus_handle: FocusHandle,
}

impl GreeterView {
    fn new(state: gpui::Entity<GreeterState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        Self { state, focus_handle }
    }

    /// One input field: masked when it holds the password, outlined
    /// when it holds the focus, dim when empty.
    fn field(
        &self,
        value: &str,
        placeholder: &'static str,
        focused: bool,
        busy: bool,
    ) -> gpui::Stateful<gpui::Div> {        let shown: gpui::SharedString = if value.is_empty() {
            placeholder.into()
        } else {
            "•".repeat(value.chars().count()).into()
        };
        div()
            .id(placeholder)
            .flex()
            .items_center()
            .w(px(280.))
            .px_4()
            .py_2()
            .rounded_xl()
            .bg(rgba(theme::current().panel_bg))
            .border_1()
            .border_color(rgb(if focused {
                theme::current().accent
            } else {
                theme::current().divider
            }))
            .text_size(px(13.))
            .text_color(rgb(if value.is_empty() {
                theme::current().text_dim
            } else {
                theme::current().text
            }))
            .when(busy, |el| el.text_color(rgb(theme::current().text_dim)))
            .child(shown)
    }

    /// Same field, fed a count instead of the password itself: the
    /// render never clones secret text onto the heap (the lock
    /// screen's rule), so a keystroke's repaint adds no fresh copies
    /// of a half-typed password.
    fn masked_field(
        &self,
        len: usize,
        placeholder: &'static str,
        focused: bool,
        busy: bool,
    ) -> gpui::Stateful<gpui::Div> {
        let dots = "•".repeat(len);
        self.field(&dots, placeholder, focused, busy)
    }
}

impl Focusable for GreeterView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GreeterView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui::{img, ObjectFit};

        let state = self.state.read(cx);
        let _theme = theme::current();
        let wallpaper = state.wallpaper.clone();
        let theme = theme::current();
        let username = state.username.clone();
        let password_len = state.password.chars().count();
        let focus = state.focus;
        let busy = state.busy;
        let message = state.message.clone();
        let session_open = state.session_open;
        let session_index = state.session_index;
        let session_names: Vec<String> = state
            .sessions
            .iter()
            .map(|s| s.name.clone())
            .collect();
        let current_session = state
            .sessions
            .get(session_index)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "niri".to_string());

        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                this.state
                    .update(cx, |state, cx| state.handle_key(event, cx));
            }))
            .bg(rgb(theme.panel_bg))
            .when_some(wallpaper, |el, image| {
                el.child(
                    img(gpui::ImageSource::Render(image))
                        .object_fit(ObjectFit::Cover)
                        .size_full()
                        .absolute(),
                )
            })
            .child(
                div()
                    .absolute()
                    .size_full()
                    .bg(rgba(0x000000B4)),
            )
            .child(
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_4()
                    .child(
                        div()
                            .text_size(px(64.))
                            .text_color(rgb(theme.text))
                            .child(state.clock.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap_3()
                            .child(
                                self.field(&username, "username", focus == USERNAME, busy)
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(|this, _, _, cx| {
                                            this.state.update(cx, |state, cx| {
                                                state.focus = USERNAME;
                                                state.session_open = false;
                                                cx.notify();
                                            });
                                        }),
                                    ),
                            )
                            .child(
                                self.masked_field(password_len, "password", focus == PASSWORD, busy)
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(|this, _, _, cx| {
                                            this.state.update(cx, |state, cx| {
                                                state.focus = PASSWORD;
                                                state.session_open = false;
                                                cx.notify();
                                            });
                                        }),
                                    ),
                            ),
                    )
                    .when(!session_names.is_empty(), |el| {
                        el.child(kuma_shell::panel_kit::dropdown(
                            cx,
                            "greeter-session",
                            current_session,
                            session_names,
                            session_index,
                            session_open,
                            |this: &mut GreeterView, _, cx| {
                                this.state.update(cx, |state, cx| {
                                    state.session_open = !state.session_open;
                                    cx.notify();
                                });
                            },
                            |index: usize, this: &mut GreeterView, _, cx| {
                                this.state.update(cx, |state, cx| {
                                    state.session_index = index;
                                    state.session_open = false;
                                    cx.notify();
                                });
                            },
                            |this: &mut GreeterView, _, cx| {
                                this.state.update(cx, |state, cx| {
                                    state.session_open = false;
                                    cx.notify();
                                });
                            },
                        ))
                    })
                    .when_some(message, |el, message| {
                        el.child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(theme::URGENT))
                                .child(message),
                        )
                    })
                    .when(busy, |el| {
                        el.child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(theme.text_dim))
                                .child("signing in…"),
                        )
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_names_the_binary() {
        let line = version_line();
        assert!(line.starts_with("kuma-greeter "));
        assert!(line.contains("(g"));
    }

    #[test]
    fn version_is_a_first_argument_flag_only() {
        assert!(wants_version(&["--version".to_string()]));
        assert!(wants_version(&["-V".to_string()]));
        assert!(!wants_version(&[]));
        // a later spelling belongs to the command after --headless:
        // printing the version instead of logging in would be a very
        // quiet way to strand a login screen
        assert!(!wants_version(&[
            "--headless".to_string(),
            "user".to_string(),
            "niri-session".to_string(),
            "-V".to_string(),
        ]));
    }
}
