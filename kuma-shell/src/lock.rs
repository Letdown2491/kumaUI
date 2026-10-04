//! The lock screen: opaque, wallpaper-backed surfaces on every output with
//! exclusive keyboard, one shared password field, and PAM authentication
//! (pam-client2, greetd's own lineage). Triggered by logind's session
//! `Lock` signal, so `loginctl lock-session` is the universal keybind and
//! any future idle daemon rides the same seam.

use std::ffi::{CStr, CString};
use std::sync::Arc;
use std::time::Duration;

use chrono::Local;
use futures_lite::StreamExt;
use gpui::{
    App, AppContext, Bounds, Context, DisplayId, Entity, FocusHandle, Focusable, KeyDownEvent,
    ObjectFit, Render, RenderImage, SharedString, Window, WindowBackgroundAppearance, WindowBounds,
    WindowHandle, WindowKind, WindowOptions, div, img,
    layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
    point,
    prelude::*,
    px, rgb, rgba, size,
};
use smol::channel::{Receiver, Sender, unbounded};

use crate::imaging;
use crate::settings::Settings;
use crate::theme::*;

/// The PAM service chain, first that starts wins: a distro-provided
/// `kuma-lock` (someday), swaylock's (if installed), vlock's (on kumaOS
/// today: auth ← system-auth, account permit; a locker's shape, no
/// session modules).
const PAM_SERVICES: [&str; 3] = ["kuma-lock", "swaylock", "vlock"];

/// The PAM conversation for an unlock attempt: the pre-typed password
/// answers the password prompt. Anything PAM wants to ask interactively
/// (a second factor, a re-prompt) is refused; v1 is password-only, and
/// fingerprint hardware that consults the user directly doesn't need us.
struct PasswordConversation {
    password: CString,
}

impl pam_client2::ConversationHandler for PasswordConversation {
    fn prompt_echo_on(&mut self, _prompt: &CStr) -> Result<CString, pam_client2::ErrorCode> {
        Err(pam_client2::ErrorCode::CONV_ERR)
    }

    fn prompt_echo_off(&mut self, _prompt: &CStr) -> Result<CString, pam_client2::ErrorCode> {
        Ok(self.password.clone())
    }

    fn text_info(&mut self, msg: &CStr) {
        log::info!("pam says: {}", msg.to_string_lossy());
    }

    fn error_msg(&mut self, msg: &CStr) {
        log::warn!("pam says: {}", msg.to_string_lossy());
    }
}

/// One unlock attempt, on a blocking thread: start the first service in
/// the chain, authenticate, validate the account.
fn authenticate(user: &str, password: String) -> anyhow::Result<()> {
    let mut last = None;
    for service in PAM_SERVICES {
        // Linux-PAM defers reading a service's stack to the first call and,
        // when /etc/pam.d/<service> is missing, answers from the `other`
        // policy (pam_deny here). That rejects every password exactly like
        // a wrong one would, and pam_start succeeds regardless, so the only
        // way the chain reaches the first installed service is to skip the
        // ones that aren't on disk (kuma-lock and swaylock on kumaOS).
        if !std::path::Path::new("/etc/pam.d").join(service).exists() {
            continue;
        }
        let conversation = PasswordConversation {
            password: CString::new(password.as_bytes())
                .map_err(|_| anyhow::anyhow!("password contains a NUL byte"))?,
        };
        match pam_client2::Context::new(service, Some(user), conversation) {
            Ok(mut context) => {
                // A real stack's answer is final: wrong password, expired
                // account, whatever; falling through would just repeat
                // the deny in the next service.
                context.authenticate(pam_client2::Flag::NONE)?;
                context.acct_mgmt(pam_client2::Flag::NONE)?;
                return Ok(());
            }
            Err(err) => last = Some(err),
        }
    }
    Err(anyhow::anyhow!(
        "no PAM service in the chain starts: {}",
        last.map(|err| err.to_string()).unwrap_or_default()
    ))
}

/// The whole lock state: locked or not, the shared password field, one
/// surface per display, the decoded wallpaper. Views observe this; key
/// events from whichever surface has the keyboard all feed into it.
pub struct LockState {
    locked: bool,
    password: String,
    /// Set while a PAM attempt is in flight: the field shows "checking".
    busy: bool,
    /// The last attempt's failure message (shown under the field).
    message: Option<SharedString>,
    wallpaper: Option<Arc<RenderImage>>,
    wallpaper_path: Option<std::path::PathBuf>,
    surfaces: Vec<WindowHandle<LockView>>,
    settings: Entity<Settings>,
}

