//! The polkit authentication agent: registers on the system bus as the
//! session's `org.freedesktop.PolicyKit1.AuthenticationAgent` and answers
//! polkitd's `BeginAuthentication` with a centered card (a password field,
//! an identity picker when the requested identities resolve to more than
//! one user, and a note line). This replaces the alien GTK agent
//! (mate-polkit) that answered prompts before; the OS image drops it once
//! this works.
//!
//! The PAM conversation rides polkit 127's helper, in the reference
//! agent's two modes: the socket-activated helper
//! (`/run/polkit/agent-helper.socket`) when it exists, else the setuid
//! spawn (`polkit-agent-helper-1`, username as argv, cookie on stdin).
//! kumaOS ships the setuid bit and leaves the socket unit disabled, so in
//! practice we spawn. In both modes the HELPER reports success to polkitd
//! itself and writes `SUCCESS` only after the authority accepted, so a
//! `SUCCESS` line means done and the method reply goes out at once. The
//! method is deliberately kept open across failed attempts: a wrong
//! password only ends the helper exchange, so the card stays up for
//! a retry until the user succeeds or dismisses it.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use gpui::{
    App, AppContext, Bounds, Context, Entity, FocusHandle, Focusable, KeyDownEvent, MouseButton,
    Render, SharedString, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle,
    WindowKind, WindowOptions, div,
    layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
    point,
    prelude::*,
    px, rgb, rgba, size,
};
use smol::channel::{Sender, bounded, unbounded};
use zbus::zvariant::{OwnedValue, Value};

use crate::lock::LockState;

const AUTHORITY_DEST: &str = "org.freedesktop.PolicyKit1";
const AUTHORITY_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";
const AUTHORITY_IFACE: &str = "org.freedesktop.PolicyKit1.Authority";
const AGENT_PATH: &str = "/org/kuma/PolicyKit1/AuthenticationAgent";
/// polkit 127's socket-activated helper; the setuid spawn fallback is
/// deliberately not implemented (kumaOS ships the socket).
const HELPER_SOCKET: &str = "/run/polkit/agent-helper.socket";
/// A dead connection (system bus restart) retries rather than ending the
/// feature, night-light style.
const RETRY: Duration = Duration::from_secs(5);
/// How often the agent task checks whether its connection died.
const LIVENESS: Duration = Duration::from_secs(5);
const CARD_WIDTH: f32 = 400.;

/// Register and serve the agent for the shell's lifetime. Started from
/// main like the other singletons; holds the lock state so prompts refuse
/// to open under the lock screen.
pub fn run(lock: &Entity<LockState>, cx: &mut App) {
    let state = cx.new(|_| PolkitState {
        lock: lock.clone(),
        prompt: None,
        window: None,
    });

    let (tx, rx) = unbounded::<AgentEvent>();
    cx.background_spawn(async move {
        if let Err(err) = run_agent(tx).await {
            log::error!("polkit agent terminated: {err:#}");
        }
    })
    .detach();

    // an authentication prompt under the lock screen is pointless and
    // would fight the lock surfaces for the exclusive keyboard
    let lock_state = state.clone();
    cx.observe(lock, move |lock: Entity<LockState>, cx: &mut App| {
        if lock.read(cx).is_locked() {
            let _ = lock_state.update(cx, |state, cx| state.dismiss(cx));
        }
    })
    .detach();

    cx.spawn(async move |cx| {
        while let Ok(event) = rx.recv().await {
            let _ = cx.update(|cx| match event {
                AgentEvent::Cancel { cookie } => {
                    state.update(cx, |state, cx| state.cancelled(&cookie, cx));
                }
                AgentEvent::Begin(request) => {
                    // the window opens outside any PolkitState update:
                    // opening paints the first frame, and the view's render
                    // reads the state (see open_lock_surfaces)
                    let wants_window = state.update(cx, |state, cx| state.begin(request, cx));
                    if wants_window {
                        let handle = cx.open_window(prompt_window_options(), |window, cx| {
                            cx.new(|cx| PromptView::new(state.clone(), window, cx))
                        });
                        let _ = state.update(cx, |state, _| state.window = handle.ok());
                    }
                }
            });
        }
    })
    .detach();
}

/// The request polkitd sent, parsed from the D-Bus call.
struct BeginPrompt {
    action_id: String,
    message: String,
    cookie: String,
    identities: Vec<Identity>,
    /// the reply path back into the awaiting D-Bus handler
    reply: Sender<Outcome>,
}

/// What the UI told the awaiting `BeginAuthentication` handler.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Outcome {
    /// authenticated and accepted by the authority
    Done,
    /// dismissed, refused, or unreachable: polkitd sees an error
    Dismissed,
}

enum AgentEvent {
    Begin(BeginPrompt),
    Cancel { cookie: String },
}

