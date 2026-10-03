//! The notification center: a dbus daemon (org.freedesktop.Notifications)
//! hosted inside the shell, the state it feeds, and the seams panels and the
//! bar read from. No separate mako process: the shell *is* the daemon.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use gpui::{App, AppContext, Context, Entity};
use smol::channel::{Receiver, Sender, unbounded};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Str};

use crate::imaging::IconImage;
use crate::settings::Settings;

const OBJECT_PATH: &str = "/org/freedesktop/Notifications";
/// The history cap: the list keeps the newest N.
const MAX_HISTORY: usize = 50;
/// Toast duration when the client leaves it to us (-1).
const DEFAULT_EXPIRE: Duration = Duration::from_secs(5);
/// The cap: clients may ask for anything (0 = never, 600000 = ten minutes),
/// but a toast banner hangs around for at most this long.
const MAX_EXPIRE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Urgency {
    #[default]
    Low,
    Normal,
    Critical,
}

#[derive(Clone, Debug)]
pub struct Notification {
    pub id: u32,
    pub app_name: String,
    /// The resolved icon (app_icon or the image-path hint): raster or svg.
    pub icon: Option<crate::imaging::IconImage>,
    pub summary: String,
    pub body: String,
    /// (key, label) pairs, straight from the client.
    pub actions: Vec<(String, String)>,
    pub urgency: Urgency,
    pub received_at: DateTime<Local>,
    /// The client's expire hint, milliseconds: -1 = our default, 0 = never.
    pub expire_timeout: i32,
    /// Closed signals fire once per notification; the toast timing a second
    /// close would confuse clients that already saw one.
    pub closed: bool,
}

/// daemon → state (via the app's event loop, like the niri stream)
enum DaemonEvent {
    Notify(Notification),
    CloseRequested(u32),
}

/// state → daemon task (dbus signal emission)
#[derive(Clone, Debug)]
enum SignalOut {
    /// NotificationClosed(id, reason): 1 expired, 2 dismissed, 3 close method
    Closed(u32, u32),
    /// ActionInvoked(id, key), then Closed(id, 2)
    Action(u32, String),
}

/// The daemon's dbus object. Methods park events on the channel and return
/// immediately; the state on the main thread does everything visible.
struct KumaDaemon {
    events: Sender<DaemonEvent>,
    next_id: AtomicU32,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl KumaDaemon {
    async fn notify(
        &mut self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        // 0 means "no id"; a replaces_id reuses the client's id
        let id = if replaces_id != 0 {
            replaces_id
        } else {
            self.next_id.fetch_add(1, Ordering::Relaxed) + 1
        };
        // the icon: the app_icon argument, else the image-path hint; paths
        // decode directly, themed names resolve through the cache (off the
        // dispatch thread; the first call walks the whole theme)
        let icon_spec = icon_spec_from(&app_icon, &hints);
        let icon = if icon_spec.is_empty() {
            None
        } else {
            smol::unblock(move || crate::imaging::resolve_icon(&icon_spec)).await
        };
        let notification = Notification {
            id,
            app_name,
            icon,
            summary,
            body,
            actions: parse_actions(&actions),
            urgency: urgency_from_hints(&hints),
            received_at: Local::now(),
            expire_timeout,
            closed: false,
        };
        let _ = self.events.try_send(DaemonEvent::Notify(notification));
        id
    }

    async fn close_notification(&mut self, id: u32) {
        let _ = self.events.try_send(DaemonEvent::CloseRequested(id));
    }

    async fn get_capabilities(&self) -> Vec<&'static str> {
        vec!["actions", "body", "icon-static"]
    }

    async fn get_server_information(&self) -> (String, String, String, String) {
        (
            "kuma-shell".into(),
            "kuma".into(),
            "0.1".into(),
            "1.2".into(),
        )
    }

    #[zbus(signal)]
    async fn notification_closed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn action_invoked(
        emitter: &SignalEmitter<'_>,
        id: u32,
        action_key: String,
    ) -> zbus::Result<()>;
}