impl LockState {
    pub fn new(settings: Entity<Settings>) -> Self {
        Self {
            locked: false,
            password: String::new(),
            busy: false,
            message: None,
            wallpaper: None,
            wallpaper_path: None,
            surfaces: Vec::new(),
            settings,
        }
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    pub fn lock(&mut self, cx: &mut Context<Self>) {
        if self.locked {
            return;
        }
        self.locked = true;
        self.password.clear();
        self.busy = false;
        self.message = None;
        cx.notify();

        // under the lock surfaces nothing else should be open
        crate::panel::close_panels(cx);

        // the wallpaper the user set, decoded off-thread; the surfaces
        // show a dark base until it lands
        let path = self.settings.read(cx).background.current_path();
        if self.wallpaper_path.as_deref() != Some(path.as_path()) {
            self.wallpaper = None;
            self.wallpaper_path = Some(path.clone());
            cx.spawn(async move |this, cx| {
                let image = cx
                    .background_spawn(async move { imaging::decode_file(&path).map(Arc::new) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.wallpaper = image;
                    cx.notify();
                });
            })
            .detach();
        }

        // the lock clock ticks while locked (minute-precision, checked
        // often enough to never be a minute stale)
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(10))
                    .await;
                let alive = this
                    .update(cx, |this, cx| {
                        if this.locked {
                            cx.notify();
                            true
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    pub fn unlock(&mut self, cx: &mut Context<Self>) {
        if !self.locked {
            return;
        }
        self.locked = false;
        self.password.clear();
        self.busy = false;
        self.message = None;
        cx.notify();
        let surfaces = std::mem::take(&mut self.surfaces);
        for handle in surfaces {
            crate::panel::defer_close(
                Box::new(move |cx| {
                    if let Err(err) = handle.update(cx, |_, window, _| window.remove_window()) {
                        log::error!("closing lock surface failed: {err:#}");
                    }
                }),
                cx,
            );
        }
    }

    /// Key routing from whichever lock surface holds the keyboard.
    fn handle_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let keystroke = &event.keystroke;
        match keystroke.key.as_str() {
            "enter" => self.attempt(cx),
            "backspace" => {
                self.password.pop();
                self.message = None;
                cx.notify();
            }
            "tab" => {}
            // Insert the typed character, not the key name: shift+a and
            // caps-lock+a both report key "a" with key_char "A", a composed
            // accent reports a dead-key name with key_char "á", and space
            // reports key "space" with key_char " ". Shift is the one
            // modifier that still yields text; with the others the key is
            // a shortcut, not input.
            _ if !keystroke.modifiers.control
                && !keystroke.modifiers.alt
                && !keystroke.modifiers.platform
                && !keystroke.modifiers.function =>
            {
                if let Some(character) = keystroke.key_char.as_deref() {
                    self.password.push_str(character);
                    self.message = None;
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    fn attempt(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.password.is_empty() {
            return;
        }
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_default();
        if user.is_empty() {
            self.message = Some("no user to authenticate".into());
            cx.notify();
            return;
        }
        let password = std::mem::take(&mut self.password);
        self.busy = true;
        self.message = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { authenticate(&user, password) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(()) => this.unlock(cx),
                    Err(err) => {
                        // generic message: PAM's own text can leak details
                        log::info!("unlock attempt failed: {err:#}");
                        this.message = Some("wrong password".into());
                        this.password.clear();
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }
}

/// One lock surface on one display: the user's wallpaper (darkened), a
/// big clock, the date, and the shared password field.
pub struct LockView {
    state: Entity<LockState>,
    focus_handle: FocusHandle,
}

impl LockView {
    fn new(state: Entity<LockState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        Self {
            state,
            focus_handle,
        }
    }
}

impl Focusable for LockView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for LockView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let password_len = state.password.chars().count();
        let busy = state.busy;
        let message = state.message.clone();
        let wallpaper = state.wallpaper.clone();

        let now = Local::now();
        let time = now.format("%H:%M").to_string();
        let date = now.format("%A, %B %e").to_string();

        let field: SharedString = if busy {
            "checking…".into()
        } else if password_len == 0 {
            "type your password".into()
        } else {
            "•".repeat(password_len).into()
        };

        div()
            .id("lock")
            .size_full()
            .relative()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                this.state
                    .update(cx, |state, cx| state.handle_key(event, cx));
            }))
            .bg(rgb(0x11111B))
            .when_some(wallpaper, |el, image| {
                el.child(
                    img(gpui::ImageSource::Render(image))
                        .object_fit(ObjectFit::Cover)
                        .size_full()
                        .absolute(),
                )
            })
            .child(div().absolute().size_full().bg(rgba(0x000000B4)))
            .child(
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_4()
                    .child(div().text_size(px(64.)).text_color(rgb(crate::theme::current().text)).child(time))
                    .child(
                        div()
                            .text_size(px(14.))
                            .text_color(rgb(crate::theme::current().text_dim))
                            .child(date),
                    )
                    .child(
                        div()
                            .id("lock-password")
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(280.))
                            .px_4()
                            .py_2()
                            .rounded_xl()
                            .bg(rgba(crate::theme::current().panel_bg))
                            .border_1()
                            .border_color(rgb(if message.is_some() { crate::theme::URGENT } else { crate::theme::current().divider }))
                            .text_size(px(13.))
                            .text_color(rgb(if busy || password_len == 0 {
                                crate::theme::current().text_dim
                            } else {
                                crate::theme::current().text
                            }))
                            .child(field),
                    )
                    .when_some(message, |el, message| {
                        el.child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(crate::theme::URGENT))
                                .child(message),
                        )
                    }),
            )
    }
}

/// One lock surface: fullscreen on its display, above everything (overlay
/// layer, created last), opaque, keyboard-exclusive.
fn lock_window_options(display_id: DisplayId) -> WindowOptions {
    WindowOptions {
        titlebar: None,
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(0.), px(0.)),
        })),
        app_id: Some("kuma-shell-lock".into()),
        window_background: WindowBackgroundAppearance::Opaque,
        kind: WindowKind::LayerShell(LayerShellOptions {
            namespace: "kuma-lock".into(),
            layer: Layer::Overlay,
            exclusive_zone: Some(px(-1.)),
            anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
            keyboard_interactivity: KeyboardInteractivity::Exclusive,
            ..Default::default()
        }),
        display_id: Some(display_id),
        ..Default::default()
    }
}

