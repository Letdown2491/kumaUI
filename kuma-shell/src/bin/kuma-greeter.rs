//! The kuma greeter. The UI half of the login screen: a full-screen
//! view with a username field, a masked password field, and the
//! session picker, driving the greetd client in `kuma_shell::greeter`.
//! A successful login quits the process: greetd starts the session
//! once its greeter is gone.
//!
//! `--headless USER` keeps the phase-one smoke path (password on
//! stdin, no UI) for the VT2 dress rehearsal and scripting.

use std::io::BufRead;

use gpui::{
    prelude::*, div, px, rgba, rgb, App, AppContext, Context, Focusable, FocusHandle,
    KeyDownEvent, Render, Window, WindowKind, WindowOptions,
};
use kuma_shell::greeter::{self, GreetdClient, LoginOutcome};
use kuma_shell::icons::KumaAssets;
use kuma_shell::theme;

use gpui_platform::application;

fn main() {
    use std::io::Write as _;
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

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--headless") {
        std::process::exit(headless(&args[1..]));
    }

    application().with_assets(KumaAssets).run(|cx: &mut App| {
        let state = cx.new(GreeterState::new);
        cx.open_window(window_options(), |window, cx| {
            cx.new(|cx| GreeterView::new(state, window, cx))
        })
        .expect("the greeter window");
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

    // the password comes from stdin, one line, never echoed back
    let mut password = String::new();
    std::io::stdin().lock().read_line(&mut password).unwrap();
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
    WindowOptions {
        titlebar: None,
        app_id: Some("kuma-greeter".into()),
        window_background: gpui::WindowBackgroundAppearance::Opaque,
        window_bounds: Some(gpui::WindowBounds::Fullscreen(gpui::Bounds::default())),
        kind: WindowKind::Normal,
        ..Default::default()
    }
}

// ---------- state ----------

/// Which field takes the typing: 0 is the username, 1 the password.
const USERNAME: usize = 0;
const PASSWORD: usize = 1;

struct GreeterState {
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
                    greeter::login(&mut client, &username, &password, &cmd)
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
    ) -> gpui::Stateful<gpui::Div> {
        let shown: gpui::SharedString = if value.is_empty() {
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
}

impl Focusable for GreeterView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GreeterView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let theme = theme::current();
        let username = state.username.clone();
        let password = state.password.clone();
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
                                self.field(&password, "password", focus == PASSWORD, busy)
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