/// The icon spec for a notification: the app_icon argument wins, else the
/// image-path hint (both spellings clients use). Empty means no icon.
fn icon_spec_from(app_icon: &str, hints: &HashMap<String, OwnedValue>) -> String {
    if !app_icon.is_empty() {
        return app_icon.to_string();
    }
    ["image-path", "image_path"]
        .iter()
        .find_map(|key| hints.get(*key))
        .and_then(|value| <&Str>::try_from(value).ok())
        .map(|spec| spec.as_str().to_string())
        .unwrap_or_default()
}

/// The client's flat (key, label, key, label…) list → pairs.
fn parse_actions(actions: &[String]) -> Vec<(String, String)> {
    actions
        .chunks(2)
        .filter(|chunk| chunk.len() == 2)
        .map(|chunk| (chunk[0].clone(), chunk[1].clone()))
        .collect()
}

fn urgency_from_hints(hints: &HashMap<String, OwnedValue>) -> Urgency {
    hints
        .get("urgency")
        .and_then(|value| <&Str>::try_from(value).ok())
        .map(|urgency| match urgency.as_str() {
            "critical" => Urgency::Critical,
            "normal" => Urgency::Normal,
            _ => Urgency::Low,
        })
        .unwrap_or(Urgency::Normal)
}

/// The expire hint → how long the toast hangs: -1 = our default, 0 = the
/// cap; anything longer than the cap clamps to it (a toast banner is a
/// tap on the shoulder, not a wallpaper).
pub fn toast_duration(expire_timeout: i32) -> Duration {
    match expire_timeout {
        -1 => DEFAULT_EXPIRE,
        0 => MAX_EXPIRE,
        ms if ms > 0 => Duration::from_millis(ms as u64).min(MAX_EXPIRE),
        _ => DEFAULT_EXPIRE,
    }
}

/// The whole state of the notification center, on the main thread.
pub struct NotificationState {
    /// Newest first, capped at `MAX_HISTORY`.
    pub notifications: Vec<Notification>,
    /// Notifications since the panel was last opened: drives the bell.
    pub unread: usize,
    /// Mirrored from Settings (the source of truth; persisted there).
    pub dnd: bool,
    /// Whether the quiet-hours window is active right now (recomputed
    /// on a slow tick and on settings changes).
    pub scheduled: bool,
    /// A manual toggle's decision inside an active window: it holds
    /// until the window boundary re-arms the schedule.
    manual_override: Option<bool>,
    settings: Entity<Settings>,
    signals: Sender<SignalOut>,
    /// The toast currently on screen, if any: its notification id and window.
    toast: Option<(
        u32,
        gpui::WindowHandle<crate::notifications_view::ToastView>,
    )>,
    /// Ids the shell mints for its own notifications. Offset far past
    /// the daemon's counter so a self-push can never replace a client's
    /// toast by id collision.
    next_id: AtomicU32,
}

impl NotificationState {
    fn new(settings: Entity<Settings>, signals: Sender<SignalOut>, cx: &mut Context<Self>) -> Self {
        let dnd = settings.read(cx).notifications.dnd;
        let scheduled = {
            let s = settings.read(cx).notifications;
            match (s.quiet_from, s.quiet_to) {
                (Some(from), Some(to)) => {
                    crate::settings::in_quiet_window(now_minutes(), from, to)
                }
                _ => false,
            }
        };
        cx.observe(&settings, |this, _, cx| this.on_settings(cx))
            .detach();
        Self {
            notifications: Vec::new(),
            unread: 0,
            dnd,
            scheduled,
            manual_override: None,
            settings,
            signals,
            toast: None,
            next_id: AtomicU32::new(1_000_000),
        }
    }

    /// Settings moved: re-mirror the manual DND value and re-read the
    /// quiet window (an edit may have crossed a boundary).
    fn on_settings(&mut self, cx: &mut Context<Self>) {
        let dnd = self.settings.read(cx).notifications.dnd;
        if self.dnd != dnd {
            self.dnd = dnd;
        }
        self.evaluate_schedule(cx);
    }

    /// The DND state that actually gates toasts: inside the quiet
    /// window it is on unless the manual override says otherwise;
    /// outside, it is the plain mirrored toggle.
    pub fn dnd_effective(&self) -> bool {
        if self.scheduled {
            self.manual_override.unwrap_or(true)
        } else {
            self.dnd
        }
    }