/// What logind told us. Copy: both listeners may fire it repeatedly.
#[derive(Clone, Copy)]
enum LockEvent {
    Lock,
    Unlock,
    /// The machine is about to go under (logind's PrepareForSleep,
    /// argument true). Honoured only while lock-before-suspend is on.
    Sleep,
}

/// Open one opaque lock surface per display. Runs OUTSIDE any
/// `LockState` update: opening a window paints its first frame, and the
/// lock view's render reads the state; reading it mid-update would panic.
fn open_lock_surfaces(state: &Entity<LockState>, cx: &mut App) {
    let mut surfaces = Vec::new();
    for display in cx.displays() {
        match cx.open_window(lock_window_options(display.id()), |window, cx| {
            cx.new(|cx| LockView::new(state.clone(), window, cx))
        }) {
            Ok(handle) => surfaces.push(handle),
            Err(err) => log::error!("failed to open lock surface: {err:#}"),
        }
    }
    state.update(cx, |state, _| state.surfaces = surfaces);
}

/// The one lock path every trigger shares: logind's Lock signal, idle
/// reaching its timeout, sleep coming. Idempotent: an already-locked
/// session does nothing, so an idle watcher firing under the lock
/// screen is noise, not a second lock.
pub fn engage(state: &Entity<LockState>, cx: &mut App) {
    // the state flips inside the update; the surfaces open
    // outside it (see open_lock_surfaces)
    let needs_lock = !state.read(cx).is_locked();
    if needs_lock {
        state.update(cx, |state, cx| state.lock(cx));
        open_lock_surfaces(state, cx);
    }
}

/// The surfaces watch's lock arm: a locked session whose surfaces died
/// with their outputs gets them back when displays exist again. An
/// unlocked session needs nothing; its surfaces arrive with the next
/// [`engage`].
pub fn ensure_surfaces(state: &Entity<LockState>, cx: &mut App) {
    let locked = state.read(cx).is_locked();
    if !locked {
        return;
    }
    let surfaces = state.read(cx).surfaces.clone();
    let live = surfaces
        .iter()
        .filter(|surface| surface.update(cx, |_, _, _| {}).is_ok())
        .count();
    if needs_lock_surfaces(locked, live, cx.displays().len()) {
        open_lock_surfaces(state, cx);
    }
}

/// Whether the lock surface pass should run. Pinned by test: outputs
/// returning while locked must bring the lock screen back, and nothing
/// else recreates it.
fn needs_lock_surfaces(locked: bool, live_surfaces: usize, displays: usize) -> bool {
    locked && live_surfaces == 0 && displays > 0
}