/// The one prompt state: the open card's content plus the reply path.
/// All key events from the surface feed into here.
struct Prompt {
    cookie: String,
    message: String,
    candidates: Vec<Candidate>,
    chosen: usize,
    password: String,
    /// set while a helper exchange is in flight
    busy: bool,
    /// PAM's info/error text, or our own status
    note: Option<SharedString>,
    /// bumped on every resubmit so stale pump events drop
    generation: u64,
    /// set once the exchange ended, so a late note cannot overwrite the
    /// final status
    finished: Option<u64>,
    reply: Sender<Outcome>,
}

pub struct PolkitState {
    lock: Entity<LockState>,
    prompt: Option<Prompt>,
    window: Option<WindowHandle<PromptView>>,
}

impl PolkitState {
    /// Handle a `BeginAuthentication`. Returns true when the card should
    /// open (the caller opens it outside this update).
    fn begin(&mut self, request: BeginPrompt, cx: &mut Context<Self>) -> bool {
        if self.lock.read(cx).is_locked() {
            log::warn!("polkit: refusing {}: the session is locked", request.action_id);
            let _ = request.reply.try_send(Outcome::Dismissed);
            return false;
        }
        // one prompt at a time: a second concurrent request is refused
        // (polkitd reports the failure to its own caller). A prompt whose
        // surface died with its output is cleared here instead, so a new
        // request can open fresh.
        if let Some(existing) = self.prompt.take() {
            if self.window_live(cx) {
                log::warn!(
                    "polkit: refusing {}: a prompt is already open",
                    request.action_id
                );
                let _ = existing.reply.try_send(Outcome::Dismissed);
                self.prompt = Some(existing);
                return false;
            }
            log::warn!("polkit: dropping a prompt whose surface is gone");
            let _ = existing.reply.try_send(Outcome::Dismissed);
        }
        let candidates = resolve_candidates(&request.identities, &passwd_text(), &group_text());
        if candidates.is_empty() {
            log::error!(
                "polkit: no candidate user for {} (identities: {:?})",
                request.action_id,
                request.identities
            );
            let _ = request.reply.try_send(Outcome::Dismissed);
            return false;
        }
        log::info!(
            "polkit: prompt for {}: {}",
            request.action_id,
            request.message
        );
        self.prompt = Some(Prompt {
            cookie: request.cookie,
            message: request.message,
            candidates,
            chosen: 0,
            password: String::new(),
            busy: false,
            note: None,
            generation: 0,
            finished: None,
            reply: request.reply,
        });
        cx.notify();
        true
    }

    /// `CancelAuthentication` from polkitd: the caller went away.
    fn cancelled(&mut self, cookie: &str, cx: &mut Context<Self>) {
        let matches = self
            .prompt
            .as_ref()
            .map(|prompt| prompt.cookie == cookie)
            .unwrap_or(false);
        if matches {
            self.dismiss(cx);
        }
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        if let Some(prompt) = self.prompt.take() {
            log::info!("polkit: prompt dismissed");
            let _ = prompt.reply.try_send(Outcome::Dismissed);
            self.close_window(cx);
            cx.notify();
        }
    }

    /// Whether the prompt's surface can still be updated (a surface dies
    /// with its output; without this check a dead prompt would refuse
    /// every future request).
    fn window_live(&self, cx: &mut Context<Self>) -> bool {
        self.window
            .as_ref()
            .map(|window| window.update(cx, |_, _, _| {}).is_ok())
            .unwrap_or(false)
    }

    fn close_window(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.window.take() {
            crate::panel::defer_close(
                Box::new(move |cx| {
                    if let Err(err) = handle.update(cx, |_, window, _| window.remove_window()) {
                        log::error!("closing polkit prompt failed: {err:#}");
                    }
                }),
                cx,
            );
        }
    }