    /// Re-read the quiet-hours window from settings; a boundary
    /// crossing flips `scheduled` and clears any manual override (the
    /// override's contract is to hold until the next boundary).
    fn evaluate_schedule(&mut self, cx: &mut Context<Self>) {
        let scheduled = {
            let s = self.settings.read(cx).notifications;
            match (s.quiet_from, s.quiet_to) {
                (Some(from), Some(to)) => {
                    crate::settings::in_quiet_window(now_minutes(), from, to)
                }
                _ => false,
            }
        };
        if scheduled != self.scheduled {
            self.scheduled = scheduled;
            self.manual_override = None;
            log::info!(
                "quiet hours {}",
                if scheduled { "on" } else { "off" }
            );
            cx.notify();
        }
    }

    /// Whether a notification with this urgency may show its toast:
    /// DND silences everything except a critical allowed through by
    /// the quiet-hours pass-through.
    fn toast_allowed(&self, urgency: Urgency, cx: &Context<Self>) -> bool {
        if !self.dnd_effective() {
            return true;
        }
        let settings = self.settings.read(cx);
        settings.notifications.quiet_urgent && urgency == Urgency::Critical
    }

    /// The shell notifying itself: the signer's "asks waiting" nudge and
    /// a failed act's sentence ride the same road a client's Notify call
    /// would: DND, history, and the toast behave identically.
    pub fn push(&mut self, mut notification: Notification, cx: &mut Context<Self>) {
        let urgency = notification.urgency;
        notification.id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.notifications
            .retain(|existing| existing.id != notification.id);
        for evicted in insert_notification(&mut self.notifications, notification, MAX_HISTORY) {
            Self::drop_icon(&evicted.icon, cx);
        }
        self.unread += 1;
        cx.notify();
        let id = self.notifications[0].id;
        if self.toast_allowed(urgency, cx) {
            self.show_toast(id, cx);
        }
    }

    fn apply(&mut self, event: DaemonEvent, cx: &mut Context<Self>) {
        match event {
            DaemonEvent::Notify(notification) => {
                let urgency = notification.urgency;
                // a replace removes the old entry and keeps the id
                let previous = self
                    .notifications
                    .iter()
                    .find(|existing| existing.id == notification.id)
                    .map(|existing| existing.icon.clone());
                self.notifications
                    .retain(|existing| existing.id != notification.id);
                for evicted in
                    insert_notification(&mut self.notifications, notification, MAX_HISTORY)
                {
                    Self::drop_icon(&evicted.icon, cx);
                }
                // a replaced entry's icon must not pin its atlas slot
                // unless the replacement carries the same image
                if let Some(previous) = previous {
                    let replacement = self.notifications.first().and_then(|n| n.icon.clone());
                    if !same_raster(&previous, &replacement) {
                        Self::drop_icon(&previous, cx);
                    }
                }
                self.unread += 1;
                cx.notify();
                let id = self.notifications[0].id;
                if self.toast_allowed(urgency, cx) {
                    self.show_toast(id, cx);
                }
            }
            DaemonEvent::CloseRequested(id) => self.dismiss(id, 3, cx),
        }
    }

    /// Remove a notification and emit Closed once: from the panel, the
    /// toast, or a client's CloseNotification call. Removal always happens;
    /// the closed flag only gates the dbus signal (an expired notification
    /// already signaled; it must still be dismissable from the history).
    pub fn dismiss(&mut self, id: u32, reason: u32, cx: &mut Context<Self>) {
        let Some(index) = self.notifications.iter().position(|n| n.id == id) else {
            return;
        };
        let already_closed = self.notifications[index].closed;
        let removed = self.notifications.remove(index);
        self.unread = self.unread.saturating_sub(1);
        cx.notify();
        if !already_closed {
            let _ = self.signals.try_send(SignalOut::Closed(id, reason));
        }
        // the entry is gone from everywhere that matters (the toast for
        // it closes just below), so its atlas tiles can go too
        Self::drop_icon(&removed.icon, cx);
        self.close_toast_if(id, cx);
    }

