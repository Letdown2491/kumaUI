//! The notification surfaces: the toast that pops under the bar when a
//! notification arrives, and the history panel behind the bell widget.

use std::time::{Duration, Instant};

use gpui::{
    Bounds, Context, Div, Entity, ImageSource, Render, SharedString, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, img,
    layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
    point,
    prelude::*,
    px, relative, rgb, rgba, size, svg,
};

use crate::imaging::IconImage;
use crate::notifications::{Notification, NotificationState, time_ago};
use crate::panel::PanelGeometry;
use crate::panel_kit as kit;
use crate::settings::Settings;

/// The toast card's corner radius: the drain bar insets itself by this so
/// it stays inside the rounded corners (gpui's overflow clip is rectangular).
const TOAST_RADIUS: f32 = 12.;
/// The toast window's first guess at its height; the measured-height flow
/// refines it to the content (a wrapped body makes it taller).
pub const TOAST_HEIGHT: f32 = 88.;
/// How tall the wrapped body may grow (two lines of 11px text) before it
/// clips; toasts are taps on the shoulder, not reading panes.
const BODY_MAX_HEIGHT: f32 = 30.;

/// The transient banner for one arriving notification: no scrim, no
/// keyboard, one surface dismissed by any click or its own clock. The
/// clock pauses while the pointer hovers; the drain bar at the bottom
/// shows what's left of it, and the window's height measures itself.
pub struct ToastView {
    pub state: Entity<NotificationState>,
    pub notification: Notification,
    /// How long the toast hangs (the same duration the auto-close uses):
    /// the drain bar divides elapsed by it.
    pub duration: Duration,
    pub started_at: Instant,
    /// When the pointer entered (the current pause), and the pause time
    /// already banked by earlier hovers.
    pub paused_at: Option<Instant>,
    pub paused_total: Duration,
    /// The live window height: refined by the measured-height flow.
    pub height: f32,
}

impl ToastView {
    /// Effective elapsed time: wall time minus banked pauses and the
    /// pause in progress, if any.
    fn elapsed(&self) -> Duration {
        let mut elapsed = self.started_at.elapsed();
        if let Some(paused_at) = self.paused_at {
            elapsed = elapsed.saturating_sub(paused_at.elapsed());
        }
        elapsed.saturating_sub(self.paused_total)
    }

    /// Fraction of the toast's life remaining, 1 → 0.
    fn progress(&self) -> f32 {
        let elapsed = self.elapsed().as_secs_f32();
        (1.0 - elapsed / self.duration.as_secs_f32()).clamp(0.0, 1.0)
    }

    /// The actions that render as buttons: every one of the client's
    /// but the default, which the body's click fires instead of a
    /// button (the spec's "default" key is the banner click).
    fn actions(&self) -> Vec<(String, String)> {
        self.notification
            .actions
            .iter()
            .filter(|(key, _)| key != "default")
            .cloned()
            .collect()
    }

    /// The toast's clock: repaint ~7×/sec so the bar drains smoothly,
    /// pause while hovered, and expire when the drained time runs out,
    /// which may be much later than `duration` if the user keeps the
    /// pointer on it. Called by the toast's constructor in
    /// `notifications.rs`.
    pub fn start_ticker(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;
                match this.update(cx, |this, cx| {
                    cx.notify();
                    if this.progress() <= 0.0 {
                        expire_toast(this, cx);
                        false
                    } else {
                        true
                    }
                }) {
                    Ok(true) => {}
                    _ => break,
                }
            }
        })
        .detach();
    }
}

fn expire_toast(view: &mut ToastView, cx: &mut Context<ToastView>) {
    let state = view.state.clone();
    let id = view.notification.id;
    state.update(cx, |state, cx| state.expire_toast(id, cx));
}

/// An icon at a given size: the shared rendering of decoded icons.
fn icon_element(icon: IconImage, size: f32) -> Div {
    match icon {
        IconImage::Raster(raster) => div().child(
            img(ImageSource::Render(raster))
                .size(px(size))
                .into_any_element(),
        ),
        IconImage::Svg(bytes) => div().child(svg().data(&bytes).size(px(size)).into_any_element()),
    }
}