    /// Key routing from the prompt surface (the lock field's shape, plus
    /// escape to dismiss and arrows to cycle the identity).
    fn handle_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        match keystroke.key.as_str() {
            "escape" => self.dismiss(cx),
            "enter" => self.attempt(cx),
            "left" => self.cycle(-1, cx),
            "right" => self.cycle(1, cx),
            "tab" => {}
            // Insert the typed character, not the key name (ADR-0007, as
            // on the lock screen): shift+a reports key "a" with key_char
            // "A"; with modifiers held the key is a shortcut, not input.
            _ if !keystroke.modifiers.control
                && !keystroke.modifiers.alt
                && !keystroke.modifiers.platform
                && !keystroke.modifiers.function =>
            {
                if let Some(prompt) = self.prompt.as_mut() {
                    if let Some(character) = keystroke.key_char.as_deref() {
                        prompt.password.push_str(character);
                        prompt.note = None;
                        cx.notify();
                    }
                }
            }
            _ => {}
        }
    }

    fn cycle(&mut self, delta: i32, cx: &mut Context<Self>) {
        let Some(prompt) = self.prompt.as_mut() else {
            return;
        };
        if prompt.candidates.len() > 1 {
            let n = prompt.candidates.len() as i32;
            prompt.chosen = ((prompt.chosen as i32 + delta + n) % n) as usize;
            prompt.note = None;
            cx.notify();
        }
    }

    /// Enter pressed: run the helper exchange with the typed password.
    /// Each attempt is a fresh helper connection under the same polkitd
    /// cookie, and the BeginAuthentication method stays open across
    /// failures so the card can re-ask.
    fn attempt(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.prompt.as_mut() else {
            return;
        };
        if prompt.busy || prompt.password.is_empty() {
            return;
        }
        let candidate = prompt.candidates[prompt.chosen].clone();
        let password = std::mem::take(&mut prompt.password);
        prompt.busy = true;
        prompt.note = None;
        prompt.finished = None;
        prompt.generation += 1;
        let generation = prompt.generation;
        let cookie = prompt.cookie.clone();
        cx.notify();

        // PAM's info and error lines land while the exchange runs
        let (note_tx, note_rx) = unbounded::<PumpNote>();
        cx.spawn(async move |this, cx| {
            while let Ok(note) = note_rx.recv().await {
                let _ = this.update(cx, |state, cx| state.pump_note(generation, note, cx));
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let end = cx
                .background_spawn(async move {
                    run_pump(&cookie, &candidate.name, &password, &note_tx)
                })
                .await;
            let _ = this.update(cx, |state, cx| state.pump_finished(generation, end, cx));
        })
        .detach();
    }

    /// A mid-exchange line from the helper (info/error text).
    fn pump_note(&mut self, generation: u64, note: PumpNote, cx: &mut Context<Self>) {
        let Some(prompt) = self.prompt.as_mut() else {
            return;
        };
        if prompt.generation == generation && prompt.finished != Some(generation) {
            prompt.note = Some(note_text(note));
            cx.notify();
        }
    }

    /// The helper exchange ended: success means the helper reported to
    /// polkitd and was accepted, so the method reply goes out; failure
    /// just re-arms the card for another try.
    fn pump_finished(&mut self, generation: u64, end: PumpEnd, cx: &mut Context<Self>) {
        let Some(prompt) = self.prompt.as_mut() else {
            return;
        };
        if prompt.generation != generation {
            return;
        }
        match end {
            PumpEnd::Success => {
                // the helper already reported the authentication to polkitd
                // (its SUCCESS line only follows an accepted report): the
                // method reply completes polkitd's side
                log::info!("polkit: authentication complete");
                let reply = prompt.reply.clone();
                self.prompt = None;
                let _ = reply.try_send(Outcome::Done);
                self.close_window(cx);
            }
            PumpEnd::Failure => {
                prompt.busy = false;
                prompt.finished = Some(generation);
                prompt.note = Some("authentication failed".into());
                cx.notify();
            }
            PumpEnd::Broke(reason) => {
                log::error!("polkit helper exchange failed: {reason}");
                prompt.busy = false;
                prompt.finished = Some(generation);
                prompt.note = Some("the authentication helper failed".into());
                cx.notify();
            }
        }
    }
}

/// The card: a centered panel over a full-screen scrim. Keyboard is
/// exclusive, so the field needs no focus widget: the surface's key
/// listener routes everything (lock-screen style). Clicks outside are
/// deliberately ignored (GNOME behaves the same): dismissal is escape or
/// the cancel row, never a stray click.
pub struct PromptView {
    state: Entity<PolkitState>,
    focus_handle: FocusHandle,
}

impl PromptView {
    fn new(state: Entity<PolkitState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        Self {
            state,
            focus_handle,
        }
    }
}