    /// Invoke one of the notification's actions, then close it (reason 2).
    pub fn invoke(&mut self, id: u32, action_key: String, cx: &mut Context<Self>) {
        let _ = self.signals.try_send(SignalOut::Action(id, action_key));
        self.dismiss(id, 2, cx);
    }

    /// The toast's own clock ran out (possibly paused and resumed along the
    /// way): signal Closed(1) once, close the toast, keep the history entry.
    pub fn expire_toast(&mut self, id: u32, cx: &mut Context<Self>) {
        if let Some(notification) = self.notifications.iter_mut().find(|n| n.id == id)
            && !notification.closed
        {
            notification.closed = true;
            let _ = self.signals.try_send(SignalOut::Closed(id, 1));
        }
        self.close_toast_if(id, cx);
    }

    /// DND rides Settings: the toggle flips it there, this mirrors it.
    /// Inside the quiet window the flip is an override (held until the
    /// boundary re-arms the schedule), not a write to the persisted
    /// manual value.
    pub fn toggle_dnd(&mut self, cx: &mut Context<Self>) {
        let dnd = !self.dnd_effective();
        if self.scheduled {
            self.manual_override = Some(dnd);
            cx.notify();
        } else {
            self.settings
                .update(cx, |settings, cx| settings.set_notifications_dnd(dnd, cx));
        }
    }

    /// The panel opened: everything is seen.
    pub fn mark_read(&mut self, cx: &mut Context<Self>) {
        if self.unread != 0 {
            self.unread = 0;
            cx.notify();
        }
    }

    /// Drop the whole history (the panel's clear button).
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        if !self.notifications.is_empty() {
            for notification in self.notifications.drain(..) {
                Self::drop_icon(&notification.icon, cx);
            }
            self.unread = 0;
            cx.notify();
        }
    }

    fn show_toast(&mut self, id: u32, cx: &mut Context<Self>) {
        self.close_toast(cx);
        let Some(notification) = self.notifications.iter().find(|n| n.id == id).cloned() else {
            return;
        };
        let duration = toast_duration(notification.expire_timeout);
        let state = cx.entity();
        // toasts hang just under the bar's bottom edge
        let top = cx
            .try_global::<crate::panel::PanelHost>()
            .map(|host| host.bar().panel_top + 8.)
            .unwrap_or(8.);
        match cx.open_window(
            crate::notifications_view::toast_window_options(top),
            |_, cx| {
                cx.new(|cx| {
                    let view = crate::notifications_view::ToastView {
                        state: state.clone(),
                        notification: notification.clone(),
                        duration,
                        started_at: Instant::now(),
                        paused_at: None,
                        paused_total: Duration::ZERO,
                        height: crate::notifications_view::TOAST_HEIGHT,
                    };
                    crate::notifications_view::ToastView::start_ticker(cx);
                    view
                })
            },
        ) {
            Ok(handle) => self.toast = Some((id, handle)),
            Err(err) => log::error!("failed to open toast window: {err:#}"),
        }
        // the toast's own ticker owns the clock (pauses while hovered) and
        // expires it; see ToastView::start_ticker
    }

    /// Close whatever toast is on screen.
    fn close_toast(&mut self, cx: &mut Context<Self>) {
        if let Some((_, handle)) = self.toast.take() {
            crate::panel::defer_close(
                Box::new(move |cx| {
                    if let Err(err) = handle.update(cx, |_, window, _| window.remove_window()) {
                        log::error!("closing toast failed: {err:#}");
                    }
                }),
                cx,
            );
        }
    }

    /// Free an icon's atlas tiles. The `img` element never drops the
    /// tile it paints, so a notification's icon would otherwise pin its
    /// slot in the polychrome atlas forever: the atlas grows a new page
    /// once full, and on a real session those pages are what the
    /// suspend cycle swaps out.
    fn drop_icon(icon: &Option<IconImage>, cx: &mut Context<Self>) {
        if let Some(IconImage::Raster(image)) = icon {
            cx.drop_image(image.clone(), None);
        }
    }

    /// Close the toast if it's showing this notification.
    fn close_toast_if(&mut self, id: u32, cx: &mut Context<Self>) {
        if self
            .toast
            .as_ref()
            .is_some_and(|(toast_id, _)| *toast_id == id)
        {
            self.close_toast(cx);
        }
    }
}