impl Render for ToastView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.clone();
        let id = self.notification.id;
        let notification = self.notification.clone();
        // the body's click fires the client's default action when it
        // has one (the spec's "default" key), else the plain dismiss
        let default_action = notification
            .actions
            .iter()
            .find(|(key, _)| key == "default")
            .map(|(key, _)| key.clone());
        let toast_state = state.clone();
        let toast_actions = self.actions();
        let progress = self.progress();
        let height = self.height;

        // a toast is a banner, not a drawer: plain rounded card, no coves;
        // it hangs under the bar's transparent stretch, not its content
        let content = div()
            .id("toast")
            .flex()
            .flex_col()
            .rounded_xl()
            .bg(rgba(crate::theme::current().panel_bg))
            .border_1()
            .border_color(rgb(crate::theme::current().divider))
            .overflow_hidden()
            // hovering pauses the clock; entering starts a pause (the
            // compositor's synthesized enter-move triggers this), leaving
            // banks it
            .on_mouse_move(cx.listener(|this, _: &gpui::MouseMoveEvent, _, cx| {
                if this.paused_at.is_none() {
                    this.paused_at = Some(Instant::now());
                    cx.notify();
                }
            }))
            .on_mouse_exit(cx.listener(|this, _: &gpui::MouseExitEvent, _, cx| {
                if let Some(paused_at) = this.paused_at.take() {
                    this.paused_total += paused_at.elapsed();
                    cx.notify();
                }
            }))
            // the body is the click target (default action or dismiss);
            // the actions row and the drain bar are siblings of it, so
            // a button click never also dismisses
            .child(
                div()
                    .id("toast-body")
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .px(px(14.))
                    .pt(px(10.))
                    .pb(px(8.))
                    .min_h_0()
                    .flex_1()
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        toast_state.update(cx, |state, cx| match &default_action {
                            Some(key) => state.invoke(id, key.clone(), cx),
                            None => state.dismiss(id, 2, cx),
                        });
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .when_some(notification.icon.clone(), |el, icon| {
                                el.child(icon_element(icon, 14.))
                            })
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(crate::theme::current().text_dim))
                                    .truncate()
                                    .child(notification.app_name.clone()),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(crate::theme::current().text))
                            .truncate()
                            .child(notification.summary.clone()),
                    )
                    .when(!notification.body.is_empty(), |el| {
                        el.child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(crate::theme::current().text_dim))
                                .max_h(px(BODY_MAX_HEIGHT))
                                .overflow_hidden()
                                .child(notification.body.clone()),
                        )
                    }),
            )
            .when(!toast_actions.is_empty(), |el| {
                el.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_1()
                        .px(px(14.))
                        .pb(px(8.))
                        .children(toast_actions.into_iter().map(|(key, label)| {
                            let action_state = state.clone();
                            let action_key = key.clone();
                            kit::button(
                                SharedString::from(format!("toast-action-{id}-{key}")),
                                &label,
                                None,
                                kit::ButtonVariant::Ghost,
                                move |_, _, cx| {
                                    action_state.update(cx, |state, cx| {
                                        state.invoke(id, action_key.clone(), cx)
                                    });
                                },
                            )
                        })),
                )
            })
            .child(
                // the closing indication: the drain bar. Inset by the card's
                // corner radius: gpui's overflow clip is rectangular, so a
                // full-width bar would paint past the rounded corners.
                div().h(px(2.)).w_full().px(px(TOAST_RADIUS)).child(
                    div()
                        .h_full()
                        .w_full()
                        .rounded_full()
                        .bg(rgba(crate::theme::current().divider_soft))
                        .overflow_hidden()
                        .child(div().h_full().w(relative(progress)).bg(rgb(crate::theme::current().accent))),
                ),
            );

        // the measured-height flow: the content's laid-out height refines
        // this view's geometry (the calendar's pattern). The +2 covers the
        // border and drain bar that paint inside the card but outside the
        // measured child. The surface is never resized: it spans the
        // output, and the drawer reads the height at render.
        let view = cx.weak_entity();
        let width = 360.;
        let measured =
            crate::panel::MeasureHeight::new(content, move |content_height, _window, cx| {
                let Some(view) = view.upgrade() else {
                    return;
                };
                view.update(cx, |this, cx| {
                    let new_height = (content_height + 2.).clamp(64., 140.);
                    if (this.height - new_height).abs() > 0.5 {
                        this.height = new_height;
                        cx.notify();
                    }
                });
            });

        div()
            .size_full()
            .flex()
            .child(div().w(px(width)).h(px(height)).child(measured))
    }
}