impl Focusable for PromptView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PromptView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::current();
        let (message, busy, note, candidates, chosen, password_len) = self
            .state
            .read(cx)
            .prompt
            .as_ref()
            .map(|prompt| {
                (
                    prompt.message.clone(),
                    prompt.busy,
                    prompt.note.clone(),
                    prompt.candidates.clone(),
                    prompt.chosen,
                    prompt.password.chars().count(),
                )
            })
            .unwrap_or((String::new(), false, None, Vec::new(), 0, 0));

        let field: SharedString = if busy {
            "checking…".into()
        } else if password_len == 0 {
            "type your password".into()
        } else {
            "•".repeat(password_len).into()
        };

        let picking = candidates.len() > 1;
        let picker = |id: &'static str, delta: i32| {
            let state = self.state.clone();
            div()
                .id(id)
                .px_2()
                .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                    state.update(cx, |state, cx| state.cycle(delta, cx));
                })
                .text_color(rgb(theme.text_dim))
                .child(if delta < 0 { "<" } else { ">" })
        };

        div()
            .id("polkit")
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x000000A0))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                this.state
                    .update(cx, |state, cx| state.handle_key(event, cx));
            }))
            .child(
                div()
                    .w(px(CARD_WIDTH))
                    .px_5()
                    .py_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded_xl()
                    .bg(rgba(theme.panel_bg))
                    .border_1()
                    .border_color(rgb(theme.divider))
                    .child(
                        div()
                            .text_size(px(14.))
                            .text_color(rgb(theme.text))
                            .child(message.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap_2()
                            .text_size(px(12.))
                            .text_color(rgb(theme.text_dim))
                            .when(picking, |row| {
                                let name = candidates
                                    .get(chosen)
                                    .map(|candidate| candidate.name.clone())
                                    .unwrap_or_default();
                                row.child(picker("picker-prev", -1))
                                    .child(
                                        div()
                                            .text_color(rgb(theme.text))
                                            .child(SharedString::from(format!(
                                                "authenticate as {name}"
                                            ))),
                                    )
                                    .child(picker("picker-next", 1))
                            })
                            .when(!picking && !candidates.is_empty(), |row| {
                                row.child(SharedString::from(format!(
                                    "authenticating as {}",
                                    candidates[0].name
                                )))
                            }),
                    )
                    .child(
                        div()
                            .id("polkit-password")
                            .flex()
                            .items_center()
                            .w_full()
                            .px_4()
                            .py_2()
                            .rounded_xl()
                            .bg(rgba(theme.inset))
                            .border_1()
                            .border_color(rgb(if note.is_some() {
                                crate::theme::URGENT
                            } else {
                                theme.divider
                            }))
                            .text_size(px(13.))
                            .text_color(rgb(if busy || password_len == 0 {
                                theme.text_dim
                            } else {
                                theme.text
                            }))
                            .child(field),
                    )
                    .when_some(note, |el, note| {
                        el.child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(theme.text_dim))
                                .child(note),
                        )
                    })
                    .child({
                        let state = self.state.clone();
                        div()
                            .id("polkit-cancel")
                            .text_size(px(11.))
                            .text_color(rgb(theme.text_dim))
                            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                                state.update(cx, |state, cx| state.dismiss(cx));
                            })
                            .child("cancel (esc)")
                    }),
            )
    }
}