/// Newest first, capped: pure list surgery, testable without a context.
/// Returns the entries the cap pushed out, whose icons the caller must
/// drop from the atlas.
fn insert_notification(
    list: &mut Vec<Notification>,
    notification: Notification,
    cap: usize,
) -> Vec<Notification> {
    list.insert(0, notification);
    if list.len() > cap {
        return list.split_off(cap);
    }
    Vec::new()
}

/// Whether two icon slots hold the same decoded image.
fn same_raster(a: &Option<IconImage>, b: &Option<IconImage>) -> bool {
    match (a, b) {
        (Some(IconImage::Raster(a)), Some(IconImage::Raster(b))) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

/// "now", "3m", "2h", "1d" (the list's timestamps).
pub fn time_ago(received_at: DateTime<Local>) -> String {
    let elapsed = Local::now() - received_at;
    let minutes = elapsed.num_minutes();
    if minutes < 1 {
        "now".to_string()
    } else if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 60 * 24 {
        format!("{}h", minutes / 60)
    } else {
        format!("{}d", minutes / (60 * 24))
    }
}

/// Minutes since local midnight: the quiet-hours check's "now".
fn now_minutes() -> u32 {
    use chrono::Timelike;
    let now = Local::now().time();
    now.hour() * 60 + now.minute()
}

/// Start the daemon and hand back the state it feeds. The daemon runs on the
/// background executor; events ride a channel into the app's loop (the
/// same shape as the niri stream); signal emission rides one back.
pub fn start(settings: Entity<Settings>, cx: &mut App) -> Entity<NotificationState> {
    let (events_tx, events_rx) = unbounded();
    let (signals_tx, signals_rx) = unbounded();

    let state = cx.new(|cx| NotificationState::new(settings, signals_tx, cx));

    cx.background_spawn(async move {
        if let Err(err) = run_daemon(events_tx, signals_rx).await {
            log::error!("notification daemon terminated: {err:#}");
        }
    })
    .detach();

    let event_state = state.clone();
    cx.spawn(async move |cx| {
        while let Ok(event) = events_rx.recv().await {
            let _ = event_state.update(cx, |state, cx| state.apply(event, cx));
        }
    })
    .detach();

    // the quiet-hours clock: a slow tick re-evaluates the window, so a
    // boundary crossing needs no other excuse to flip the schedule
    let tick_state = state.clone();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_secs(30))
                .await;
            tick_state.update(cx, |state, cx| state.evaluate_schedule(cx));
        }
    })
    .detach();

    state
}

async fn run_daemon(
    events: Sender<DaemonEvent>,
    signals: Receiver<SignalOut>,
) -> anyhow::Result<()> {
    // one daemon per bus: if another owns the name, claiming fails loudly
    let daemon = KumaDaemon {
        events,
        next_id: AtomicU32::new(0),
    };
    let connection = zbus::connection::Builder::session()?
        .name("org.freedesktop.Notifications")?
        .serve_at(OBJECT_PATH, daemon)?
        .build()
        .await?;
    let emitter = SignalEmitter::new(&connection, OBJECT_PATH)?;
    log::info!("notification daemon online at {OBJECT_PATH}");

    while let Ok(signal) = signals.recv().await {
        match signal {
            SignalOut::Closed(id, reason) => {
                KumaDaemon::notification_closed(&emitter, id, reason).await?;
            }
            SignalOut::Action(id, action_key) => {
                KumaDaemon::action_invoked(&emitter, id, action_key.clone()).await?;
                KumaDaemon::notification_closed(&emitter, id, 2).await?;
            }
        }
    }
    Ok(())
}