/// Listen for logind's session Lock/Unlock signals and drive the state.
/// `loginctl lock-session` (any keybind), the idle watcher, and
/// PrepareForSleep all land here through [`engage`].
pub fn connect(state: &Entity<LockState>, cx: &mut App) {
    let (tx, rx) = unbounded::<LockEvent>();
    // The hint rides back the other way: logind's LockedHint is what
    // session tools and future greeters read, and a lock screen that
    // leaves it `no` is a locked session that reports itself open.
    let (hint_tx, hint_rx) = unbounded::<bool>();

    cx.background_spawn(async move {
        if let Err(err) = run_lock_listener(tx, hint_rx).await {
            log::error!("logind lock listener terminated: {err:#}");
        }
    })
    .detach();

    let state = state.clone();
    let lock_hint = hint_tx.clone();
    cx.spawn(async move |cx| {
        while let Ok(event) = rx.recv().await {
            cx.update(|cx| match event {
                LockEvent::Sleep => {
                    let before_suspend = state.read(cx).settings.read(cx).idle.lock_before_suspend;
                    if before_suspend {
                        engage(&state, cx);
                        let _ = lock_hint.try_send(true);
                    }
                }
                LockEvent::Lock => {
                    engage(&state, cx);
                    let _ = lock_hint.try_send(true);
                }
                LockEvent::Unlock => {
                    state.update(cx, |state, cx| state.unlock(cx));
                    let _ = lock_hint.try_send(false);
                }
            });
        }
    })
    .detach();
}

async fn run_lock_listener(tx: Sender<LockEvent>, hint_rx: Receiver<bool>) -> anyhow::Result<()> {
    // logind is a system-bus service, not a session one
    let connection = zbus::connection::Builder::system()?.build().await?;
    let manager = zbus::Proxy::new(
        &connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await?;
    // this process's session object: by the inherited session id, else by
    // PID (on this setup everything runs under the user manager, not the
    // session scope, so PID lookup only works on other layouts)
    let reply = match std::env::var("XDG_SESSION_ID") {
        Ok(id) => manager.call_method("GetSession", &id).await?,
        Err(_) => {
            manager
                .call_method("GetSessionByPID", &(std::process::id() as u32))
                .await?
        }
    };
    let path: zbus::zvariant::OwnedObjectPath = reply.body().deserialize()?;
    log::info!("lock listener on session {path}");

    // One session proxy for everything session-shaped: the two signal
    // subscriptions and the hint calls.
    let session = zbus::Proxy::new(
        &connection,
        "org.freedesktop.login1",
        path.clone(),
        "org.freedesktop.login1.Session",
    )
    .await?;

    // The hint writer. Failures warn and move on: logind refusing the
    // hint is cosmetic next to the lock itself, and a hint that cannot
    // be set must not take the lock path down with it.
    let hint_session = session.clone();
    smol::spawn(async move {
        while let Ok(locked) = hint_rx.recv().await {
            if let Err(err) = hint_session.call_method("SetLockedHint", &(locked,)).await {
                log::warn!("SetLockedHint({locked}) failed: {err:#}");
            }
        }
    })
    .detach();

    for (signal, event) in [("Lock", LockEvent::Lock), ("Unlock", LockEvent::Unlock)] {
        let session = session.clone();
        let tx = tx.clone();
        smol::spawn(async move {
            match session.receive_signal(signal).await {
                Ok(mut stream) => {
                    while stream.next().await.is_some() {
                        let _ = tx.try_send(event);
                    }
                }
                Err(err) => log::error!("subscribing to session {signal} failed: {err:#}"),
            }
        })
        .detach();
    }

    // PrepareForSleep is the manager's signal, not the session's: the
    // machine going under, not this session being told to lock. The
    // argument carries the direction: true is going down, false is
    // coming back.
    let proxy = zbus::Proxy::new(
        &connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await?;
    smol::spawn(async move {
        match proxy.receive_signal("PrepareForSleep").await {
            Ok(mut stream) => {
                while let Some(message) = stream.next().await {
                    let going_down: bool = message.body().deserialize().unwrap_or(false);
                    if going_down {
                        let _ = tx.try_send(LockEvent::Sleep);
                    }
                }
            }
            Err(err) => log::error!("subscribing to PrepareForSleep failed: {err:#}"),
        }
    })
    .detach();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::needs_lock_surfaces;

    #[test]
    fn locked_sessions_get_surfaces_back_only_when_displays_exist() {
        // outputs returned while locked: the lock screen comes back
        assert!(needs_lock_surfaces(true, 0, 1));
        // unlocked: surfaces arrive with the next engage, not the watch
        assert!(!needs_lock_surfaces(false, 0, 1));
        // live surfaces: leave them alone
        assert!(!needs_lock_surfaces(true, 2, 1));
        // displayless: there is nothing to attach to; wait
        assert!(!needs_lock_surfaces(true, 0, 0));
    }
}