/// Toasts hang under the bar's right edge, not centered on the bar content:
/// they're transient, not drawers. `top` is just under the bar's bottom edge,
/// read from the panel host at open time.
pub fn toast_window_options(edge: f32, position: crate::settings::BarPosition) -> WindowOptions {
    // toasts pin off the bar's inner face at the screen's right; a
    // bottom bar flips them above the bar
    let (anchor, margin) = match position {
        crate::settings::BarPosition::Top => (
            Anchor::TOP | Anchor::RIGHT,
            (px(edge), px(8.), px(0.), px(0.)),
        ),
        crate::settings::BarPosition::Bottom => (
            Anchor::BOTTOM | Anchor::RIGHT,
            (px(0.), px(8.), px(edge), px(0.)),
        ),
    };
    WindowOptions {
        titlebar: None,
        // client decorations: layer surfaces have no server side
        window_decorations: Some(gpui::WindowDecorations::Client),
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(360.), px(TOAST_HEIGHT)),
        })),
        app_id: Some("kuma-shell-toast".into()),
        window_background: WindowBackgroundAppearance::Transparent,
        kind: WindowKind::LayerShell(LayerShellOptions {
            namespace: "kuma-shell-toast".into(),
            layer: Layer::Overlay,
            exclusive_zone: Some(px(-1.)),
            anchor,
            keyboard_interactivity: KeyboardInteractivity::None,
            margin: Some(margin),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The history panel behind the bell: newest first, DND toggle, clear-all,
/// actions invoked straight from the card.
pub struct NotificationsView {
    geometry: PanelGeometry,
    state: Entity<NotificationState>,
    /// The app groups the panel expanded (the rest render collapsed,
    /// newest card visible): keyed by app name.
    expanded: std::collections::HashSet<String>,
}

impl NotificationsView {
    pub fn new(
        state: Entity<NotificationState>,
        _settings: Entity<Settings>,
        _window: &mut Window,
        cx: &mut Context<Self>,
        geometry: PanelGeometry,
    ) -> Self {
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        // opening the panel is seeing the notifications
        state.update(cx, |state, cx| state.mark_read(cx));
        Self {
            geometry,
            state,
            expanded: std::collections::HashSet::new(),
        }
    }
}

impl Render for NotificationsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let notifications: Vec<Notification> = self.state.read(cx).notifications.clone();
        let state = self.state.read(cx);
        let dnd = state.dnd_effective();
        let scheduled = state.scheduled;
        let dnd_label = if scheduled {
            "Do not disturb (quiet hours)"
        } else {
            "Do not disturb"
        };
        let list = if notifications.is_empty() {
            kit::empty_state(
                "icons/bell.svg",
                "Nothing has arrived",
                "Notifications land here as they come",
            )
            .into_any_element()
        } else {
            let groups = group_by_app(&notifications);
            div()
                .id("notification-list")
                .flex()
                .flex_col()
                .gap_1()
                .overflow_y_scroll()
                .children(groups.into_iter().map(|group| match group.len() {
                    1 => notification_card(&group.notifications[0], cx).into_any_element(),
                    _ => app_group(group, &self.expanded, cx).into_any_element(),
                }))
                .into_any_element()
        };

        let header = kit::pane_header("Notifications").when(!notifications.is_empty(), |el| {
            el.child(kit::button(
                "clear-all",
                "Clear all",
                Some("icons/trash.svg"),
                kit::ButtonVariant::Ghost,
                cx.listener(|this, _, _, cx| {
                    this.state.update(cx, |state, cx| state.clear(cx));
                }),
            ))
        });

        let content = div()
            .flex()
            .flex_col()
            .pt(px(10.))
            .px(px(12.))
            .pb(px(12.))
            .gap_2()
            .size_full()
            .child(header)
            .child(crate::controls::toggle_row(
                "dnd-toggle",
                "icons/bell.svg",
                dnd_label,
                dnd,
                cx.listener(|this, _, _, cx| {
                    this.state.update(cx, |state, cx| state.toggle_dnd(cx));
                }),
            ))
            .child(list);

        crate::panel::chrome(self.geometry, window, cx, content)
    }
}

/// A run of consecutive notifications from one app: the panel stacks
/// them so a morning of pings reads as one row plus a count.
struct AppGroup {
    app_name: String,
    /// The newest notification's icon (the header shows it).
    icon: Option<IconImage>,
    /// Newest first, as the history list runs.
    notifications: Vec<Notification>,
}

impl AppGroup {
    fn len(&self) -> usize {
        self.notifications.len()
    }
}

/// Group consecutive same-app runs: pure list surgery, testable
/// without a context. Non-consecutive arrivals from one app stay in
/// their own places; the list's order never changes.
fn group_by_app(list: &[Notification]) -> Vec<AppGroup> {
    let mut groups: Vec<AppGroup> = Vec::new();
    for notification in list {
        match groups.last_mut() {
            Some(group) if group.app_name == notification.app_name => {
                group.notifications.push(notification.clone());
            }
            _ => groups.push(AppGroup {
                app_name: notification.app_name.clone(),
                icon: notification.icon.clone(),
                notifications: vec![notification.clone()],
            }),
        }
    }
    groups
}