/// Read access for views: toast duration cap is a daemon-side constant, the
/// state owns the rest.

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Str;

    fn notification(id: u32) -> Notification {
        Notification {
            id,
            app_name: "test".into(),
            icon: None,
            summary: "summary".into(),
            body: String::new(),
            actions: Vec::new(),
            urgency: Urgency::Normal,
            received_at: Local::now(),
            expire_timeout: -1,
            closed: false,
        }
    }

    #[test]
    fn actions_pairs_survive_odd_lists() {
        let flat = [
            "default".to_string(),
            "Default".to_string(),
            "key".to_string(),
        ];
        assert_eq!(
            parse_actions(&flat),
            vec![("default".to_string(), "Default".to_string())]
        );
        assert!(parse_actions(&[]).is_empty());
    }

    #[test]
    fn urgency_hint_maps_to_levels() {
        let mut hints = HashMap::new();
        hints.insert(
            "urgency".to_string(),
            OwnedValue::from(Str::from("critical")),
        );
        assert_eq!(urgency_from_hints(&hints), Urgency::Critical);

        hints.insert("urgency".to_string(), OwnedValue::from(Str::from("normal")));
        assert_eq!(urgency_from_hints(&hints), Urgency::Normal);

        // missing hints default to normal
        hints.remove("urgency");
        assert_eq!(urgency_from_hints(&hints), Urgency::Normal);
    }

    #[test]
    fn toast_duration_honors_client_hints_with_a_cap() {
        assert_eq!(toast_duration(-1), DEFAULT_EXPIRE);
        assert_eq!(toast_duration(0), MAX_EXPIRE);
        assert_eq!(toast_duration(2000), Duration::from_secs(2));
        // absurd and never-expiring hints clamp to the cap
        assert_eq!(toast_duration(600_000), MAX_EXPIRE);
    }

    #[test]
    fn history_is_newest_first_and_capped() {
        let mut list = Vec::new();
        let mut evicted_all = Vec::new();
        for id in 1..=60 {
            evicted_all.extend(insert_notification(&mut list, notification(id), 50));
        }
        // newest first, and the 50 newest survived
        assert_eq!(list.len(), 50);
        assert_eq!(list[0].id, 60);
        assert_eq!(list.last().unwrap().id, 11);
        // the 10 oldest were pushed out, oldest first: their icons are
        // what the caller must drop from the atlas
        let mut evicted_ids: Vec<u32> = evicted_all.iter().map(|n| n.id).collect();
        evicted_ids.sort_unstable();
        assert_eq!(evicted_ids, (1..=10).collect::<Vec<u32>>());
        // at the cap, insertion pushes out exactly the oldest
        let evicted = insert_notification(&mut list, notification(61), 50);
        assert_eq!(evicted.iter().map(|n| n.id).collect::<Vec<u32>>(), [11]);
        assert_eq!(list[0].id, 61);
        assert_eq!(list.len(), 50);
    }

    #[test]
    fn same_raster_compares_by_pointer() {
        fn render_image() -> std::sync::Arc<gpui::RenderImage> {
            std::sync::Arc::new(gpui::RenderImage::new(smallvec::smallvec![image::Frame::new(
                image::RgbaImage::new(1, 1)
            )]))
        }
        let image = render_image();
        let raster = Some(IconImage::Raster(image.clone()));
        // the same Arc is the same image, clones included
        assert!(same_raster(&raster, &Some(IconImage::Raster(image.clone()))));
        // a distinct image is not
        let other = Some(IconImage::Raster(render_image()));
        assert!(!same_raster(&raster, &other));
        // nothing matches nothing
        assert!(!same_raster(&None, &None));
    }

    #[test]
    fn time_ago_buckets() {
        let now = Local::now();
        assert_eq!(time_ago(now), "now");
        assert_eq!(time_ago(now - chrono::Duration::minutes(3)), "3m");
        assert_eq!(time_ago(now - chrono::Duration::hours(2)), "2h");
        assert_eq!(time_ago(now - chrono::Duration::days(1)), "1d");
    }

    #[test]
    fn icon_spec_prefers_app_icon_then_hint() {
        let mut hints = HashMap::new();
        assert_eq!(icon_spec_from("", &hints), "");

        // the image-path hint, either spelling
        hints.insert(
            "image-path".to_string(),
            OwnedValue::from(Str::from("/usr/share/icons/x.png")),
        );
        assert_eq!(icon_spec_from("", &hints), "/usr/share/icons/x.png");

        // app_icon wins over the hint
        assert_eq!(icon_spec_from("firefox", &hints), "firefox");
    }
}