/// One prompt surface, centered on the focused output, above everything,
/// keyboard-exclusive. Transparent: the card paints its own panel over a
/// scrim div (the lock surfaces are opaque and wallpaper-backed; this is
/// a momentary dialog, not a takeover).
fn prompt_window_options() -> WindowOptions {
    WindowOptions {
        titlebar: None,
        // client decorations: layer surfaces have no server side
        window_decorations: Some(gpui::WindowDecorations::Client),
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(0.), px(0.)),
        })),
        app_id: Some("kuma-shell-polkit".into()),
        window_background: WindowBackgroundAppearance::Transparent,
        kind: WindowKind::LayerShell(LayerShellOptions {
            namespace: "kuma-polkit".into(),
            layer: Layer::Overlay,
            exclusive_zone: Some(px(-1.)),
            anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
            keyboard_interactivity: KeyboardInteractivity::Exclusive,
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The agent task: bring the connection up, register, hold it until it
/// dies, then retry. polkitd allows exactly one agent per session
/// subject, so while the previous agent (mate-polkit) is still running
/// registration fails with "already exists" and we keep retrying: that is
/// the transition state the OS image resolves by dropping the package.
async fn run_agent(events: Sender<AgentEvent>) -> anyhow::Result<()> {
    loop {
        if let Err(err) = bring_up(&events).await {
            log::error!("polkit agent: {err:#}");
        }
        smol::Timer::after(RETRY).await;
    }
}

async fn bring_up(events: &Sender<AgentEvent>) -> anyhow::Result<()> {
    let Some(session_id) = resolve_session_id().await else {
        anyhow::bail!("no logind session to register for");
    };
    let connection = zbus::connection::Builder::system()?
        .serve_at(AGENT_PATH, AgentService {
            events: events.clone(),
        })?
        .build()
        .await?;
    let authority =
        zbus::Proxy::new(&connection, AUTHORITY_DEST, AUTHORITY_PATH, AUTHORITY_IFACE).await?;
    let subject = (
        "unix-session",
        HashMap::from([("session-id".to_string(), Value::from(session_id.clone()))]),
    );
    if let Err(err) = authority
        .call_method(
            "RegisterAuthenticationAgent",
            &(subject, agent_locale(), AGENT_PATH),
        )
        .await
    {
        let text = format!("{err:#}");
        if text.contains("already exists") {
            log::warn!(
                "polkit: another agent is registered for this session (mate-polkit?); retrying"
            );
        }
        anyhow::bail!("registration failed: {text}");
    }
    log::info!("polkit: agent registered for session {session_id} at {AGENT_PATH}");

    // hold until the connection dies, then report to the retry loop
    while !connection.is_closed() {
        smol::Timer::after(LIVENESS).await;
    }
    anyhow::bail!("the system bus connection died");
}

/// The D-Bus object polkitd calls into. The handlers only ferry events to
/// the UI: BeginAuthentication is awaited there until the card finishes,
/// so polkitd's (timeout-free) method call resolves when the user does.
struct AgentService {
    events: Sender<AgentEvent>,
}

#[zbus::interface(name = "org.freedesktop.PolicyKit1.AuthenticationAgent")]
impl AgentService {
    async fn begin_authentication(
        &self,
        action_id: String,
        message: String,
        icon_name: String,
        details: HashMap<String, String>,
        cookie: String,
        identities: Vec<(String, HashMap<String, OwnedValue>)>,
    ) -> zbus::fdo::Result<()> {
        log::info!(
            "polkit: begin {action_id} ({icon_name}): {message} ({:?})",
            details.keys().collect::<Vec<_>>()
        );
        let identities: Vec<Identity> = identities
            .iter()
            .map(|(kind, details)| parse_identity(kind, details))
            .collect();
        let (reply_tx, reply_rx) = bounded::<Outcome>(1);
        let request = BeginPrompt {
            action_id,
            message,
            cookie,
            identities,
            reply: reply_tx,
        };
        if let Err(err) = self.events.send(AgentEvent::Begin(request)).await {
            return Err(zbus::fdo::Error::Failed(format!("agent ui is gone: {err}")));
        }
        match reply_rx.recv().await {
            Ok(Outcome::Done) => Ok(()),
            Ok(Outcome::Dismissed) | Err(_) => Err(zbus::fdo::Error::Failed(
                "authentication was dismissed".into(),
            )),
        }
    }

    async fn cancel_authentication(&self, cookie: String) -> zbus::fdo::Result<()> {
        let _ = self.events.try_send(AgentEvent::Cancel { cookie });
        Ok(())
    }
}

/// The session subject's id: the inherited logind id, else the tail of
/// the session object path GetSessionByPID returns (`.../session/_NN`).
async fn resolve_session_id() -> Option<String> {
    if let Ok(id) = std::env::var("XDG_SESSION_ID") {
        if !id.is_empty() {
            return Some(id);
        }
    }
    let connection = zbus::connection::Builder::system()
        .ok()?
        .build()
        .await
        .ok()?;
    let manager = zbus::Proxy::new(
        &connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await
    .ok()?;
    let reply = manager
        .call_method("GetSessionByPID", &(std::process::id() as u32))
        .await
        .ok()?;
    let path: zbus::zvariant::OwnedObjectPath = reply.body().deserialize().ok()?;
    path.as_str()
        .rsplit('/')
        .next()?
        .strip_prefix('_')
        .map(String::from)
}

/// The locale for polkitd's message localization: LANG without the
/// codeset ("en_US.UTF-8" registers as "en_US"), like the glib agents.
fn agent_locale() -> String {
    let locale = std::env::var("LANG")
        .unwrap_or_default()
        .split('.')
        .next()
        .unwrap_or("C")
        .to_string();
    if locale.is_empty() {
        "C".to_string()
    } else {
        locale
    }
}

/// A line PAM sent mid-exchange, for the card's note area.
struct PumpNote {
    text: String,
    is_error: bool,
}

fn note_text(note: PumpNote) -> SharedString {
    if note.is_error {
        SharedString::from(format!("pam: {}", note.text))
    } else {
        SharedString::from(note.text)
    }
}

/// How the helper exchange ended.
#[derive(Clone, Debug, PartialEq)]
enum PumpEnd {
    Success,
    Failure,
    Broke(String),
}

/// Where distros put the setuid helper (Fedora ships the first).
const HELPER_BINARIES: [&str; 2] = [
    "/usr/lib/polkit-1/polkit-agent-helper-1",
    "/usr/libexec/polkit-agent-helper-1",
];

/// One helper exchange: the reference agent's two modes. The
/// socket-activated helper (username line, then the cookie line over the
/// socket) when the unit is up; otherwise the setuid spawn (username as
/// argv[1], the cookie line on stdin), which is what kumaOS runs.
/// Blocking on purpose: it runs on a background executor thread like
/// every other subprocess read.
fn run_pump(cookie: &str, user: &str, password: &str, notes: &Sender<PumpNote>) -> PumpEnd {
    if std::path::Path::new(HELPER_SOCKET).exists() {
        match UnixStream::connect(HELPER_SOCKET) {
            Ok(stream) => {
                let mut writer = match stream.try_clone() {
                    Ok(clone) => clone,
                    Err(err) => return PumpEnd::Broke(err.to_string()),
                };
                let reader = BufReader::new(stream);
                return pump_conversation(reader, &mut writer, &[user, cookie], password, notes);
            }
            Err(err) => log::warn!(
                "polkit: the helper socket exists but connect failed: {err}; spawning the setuid helper"
            ),
        }
    }
    let Some(helper) = HELPER_BINARIES
        .iter()
        .map(std::path::Path::new)
        .find(|path| path.exists())
    else {
        return PumpEnd::Broke("no polkit agent helper is available".into());
    };
    let child = std::process::Command::new(helper)
        .arg(user)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(err) => return PumpEnd::Broke(format!("cannot start the helper: {err}")),
    };
    let mut writer = match child.stdin.take() {
        Some(stdin) => stdin,
        None => return PumpEnd::Broke("the helper has no stdin".into()),
    };
    let reader = BufReader::new(match child.stdout.take() {
        Some(stdout) => stdout,
        None => return PumpEnd::Broke("the helper has no stdout".into()),
    });
    // the setuid helper takes the cookie (and only the cookie) on stdin
    let end = pump_conversation(reader, &mut writer, &[cookie], password, notes);
    // EOF ends the helper; reap it so nothing lingers
    drop(writer);
    let _ = child.wait();
    end
}

/// The line protocol over an established helper connection: write the
/// identify lines, then answer prompt lines with the pre-typed password.
fn pump_conversation<R: std::io::Read, W: std::io::Write>(
    mut reader: BufReader<R>,
    writer: &mut W,
    identify: &[&str],
    password: &str,
    notes: &Sender<PumpNote>,
) -> PumpEnd {
    for line in identify {
        if let Err(err) = write_line(writer, line) {
            return PumpEnd::Broke(format!("cannot identify to the helper: {err}"));
        }
    }
    // the typed password answers the first secret prompt; a re-ask
    // (second factor, forced rotation) and a plain-text (echo) prompt
    // have no pre-typed answer: an empty line goes out and PAM fails
    // honestly: v1 is password-only, like the lock screen
    let mut answered = false;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return PumpEnd::Broke("the helper closed the connection".into()),
            Ok(_) => {}
            Err(err) => return PumpEnd::Broke(err.to_string()),
        }
        // the reference agent compresses the whole line, then matches
        // the token: the separator space is real, the value was escaped
        match parse_helper_line(&unescape_line(line.trim_end_matches('\n'))) {
            HelperLine::Prompt { echo, .. } => {
                let answer: &str = if echo {
                    ""
                } else if answered {
                    ""
                } else {
                    password
                };
                answered = true;
                if let Err(err) = write_line(writer, answer) {
                    return PumpEnd::Broke(err.to_string());
                }
            }
            HelperLine::Info(text) => {
                let _ = notes.try_send(PumpNote {
                    text,
                    is_error: false,
                });
            }
            HelperLine::Error(text) => {
                let _ = notes.try_send(PumpNote { text, is_error: true });
            }
            HelperLine::Success => return PumpEnd::Success,
            HelperLine::Failure => return PumpEnd::Failure,
            HelperLine::Unknown(line) => {
                return PumpEnd::Broke(format!("unknown line from the helper: {line:?}"))
            }
        }
    }
}