/// One app group: a header row (icon, name, count, expand toggle) plus
/// the newest card always, the rest when expanded.
fn app_group(
    group: AppGroup,
    expanded: &std::collections::HashSet<String>,
    cx: &mut Context<NotificationsView>,
) -> gpui::Stateful<Div> {
    let app_name = group.app_name.clone();
    let app_label = app_name.clone();
    let count = group.len();
    let is_expanded = expanded.contains(&app_name);
    let newest = &group.notifications[0];

    kit::card(SharedString::from(format!("group-{app_name}")))
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .id(SharedString::from(format!("group-header-{app_name}")))
                .flex()
                .items_center()
                .gap_1()
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.expanded.remove(&app_label) {
                        this.expanded.insert(app_label.clone());
                    }
                    cx.notify();
                }))
                .when_some(group.icon.clone(), |el, icon| {
                    el.child(icon_element(icon, 14.))
                })
                .child(
                    div()
                        .flex_1()
                        .text_size(px(10.))
                        .text_color(rgb(crate::theme::current().text_dim))
                        .truncate()
                        .child(app_name.clone()),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(crate::theme::current().text_dim))
                        .child(format!("{count}")),
                )
                .child(
                    gpui::svg()
                        .path(if is_expanded {
                            "icons/arrow-up.svg"
                        } else {
                            "icons/arrow-down.svg"
                        })
                        .size(px(10.))
                        .text_color(rgb(crate::theme::current().text_dim)),
                ),
        )
        .child(notification_card(newest, cx))
        .when(is_expanded, |el| {
            el.children(
                group.notifications[1..]
                    .iter()
                    .map(|notification| notification_card(notification, cx)),
            )
        })
}

/// One history card: app + time, summary, body, actions.
fn notification_card(
    notification: &Notification,
    cx: &mut Context<NotificationsView>,
) -> gpui::Stateful<Div> {
    let id = notification.id;
    let app_name = notification.app_name.clone();
    let summary = notification.summary.clone();
    let body = notification.body.clone();
    let when = time_ago(notification.received_at);
    let actions = notification.actions.clone();

    kit::card(SharedString::from(format!("notification-{id}")))
        .cursor_pointer()
        .hover(|el| el.bg(rgb(crate::theme::current().surface_hover)))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.state.update(cx, |state, cx| state.dismiss(id, 2, cx));
        }))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .min_w_0()
                        .when_some(notification.icon.clone(), |el, icon| {
                            el.child(icon_element(icon, 14.))
                        })
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(crate::theme::current().text_dim))
                                .truncate()
                                .child(app_name),
                        ),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(crate::theme::current().text_dim))
                        .child(when),
                ),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(crate::theme::current().text))
                .child(summary),
        )
        .when(!body.is_empty(), |el| {
            el.child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(crate::theme::current().text_dim))
                    .child(body),
            )
        })
        .when(!actions.is_empty(), |el| {
            el.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .children(actions.into_iter().map(|(key, label)| {
                        let key = key.clone();
                        kit::button(
                            SharedString::from(format!("action-{id}-{key}")),
                            &label,
                            None,
                            kit::ButtonVariant::Ghost,
                            cx.listener(move |this, _, _, cx| {
                                this.state
                                    .update(cx, |state, cx| state.invoke(id, key.clone(), cx));
                            }),
                        )
                    })),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(id: u32, app: &str) -> Notification {
        Notification {
            id,
            app_name: app.into(),
            icon: None,
            summary: format!("summary {id}"),
            body: String::new(),
            actions: Vec::new(),
            urgency: crate::notifications::Urgency::Normal,
            received_at: chrono::Local::now(),
            expire_timeout: -1,
            closed: false,
        }
    }

    #[test]
    fn consecutive_same_app_runs_group() {
        // the history runs newest first: ids 4, 3 are two GitLab
        // arrivals, then Files, then an older GitLab
        let list = vec![
            notification(4, "GitLab"),
            notification(3, "GitLab"),
            notification(2, "Files"),
            notification(1, "GitLab"),
        ];
        let groups = group_by_app(&list);
        // two runs of GitLab around Files: three groups
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].len(), 2);
        assert_eq!(groups[1].len(), 1);
        assert_eq!(groups[2].len(), 1);
        // newest first inside a group, order preserved overall
        assert_eq!(groups[0].notifications[0].id, 4);
        assert_eq!(groups[0].notifications[1].id, 3);
        assert_eq!(groups[2].notifications[0].id, 1);
    }

    #[test]
    fn a_single_app_is_one_group_and_singletons_render_plain() {
        let list = vec![notification(2, "GitLab"), notification(1, "GitLab")];
        let groups = group_by_app(&list);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 2);
        // an empty history groups to nothing
        assert!(group_by_app(&[]).is_empty());
    }
}