fn write_line<W: std::io::Write>(stream: &mut W, line: &str) -> std::io::Result<()> {
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")
}

/// One parsed helper line (the value after the token is unescaped).
#[derive(Clone, Debug, PartialEq)]
enum HelperLine {
    /// a question; echo says whether the answer shows on screen
    Prompt { text: String, echo: bool },
    Info(String),
    Error(String),
    Success,
    Failure,
    Unknown(String),
}

fn parse_helper_line(line: &str) -> HelperLine {
    if let Some(text) = line.strip_prefix("PAM_PROMPT_ECHO_OFF ") {
        return HelperLine::Prompt {
            text: text.to_string(),
            echo: false,
        };
    }
    if let Some(text) = line.strip_prefix("PAM_PROMPT_ECHO_ON ") {
        return HelperLine::Prompt {
            text: text.to_string(),
            echo: true,
        };
    }
    if let Some(text) = line.strip_prefix("PAM_ERROR_MSG ") {
        return HelperLine::Error(text.to_string());
    }
    if let Some(text) = line.strip_prefix("PAM_TEXT_INFO ") {
        return HelperLine::Info(text.to_string());
    }
    if line.starts_with("SUCCESS") {
        return HelperLine::Success;
    }
    if line.starts_with("FAILURE") {
        return HelperLine::Failure;
    }
    HelperLine::Unknown(line.to_string())
}

/// The `g_strcompress` half of glib's escaping: the helper's prompt text
/// arrives with backslash escapes (`\n`, `\t`, `\\`, `\OOO` octal);
/// unknown escapes pass the letter through.
fn unescape_line(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' || i + 1 >= bytes.len() {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        i += 1;
        match bytes[i] {
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'\\' => out.push(b'\\'),
            b'"' => out.push(b'"'),
            b'0'..=b'7' => {
                let mut value = 0u32;
                let mut digits = 0;
                while digits < 3 && i < bytes.len() && (b'0'..=b'7').contains(&bytes[i]) {
                    value = value * 8 + (bytes[i] - b'0') as u32;
                    i += 1;
                    digits += 1;
                }
                out.push(value as u8);
                continue;
            }
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The identities polkitd offers, reduced to what we can act on.
#[derive(Clone, Debug, PartialEq)]
enum Identity {
    UnixUser(u32),
    UnixGroup(u32),
    Other(String),
}

fn parse_identity(kind: &str, details: &HashMap<String, OwnedValue>) -> Identity {
    let number = |key: &str| -> Option<u32> {
        let value = Value::from(details.get(key)?.clone());
        match value {
            Value::U32(number) => Some(number),
            Value::I32(number) if number >= 0 => Some(number as u32),
            _ => None,
        }
    };
    match kind {
        "unix-user" => {
            number("uid").map_or_else(|| Identity::Other(kind.into()), Identity::UnixUser)
        }
        "unix-group" => {
            number("gid").map_or_else(|| Identity::Other(kind.into()), Identity::UnixGroup)
        }
        other => Identity::Other(other.into()),
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Candidate {
    name: String,
    uid: u32,
}

/// The users who could answer: direct unix-user identities land as
/// themselves, a unix-group identity expands to its members (polkitd's
/// admin identities on kumaOS are `unix-group:wheel`). Deduped by uid,
/// in the order the identities arrived.
fn resolve_candidates(identities: &[Identity], passwd: &str, group: &str) -> Vec<Candidate> {
    let users = passwd_entries(passwd);
    let mut candidates: Vec<Candidate> = Vec::new();
    for identity in identities {
        match identity {
            Identity::UnixUser(uid) => {
                if let Some((name, _)) = users.iter().find(|(_, id)| id == uid) {
                    push_candidate(&mut candidates, name.clone(), *uid);
                } else {
                    log::warn!("polkit: uid {uid} has no passwd entry");
                }
            }
            Identity::UnixGroup(gid) => {
                let Some((group_name, members)) = group_entry(group, *gid) else {
                    log::warn!("polkit: group with gid {gid} is not in /etc/group");
                    continue;
                };
                log::info!("polkit: candidates from group {group_name}");
                for member in members {
                    match users.iter().find(|(name, _)| *name == member) {
                        Some((_, uid)) => push_candidate(&mut candidates, member, *uid),
                        None => log::warn!("polkit: group member {member} has no passwd entry"),
                    }
                }
            }
            Identity::Other(kind) => log::warn!("polkit: ignoring identity kind {kind}"),
        }
    }
    candidates
}

fn push_candidate(candidates: &mut Vec<Candidate>, name: String, uid: u32) {
    if !candidates.iter().any(|candidate| candidate.uid == uid) {
        candidates.push(Candidate { name, uid });
    }
}

/// (name, uid) pairs from /etc/passwd text.
fn passwd_entries(text: &str) -> Vec<(String, u32)> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?.to_string();
            let _ = fields.next()?;
            let uid = fields.next()?.parse().ok()?;
            Some((name, uid))
        })
        .collect()
}

/// The (name, members) of the /etc/group entry with this gid.
fn group_entry(text: &str, gid: u32) -> Option<(String, Vec<String>)> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            if fields.len() >= 4 && fields[2].parse::<u32>().ok() == Some(gid) {
                Some((
                    fields[0].to_string(),
                    fields[3]
                        .split(',')
                        .filter(|member| !member.is_empty())
                        .map(String::from)
                        .collect(),
                ))
            } else {
                None
            }
        })
        .next()
}

fn passwd_text() -> String {
    std::fs::read_to_string("/etc/passwd").unwrap_or_default()
}

fn group_text() -> String {
    std::fs::read_to_string("/etc/group").unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn unescape_handles_glib_escapes() {
        assert_eq!(unescape_line("Password: "), "Password: ");
        // g_strescape: newline, tab, backslash, octal
        assert_eq!(unescape_line("a\\nb"), "a\nb");
        assert_eq!(unescape_line("a\\tb"), "a\tb");
        assert_eq!(unescape_line("a\\\\b"), "a\\b");
        assert_eq!(unescape_line("\\101\\102\\103"), "ABC");
        // a lone trailing backslash passes through
        assert_eq!(unescape_line("x\\"), "x\\");
    }

    #[test]
    fn helper_lines_parse() {
        assert_eq!(
            parse_helper_line("PAM_PROMPT_ECHO_OFF Password: "),
            HelperLine::Prompt {
                text: "Password: ".into(),
                echo: false
            }
        );
        assert_eq!(
            parse_helper_line("PAM_PROMPT_ECHO_ON login: "),
            HelperLine::Prompt {
                text: "login: ".into(),
                echo: true
            }
        );
        assert_eq!(
            parse_helper_line("PAM_ERROR_MSG wrong password"),
            HelperLine::Error("wrong password".into())
        );
        assert_eq!(
            parse_helper_line("PAM_TEXT_INFO hello"),
            HelperLine::Info("hello".into())
        );
        assert_eq!(parse_helper_line("SUCCESS"), HelperLine::Success);
        assert_eq!(parse_helper_line("FAILURE"), HelperLine::Failure);
        assert!(matches!(parse_helper_line("junk"), HelperLine::Unknown(_)));
    }

    #[test]
    fn passwd_and_group_text_parse() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\nmartin:x:1000:1000::/home/martin:/bin/zsh\n";
        let users = passwd_entries(passwd);
        assert_eq!(users[0], ("root".into(), 0));
        assert_eq!(users[1], ("martin".into(), 1000));
        let group = "wheel:x:10:martin\nkuma:x:2000:\n";
        let (name, members) = group_entry(group, 10).unwrap();
        assert_eq!(name, "wheel");
        assert_eq!(members, vec!["martin"]);
        assert!(group_entry(group, 99).is_none());
    }

    #[test]
    fn candidates_expand_groups_and_dedup() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\nmartin:x:1000:1000::/home/martin:\n";
        let group = "wheel:x:10:martin\n";
        let identities = vec![Identity::UnixGroup(10), Identity::UnixUser(1000)];
        let candidates = resolve_candidates(&identities, passwd, group);
        // the direct unix-user identity dedups against the group member
        assert_eq!(
            candidates,
            vec![Candidate {
                name: "martin".into(),
                uid: 1000
            }]
        );
        // unknown identity kinds are ignored, not fatal
        let candidates = resolve_candidates(&[Identity::Other("netgroup".into())], passwd, group);
        assert!(candidates.is_empty());
    }

    #[test]
    fn identity_details_parse() {
        let mut details = HashMap::new();
        details.insert(
            "uid".to_string(),
            OwnedValue::try_from(Value::U32(1000)).unwrap(),
        );
        assert_eq!(
            parse_identity("unix-user", &details),
            Identity::UnixUser(1000)
        );
        let mut details = HashMap::new();
        details.insert(
            "gid".to_string(),
            OwnedValue::try_from(Value::I32(10)).unwrap(),
        );
        assert_eq!(
            parse_identity("unix-group", &details),
            Identity::UnixGroup(10)
        );
        assert_eq!(
            parse_identity("unix-netgroup", &HashMap::new()),
            Identity::Other("unix-netgroup".into())
        );
    }

    #[test]
    fn pump_success_flow() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let helper = std::thread::spawn(move || {
            let mut writer = theirs.try_clone().unwrap();
            let mut reader = BufReader::new(theirs);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line, "martin\n");
            line.clear();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line, "cookie-1\n");
            writeln!(writer, "PAM_PROMPT_ECHO_OFF Password: ").unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line, "opensesame\n");
            writeln!(writer, "SUCCESS").unwrap();
        });
        let (notes_tx, notes_rx) = unbounded();
        let mut writer = ours.try_clone().unwrap();
        let end = pump_conversation(
            BufReader::new(ours),
            &mut writer,
            &["martin", "cookie-1"],
            "opensesame",
            &notes_tx,
        );
        assert_eq!(end, PumpEnd::Success);
        helper.join().unwrap();
        // nothing but the prompt reached the note channel
        drop(notes_tx);
        assert!(notes_rx.recv_blocking().is_err());
    }

    #[test]
    fn pump_failure_and_notes() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let helper = std::thread::spawn(move || {
            let mut writer = theirs.try_clone().unwrap();
            let mut reader = BufReader::new(theirs);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            reader.read_line(&mut line).unwrap();
            writeln!(writer, "PAM_ERROR_MSG \\101uth failure").unwrap();
            writeln!(writer, "FAILURE").unwrap();
        });
        let (notes_tx, notes_rx) = unbounded();
        let mut writer = ours.try_clone().unwrap();
        let end = pump_conversation(
            BufReader::new(ours),
            &mut writer,
            &["martin", "cookie-1"],
            "opensesame",
            &notes_tx,
        );
        assert_eq!(end, PumpEnd::Failure);
        helper.join().unwrap();
        let note = notes_rx.try_recv().unwrap();
        assert!(note.is_error);
        assert_eq!(note.text, "Auth failure");
    }

    #[test]
    fn pump_child_mode_takes_the_cookie_on_stdin() {
        // the setuid helper's shape: the username is argv, the cookie the
        // first stdin line; prompts and answers ride stdout/stdin after
        let script = "read cookie; echo 'PAM_PROMPT_ECHO_OFF Password: '; read pw; echo SUCCESS";
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut writer = child.stdin.take().unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        let (notes_tx, _notes_rx) = unbounded();
        let end = pump_conversation(reader, &mut writer, &["cookie-1"], "opensesame", &notes_tx);
        drop(writer);
        assert_eq!(end, PumpEnd::Success);
        child.wait().unwrap();
    }

    #[test]
    fn pump_reports_a_broken_helper() {
        // a peer that closes before the handshake: honest failure
        let (ours, theirs) = UnixStream::pair().unwrap();
        drop(theirs);
        let (notes_tx, _notes_rx) = unbounded();
        let mut writer = ours.try_clone().unwrap();
        let end = pump_conversation(
            BufReader::new(ours),
            &mut writer,
            &["martin", "cookie-1"],
            "opensesame",
            &notes_tx,
        );
        assert!(matches!(end, PumpEnd::Broke(_)));
    }
}
