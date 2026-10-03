use std::rc::Rc;
use std::time::Duration;

use chrono::Local;
use gpui::{
    AnyElement, AnyView, App, Bounds, Context, Div, Entity, MouseMoveEvent, Pixels, Point, Render,
    SharedString, Window, div, img, point, prelude::*, px, rgb, rgba, size, svg,
};
use log::error;

use crate::session::SessionState;
use crate::settings::{
    BarAlign, BarRadius, CornerRounding, Section, Settings, WidgetConfig, WidgetIconSpec,
    WidgetKind, WidgetMode,
};
use crate::sysmon::{Playback, RecordingState, SysMon};
use crate::theme;
use crate::theme::*;

// transparent headroom below the bar content so tooltips have room to render
const TOOLTIP_ROOM: f32 = 200.;

struct WorkspaceItem {
    id: u64,
    label: String,
    focused: bool,
    active: bool,
    urgent: bool,
}

pub struct ShellBar {
    niri: Entity<SessionState>,
    sysmon: Entity<SysMon>,
    settings: Entity<Settings>,
    notifications: Entity<crate::notifications::NotificationState>,
    tray: Entity<crate::tray::TrayState>,
    nostr: Entity<crate::nostr::NostrState>,
    applied_geometry: Option<(f32, f32, f32, f32)>,
    clock: String,
    /// Set by the panel host on every panel transition; consumed at the next
    /// render, which snapshots the mouse position into `tooltips_suppressed_at`.
    suppress_tooltips_requested: bool,
    /// While Some, the snapshot is where the pointer sat when the panel
    /// transitioned. Tooltip builders are off, because once the scrim dies
    /// the compositor re-enters the bar with a synthesized MouseMove (no real
    /// motion) that would resurrect the last hovered tooltip. Ends at the
    /// first move to a genuinely different position.
    tooltips_suppressed_at: Option<Point<Pixels>>,
}

impl ShellBar {
    pub fn new(
        niri: Entity<SessionState>,
        sysmon: Entity<SysMon>,
        settings: Entity<Settings>,
        notifications: Entity<crate::notifications::NotificationState>,
        tray: Entity<crate::tray::TrayState>,
        nostr: Entity<crate::nostr::NostrState>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&niri, |_, _, cx| cx.notify()).detach();
        cx.observe(&sysmon, |_, _, cx| cx.notify()).detach();
        cx.observe(&settings, |_, _, cx| cx.notify()).detach();
        cx.observe(&notifications, |_, _, cx| cx.notify()).detach();
        cx.observe(&tray, |_, _, cx| cx.notify()).detach();
        cx.observe(&nostr, |_, _, cx| cx.notify()).detach();

        cx.spawn(async move |this, cx| {
            loop {
                let now = Local::now().format("%H:%M").to_string();
                let alive = this
                    .update(cx, |this, cx| {
                        if this.clock != now {
                            this.clock = now;
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(1)).await;
            }
        })
        .detach();

        // the panel host needs to know about transitions it triggers
        let this = cx.weak_entity();
        cx.update_global(|host: &mut crate::panel::PanelHost, _| {
            host.set_bar(this);
        });

        Self {
            niri,
            sysmon,
            settings,
            notifications,
            tray,
            nostr,
            applied_geometry: None,
            clock: String::new(),
            suppress_tooltips_requested: false,
            tooltips_suppressed_at: None,
        }
    }

    /// A panel opened or closed. Tooltip builders stay off until the pointer
    /// genuinely moves (see `tooltips_suppressed_at`).
    pub fn suppress_tooltips(&mut self, cx: &mut Context<Self>) {
        self.suppress_tooltips_requested = true;
        cx.notify();
    }
}

impl Render for ShellBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.settings.read(cx).clone();
        let bar = settings.bar.clone();

        // consume a panel transition: tooltips gate off from this frame,
        // pinned to where the pointer sat: the compositor's synthesized
        // re-entry move lands on this exact position, so it won't unsuppress
        if self.suppress_tooltips_requested {
            self.suppress_tooltips_requested = false;
            self.tooltips_suppressed_at = Some(window.mouse_position());
        }
        let tooltips_on = self.tooltips_suppressed_at.is_none();

        // Sync the window with the configured bar geometry: the surface stays
        // stretched full-width and transparent; width/align/offset are applied
        // by the content div, and the input region keeps clicks on transparent
        // areas passing through to whatever is below.
        let viewport = window.viewport_size();
        let content_width = viewport.width * bar.width.fraction();
        let content_x = match bar.align {
            BarAlign::Left => px(0.),
            BarAlign::Center => ((viewport.width - content_width) / 2.).max(px(0.)),
            BarAlign::Right => (viewport.width - content_width).max(px(0.)),
        };
        let geometry = (
            f32::from(bar.height),
            f32::from(bar.offset_top),
            f32::from(content_width),
            f32::from(viewport.width),
        );
        let changed = match self.applied_geometry {
            None => true,
            Some(last) => {
                (last.0 - geometry.0).abs() > 0.5
                    || (last.1 - geometry.1).abs() > 0.5
                    || (last.2 - geometry.2).abs() > 0.5
                    || (last.3 - geometry.3).abs() > 0.5
            }
        };
        if changed {
            // niri kills the surface with a wp_viewport protocol error if we
            // resize before the first configure reports a real width
            if viewport.width > px(0.) {
                window.resize(size(
                    viewport.width,
                    px(bar.height + bar.offset_top + TOOLTIP_ROOM),
                ));
                // panels center on the bar content's center line and hang
                // flush under its bottom edge
                cx.global_mut::<crate::panel::PanelHost>()
                    .report_bar_geometry(crate::panel::BarGeometry {
                        content_x: content_x.into(),
                        content_width: content_width.into(),
                        panel_top: (f32::from(bar.offset_top) + f32::from(bar.height)).into(),
                    });
            }
            window.set_exclusive_zone(px(bar.height + bar.offset_top));
            if content_width > px(0.) {
                window.set_input_region(Some(&[Bounds {
                    origin: point(content_x, px(bar.offset_top)),
                    size: size(content_width, px(bar.height)),
                }]));
            } else {
                window.set_input_region(None);
            }
            self.applied_geometry = Some(geometry);
        }

        // relative root gives the absolutely-positioned content a containing
        // block; offsets on the root element itself would be ignored
        let content = apply_corner_radii(
            div()
                .absolute()
                .top(px(bar.offset_top))
                .left(content_x)
                .w(content_width)
                .h(px(bar.height))
                .grid()
                .grid_cols(3)
                .items_center()
                .px_2()
                .bg(rgba(theme::PANEL_BG)),
            bar.radius,
            bar.corners,
        )
        .child(self.render_section(Section::Left, cx, tooltips_on))
        .child(
            div()
                .min_w_0()
                .flex()
                .justify_center()
                .gap_1()
                .overflow_hidden()
                .children(
                    settings
                        .widgets(Section::Center)
                        .iter()
                        .filter_map(|widget| self.render_widget(widget, cx, tooltips_on)),
                ),
        )
        .child(
            div()
                .flex()
                .justify_end()
                .gap_1()
                .child(self.render_section(Section::Right, cx, tooltips_on))
                .when_some(self.sysmon.read(cx).recording.clone(), |el, rec| {
                    el.child(recording_indicator(&rec, cx, tooltips_on))
                })
                .child(gear_button(cx, tooltips_on)),
        );

        div()
            .size_full()
            .relative()
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                // real pointer motion is what re-arms tooltips
                let Some(suppressed_at) = this.tooltips_suppressed_at else {
                    return;
                };
                let delta = event.position - suppressed_at;
                if delta.x.abs() > px(0.5) || delta.y.abs() > px(0.5) {
                    this.tooltips_suppressed_at = None;
                    cx.notify();
                }
            }))
            .child(content)
    }
}

fn apply_corner_radii(el: Div, radius: BarRadius, corners: CornerRounding) -> Div {
    match radius {
        BarRadius::None => el,
        BarRadius::Sm => self_corner(
            el,
            corners,
            |e| e.rounded_tl_sm(),
            |e| e.rounded_tr_sm(),
            |e| e.rounded_bl_sm(),
            |e| e.rounded_br_sm(),
        ),
        BarRadius::Md => self_corner(
            el,
            corners,
            |e| e.rounded_tl_md(),
            |e| e.rounded_tr_md(),
            |e| e.rounded_bl_md(),
            |e| e.rounded_br_md(),
        ),
        BarRadius::Lg => self_corner(
            el,
            corners,
            |e| e.rounded_tl_lg(),
            |e| e.rounded_tr_lg(),
            |e| e.rounded_bl_lg(),
            |e| e.rounded_br_lg(),
        ),
        BarRadius::Xl => self_corner(
            el,
            corners,
            |e| e.rounded_tl_xl(),
            |e| e.rounded_tr_xl(),
            |e| e.rounded_bl_xl(),
            |e| e.rounded_br_xl(),
        ),
    }
}

fn self_corner(
    el: Div,
    corners: CornerRounding,
    tl: impl Fn(Div) -> Div,
    tr: impl Fn(Div) -> Div,
    bl: impl Fn(Div) -> Div,
    br: impl Fn(Div) -> Div,
) -> Div {
    let el = if corners.top_left { tl(el) } else { el };
    let el = if corners.top_right { tr(el) } else { el };
    let el = if corners.bottom_left { bl(el) } else { el };
    if corners.bottom_right { br(el) } else { el }
}

/// A tooltip that reads its text fresh from SysMon every render, so the value
/// updates while it hangs there, not just when it first appears.
struct SysmonTooltip {
    sysmon: Entity<SysMon>,
    text: Rc<dyn Fn(&SysMon, &App) -> SharedString>,
}

impl Render for SysmonTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text = (self.text)(self.sysmon.read(cx), cx);
        div()
            .px_2()
            .py_1()
            .rounded_sm()
            .bg(rgb(0x11111B))
            .text_size(px(12.))
            .text_color(rgb(TEXT))
            .child(text)
    }
}

/// `text_tooltip`'s live twin: the formatter reads SysMon at each render, so
/// the tooltip tracks the monitors instead of freezing at show time.
fn sysmon_tooltip(
    sysmon: Entity<SysMon>,
    text: impl Fn(&SysMon, &App) -> SharedString + 'static,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = Rc::new(text);
    move |_, cx| {
        let sysmon = sysmon.clone();
        let text = text.clone();
        cx.new(|cx| {
            cx.observe(&sysmon, |_, _, cx| cx.notify()).detach();
            SysmonTooltip {
                sysmon: sysmon.clone(),
                text: text.clone(),
            }
        })
        .into()
    }
}

pub use crate::panel_kit::text_tooltip;

impl ShellBar {
    fn render_section(&self, section: Section, cx: &mut Context<Self>, tooltips_on: bool) -> Div {
        let widgets: Vec<WidgetConfig> = self.settings.read(cx).widgets(section).to_vec();
        div().flex().flex_row().items_center().gap_1().children(
            widgets
                .iter()
                .filter_map(|widget| self.render_widget(widget, cx, tooltips_on)),
        )
    }
}

impl ShellBar {
    fn render_widget(
        &self,
        widget: &WidgetConfig,
        cx: &mut Context<Self>,
        tooltips_on: bool,
    ) -> Option<AnyElement> {
        let niri = self.niri.read(cx);
        let focused_output = niri.focused_output().unwrap_or_default().to_owned();
        let sysmon = self.sysmon.read(cx);
        match widget.kind {
            WidgetKind::Workspaces => {
                let workspaces: Vec<WorkspaceItem> = niri
                    .workspaces_on(&focused_output)
                    .into_iter()
                    .map(|workspace| WorkspaceItem {
                        id: workspace.id,
                        label: workspace
                            .name
                            .clone()
                            .unwrap_or_else(|| workspace.idx.to_string()),
                        focused: workspace.is_focused,
                        active: workspace.is_active,
                        urgent: workspace.is_urgent,
                    })
                    .collect();
                Some(workspaces_widget(&workspaces, cx, tooltips_on).into_any_element())
            }
            WidgetKind::WindowTitle => {
                let title = niri
                    .focused_window()
                    .and_then(|window| window.title.clone())
                    .unwrap_or_else(|| "kuma".to_string());
                Some(window_title_widget(&title, tooltips_on).into_any_element())
            }
            WidgetKind::Apps => Some(
                div()
                    .id("widget-apps")
                    .flex()
                    .items_center()
                    .px_1()
                    .cursor_pointer()
                    .when(tooltips_on, |el| el.tooltip(text_tooltip("Apps".into())))
                    .on_click(cx.listener(|_, _, _, cx| {
                        crate::panel::toggle_panel(crate::panel::PanelKind::Launcher, cx)
                    }))
                    .child(
                        svg()
                            .path("icons/apps.svg")
                            .size(px(14.))
                            .text_color(rgb(TEXT)),
                    )
                    .into_any_element(),
            ),
            WidgetKind::Cpu => sysmon.cpu.map(|usage| {
                let percent = (usage * 100.0).round() as u32;
                sys_widget(
                    widget.kind,
                    widget.mode,
                    Some(format!("CPU {percent}%")),
                    TEXT,
                    move |sysmon: &SysMon, _| {
                        let percent = sysmon
                            .cpu
                            .map(|usage| (usage * 100.0).round() as u32)
                            .unwrap_or(0);
                        format!("CPU usage: {percent}%").into()
                    },
                    widget_icon(widget.kind),
                    tooltips_on,
                    self.sysmon.clone(),
                )
                .into_any_element()
            }),
            WidgetKind::Volume => sysmon.volume.map(|volume| {
                let muted = volume.muted;
                let text = if muted {
                    "MUTED".to_string()
                } else {
                    format!("{}%", volume.percent)
                };
                sys_widget(
                    widget.kind,
                    widget.mode,
                    Some(text),
                    if muted { URGENT } else { TEXT },
                    move |sysmon: &SysMon, _| match sysmon.volume {
                        Some(volume) if volume.muted => "Volume muted".into(),
                        Some(volume) => format!("Volume: {}%", volume.percent).into(),
                        None => "Volume unknown".into(),
                    },
                    widget_icon(widget.kind),
                    tooltips_on,
                    self.sysmon.clone(),
                )
                .cursor_pointer()
                .on_scroll_wheel(cx.listener(|this, event: &gpui::ScrollWheelEvent, _, cx| {
                    let scroll_y = match event.delta {
                        gpui::ScrollDelta::Pixels(delta) => f32::from(delta.y),
                        gpui::ScrollDelta::Lines(delta) => delta.y,
                    };
                    let delta = if scroll_y > 0. { 5 } else { -5 };
                    this.sysmon
                        .update(cx, |sysmon, cx| sysmon.request_volume(delta, cx));
                }))
                .on_click(cx.listener(|_, event: &gpui::ClickEvent, _, cx| {
                    // hang the mini panel under the pointer, i.e. under this widget
                    let anchor = f32::from(event.position().x);
                    crate::panel::toggle_panel_anchored(crate::panel::PanelKind::Volume, anchor, cx)
                }))
                .into_any_element()
            }),
            WidgetKind::Brightness => sysmon.brightness.map(|brightness| {
                sys_widget(
                    widget.kind,
                    widget.mode,
                    Some(format!("{}%", brightness.percent)),
                    TEXT,
                    move |sysmon: &SysMon, _| {
                        let percent = sysmon.brightness.map(|b| b.percent).unwrap_or(0);
                        format!("Brightness: {percent}%").into()
                    },
                    widget_icon(widget.kind),
                    tooltips_on,
                    self.sysmon.clone(),
                )
                .cursor_pointer()
                .on_scroll_wheel(cx.listener(|this, event: &gpui::ScrollWheelEvent, _, cx| {
                    let scroll_y = match event.delta {
                        gpui::ScrollDelta::Pixels(delta) => f32::from(delta.y),
                        gpui::ScrollDelta::Lines(delta) => delta.y,
                    };
                    let delta = if scroll_y > 0. { 5 } else { -5 };
                    this.sysmon
                        .update(cx, |sysmon, cx| sysmon.request_brightness(delta, cx));
                }))
                .on_click(cx.listener(|_, event: &gpui::ClickEvent, _, cx| {
                    let anchor = f32::from(event.position().x);
                    crate::panel::toggle_panel_anchored(
                        crate::panel::PanelKind::Brightness,
                        anchor,
                        cx,
                    )
                }))
                .into_any_element()
            }),
            WidgetKind::Mic => sysmon.mic.map(|mic| {
                let muted = mic.muted;
                let text = if muted {
                    "MUTED".to_string()
                } else {
                    format!("{}%", mic.percent)
                };
                sys_widget(
                    widget.kind,
                    widget.mode,
                    Some(text),
                    if muted { URGENT } else { TEXT },
                    move |sysmon: &SysMon, _| match sysmon.mic {
                        Some(mic) if mic.muted => "Microphone muted".into(),
                        Some(mic) => format!("Microphone: {}%", mic.percent).into(),
                        None => "Microphone unknown".into(),
                    },
                    widget_icon(widget.kind),
                    tooltips_on,
                    self.sysmon.clone(),
                )
                .cursor_pointer()
                .on_click(cx.listener(|_, event: &gpui::ClickEvent, _, cx| {
                    let anchor = f32::from(event.position().x);
                    crate::panel::toggle_panel_anchored(crate::panel::PanelKind::Mic, anchor, cx)
                }))
                .into_any_element()
            }),
            WidgetKind::PowerProfile => sysmon.power_profile.map(|profile| {
                let label = profile.title();
                sys_widget(
                    widget.kind,
                    widget.mode,
                    Some(label.to_string()),
                    if profile == crate::sysmon::PowerProfile::Performance {
                        URGENT
                    } else {
                        TEXT
                    },
                    move |sysmon: &SysMon, _| match sysmon.power_profile {
                        Some(profile) => {
                            format!("Power profile: {}", profile.title()).into()
                        }
                        None => "Power profile unknown".into(),
                    },
                    widget_icon(widget.kind),
                    tooltips_on,
                    self.sysmon.clone(),
                )
                .cursor_pointer()
                .on_click(cx.listener(|_, event: &gpui::ClickEvent, _, cx| {
                    // hang the popup under the pointer, i.e. under this widget
                    let anchor = f32::from(event.position().x);
                    crate::panel::toggle_panel_anchored(
                        crate::panel::PanelKind::PowerProfile,
                        anchor,
                        cx,
                    )
                }))
                .into_any_element()
            }),
            WidgetKind::Media => sysmon.media.clone().map(|media| {
                let text = if media.artist.is_empty() {
                    media.title.clone()
                } else {
                    format!("{} – {}", media.artist, media.title)
                };
                let color = match media.status {
                    Playback::Playing => ACCENT,
                    Playback::Paused | Playback::Stopped => TEXT_DIM,
                };
                sys_widget(
                    widget.kind,
                    widget.mode,
                    Some(text),
                    color,
                    move |sysmon: &SysMon, _| {
                        let Some(media) = &sysmon.media else {
                            return "No media".into();
                        };
                        let text = if media.artist.is_empty() {
                            media.title.clone()
                        } else {
                            format!("{} – {}", media.artist, media.title)
                        };
                        match media.status {
                            Playback::Playing => {
                                format!("Playing on {}: {text}", media.player).into()
                            }
                            Playback::Paused => {
                                format!("Paused on {}: {text}", media.player).into()
                            }
                            Playback::Stopped => format!("Stopped: {text}").into(),
                        }
                    },
                    widget_icon(widget.kind),
                    tooltips_on,
                    self.sysmon.clone(),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.sysmon
                        .update(cx, |sysmon, cx| sysmon.request_play_pause(cx));
                }))
                .on_scroll_wheel(
                    cx.listener(move |this, event: &gpui::ScrollWheelEvent, _, cx| {
                        let scroll_y = match event.delta {
                            gpui::ScrollDelta::Pixels(delta) => f32::from(delta.y),
                            gpui::ScrollDelta::Lines(delta) => delta.y,
                        };
                        this.sysmon.update(cx, |sysmon, cx| {
                            sysmon.request_skip(if scroll_y > 0. { 1 } else { -1 }, cx)
                        });
                    }),
                )
                .into_any_element()
            }),
            WidgetKind::Battery => sysmon.battery.map(|battery| {
                let low = battery.percent <= 15 && !battery.on_ac;
                let text = format!(
                    "{}{}%",
                    if battery.charging { "↑" } else { "" },
                    battery.percent
                );
                sys_widget(
                    widget.kind,
                    widget.mode,
                    Some(text),
                    if low { URGENT } else { TEXT },
                    move |sysmon: &SysMon, _| {
                        let status = sysmon.battery.map(|battery| {
                            if battery.percent >= 100 {
                                "full"
                            } else if battery.charging {
                                "charging"
                            } else if battery.on_ac {
                                "on AC (charge held)"
                            } else {
                                "discharging"
                            }
                        });
                        match (sysmon.battery, status) {
                            (Some(battery), Some(status)) => {
                                format!("Battery: {}% ({status})", battery.percent).into()
                            }
                            _ => "Battery unknown".into(),
                        }
                    },
                    Some(WidgetIcon::Data(battery_icon_svg(
                        battery.percent,
                        battery.charging,
                        battery.on_ac,
                    ))),
                    tooltips_on,
                    self.sysmon.clone(),
                )
                .into_any_element()
            }),
            WidgetKind::Clock => {
                let date = Local::now().format("%A, %B %e").to_string();
                Some(
                    sys_widget(
                        widget.kind,
                        widget.mode,
                        Some(self.clock.clone()),
                        TEXT,
                        move |_: &SysMon, _| date.clone().into(),
                        widget_icon(widget.kind),
                        tooltips_on,
                        self.sysmon.clone(),
                    )
                    .cursor_pointer()
                    .on_click(cx.listener(|_, _, _, cx| {
                        crate::panel::toggle_panel(crate::panel::PanelKind::Calendar, cx)
                    }))
                    .into_any_element(),
                )
            }
            WidgetKind::Bluetooth => sysmon.bluetooth.clone().map(|bt| {
                let (color, text) = if !bt.enabled {
                    (TEXT_DIM, None)
                } else if bt.devices.is_empty() {
                    (TEXT, None)
                } else {
                    (ACCENT, Some(bt.devices.len().to_string()))
                };
                sys_widget(
                    widget.kind,
                    widget.mode,
                    text,
                    color,
                    move |sysmon: &SysMon, _| {
                        let Some(bt) = &sysmon.bluetooth else {
                            return "Bluetooth: unknown".into();
                        };
                        if !bt.enabled {
                            "Bluetooth: off".into()
                        } else if bt.devices.is_empty() {
                            "Bluetooth: on, no devices connected".into()
                        } else {
                            format!("Bluetooth: {}", bt.devices.join(", ")).into()
                        }
                    },
                    widget_icon(widget.kind),
                    tooltips_on,
                    self.sysmon.clone(),
                )
                .into_any_element()
            }),
            WidgetKind::Internet => sysmon.network.clone().map(|net| {
                let text = if !net.online {
                    Some("offline".to_string())
                } else if net.wifi {
                    net.ssid.clone()
                } else {
                    None
                };
                sys_widget(
                    widget.kind,
                    widget.mode,
                    text,
                    if net.online { TEXT } else { URGENT },
                    move |sysmon: &SysMon, _| {
                        let Some(net) = &sysmon.network else {
                            return "Network: unknown".into();
                        };
                        if !net.online {
                            "Network: offline".into()
                        } else if net.wifi {
                            format!("Wi-Fi: {}", net.ssid.clone().unwrap_or_default()).into()
                        } else {
                            "Ethernet: connected".into()
                        }
                    },
                    widget_icon(widget.kind),
                    tooltips_on,
                    self.sysmon.clone(),
                )
                .into_any_element()
            }),
            WidgetKind::Notifications => {
                let state = self.notifications.read(cx);
                let unread = state.unread;
                let dnd = state.dnd;
                let text = if unread > 0 {
                    Some(unread.to_string())
                } else {
                    None
                };
                let color = if unread > 0 {
                    ACCENT
                } else if dnd {
                    TEXT_DIM
                } else {
                    TEXT
                };
                // live tooltip: re-reads the state each time it renders
                let notifications = self.notifications.clone();
                let tooltip = move |_: &SysMon, cx: &App| {
                    let state = notifications.read(cx);
                    if state.dnd {
                        format!(
                            "Notifications: do not disturb ({} in history)",
                            state.notifications.len()
                        )
                        .into()
                    } else {
                        format!(
                            "Notifications: {} in history, {} unseen",
                            state.notifications.len(),
                            state.unread
                        )
                        .into()
                    }
                };
                Some(
                    sys_widget(
                        widget.kind,
                        widget.mode,
                        text,
                        color,
                        tooltip,
                        widget_icon(widget.kind),
                        tooltips_on,
                        self.sysmon.clone(),
                    )
                    .cursor_pointer()
                    .on_click(cx.listener(|_, _, _, cx| {
                        crate::panel::toggle_panel(crate::panel::PanelKind::Notifications, cx)
                    }))
                    .into_any_element(),
                )
            }
            WidgetKind::Nostr => {
                // The signer's tell: the pending count beside the locked
                // shield, quiet when the queue is empty.
                let pending = self.nostr.read(cx).prompts.len();
                let text = (pending > 0).then(|| pending.to_string());
                let color = if pending > 0 { ACCENT } else { TEXT };
                // live tooltip: re-reads the state each time it renders
                let nostr = self.nostr.clone();
                let tooltip = move |_: &SysMon, cx: &App| {
                    let pending = nostr.read(cx).prompts.len();
                    format!("Nostr Signer: {pending} pending").into()
                };
                Some(
                    sys_widget(
                        widget.kind,
                        widget.mode,
                        text,
                        color,
                        tooltip,
                        widget_icon(widget.kind),
                        tooltips_on,
                        self.sysmon.clone(),
                    )
                    .cursor_pointer()
                    .on_click(cx.listener(|_, _, _, cx| {
                        crate::panel::toggle_panel(crate::panel::PanelKind::Nostr, cx)
                    }))
                    .into_any_element(),
                )
            }
            WidgetKind::Tray => {
                let items = self.tray.read(cx).items().to_vec();
                if items.is_empty() {
                    return None;
                }
                Some(
                    div()
                        .id("widget-tray")
                        .flex()
                        .items_center()
                        .gap_0p5()
                        .px_1()
                        .children(items.iter().map(|item| tray_icon(item, cx, tooltips_on)))
                        .into_any_element(),
                )
            }
        }
    }
}

/// One tray item: its icon, tooltip, and click-through to dbus Activate.
fn tray_icon(
    item: &crate::tray::TrayItem,
    cx: &mut Context<ShellBar>,
    tooltips_on: bool,
) -> gpui::Stateful<Div> {
    let service = item.service.clone();
    let service_secondary = item.service.clone();
    let tooltip = item.tooltip.clone().into();
    div()
        .id(gpui::SharedString::from(format!("tray-{}", item.service)))
        .flex()
        .items_center()
        .px_0p5()
        .cursor_pointer()
        .when(tooltips_on, |el| el.tooltip(text_tooltip(tooltip)))
        .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
            let x = f32::from(event.position().x) as i32;
            let y = f32::from(event.position().y) as i32;
            this.tray.read(cx).activate(&service, x, y);
        }))
        .on_aux_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
            // aux clicks cover middle and right; only right means SecondaryActivate
            if !matches!(
                event,
                gpui::ClickEvent::Mouse(event) if event.down.button == gpui::MouseButton::Right
            ) {
                return;
            }
            let x = f32::from(event.position().x) as i32;
            let y = f32::from(event.position().y) as i32;
            this.tray
                .read(cx)
                .secondary_activate(&service_secondary, x, y);
        }))
        .child(match &item.icon {
            Some(crate::imaging::IconImage::Raster(raster)) => {
                img(gpui::ImageSource::Render(raster.clone()))
                    .size(px(16.))
                    .into_any_element()
            }
            Some(crate::imaging::IconImage::Svg(bytes)) => {
                svg().data(bytes).size(px(16.)).into_any_element()
            }
            None => div()
                .size(px(6.))
                .rounded_full()
                .bg(rgb(TEXT_DIM))
                .into_any_element(),
        })
}

enum WidgetIcon {
    Path(&'static str),
    Data(std::sync::Arc<[u8]>),
}

/// Registry → renderable icon. `Generated` icons resolve at their arm (they
/// need the widget's snapshot data); asset paths resolve here, once.
fn widget_icon(kind: WidgetKind) -> Option<WidgetIcon> {
    match kind.spec().icon {
        None | Some(WidgetIconSpec::Generated) => None,
        Some(WidgetIconSpec::Path(path)) => Some(WidgetIcon::Path(path)),
    }
}

fn battery_icon_svg(percent: u8, charging: bool, on_ac: bool) -> std::sync::Arc<[u8]> {
    // level rendered in 5% steps inside the body (y 7..19); while charging the
    // body is full with a bolt cut out of it; on AC (held at threshold) it
    // also shows full
    let level = if charging || on_ac {
        12.
    } else {
        (percent.min(100) / 5) as f32 / 100. * 12.
    };
    let y = 19. - level;
    let fill = if charging {
        r#"<path fill-rule="evenodd" d="M8 7h8v12H8z M12.9 7.5 9.9 13.2h2L11 18.5l3.7-6.2h-2.1l2-4.8z"/>"#.to_string()
    } else {
        format!(r#"<rect x="8" y="{y}" width="8" height="{level}" rx="1"/>"#)
    };
    let svg = format!(
        r#"<svg viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg"><g fill="currentColor"><rect x="9.5" y="2" width="5" height="2.5" rx="1"/><path fill-rule="evenodd" d="M8 5h8a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2zm0 2h8v12H8z"/>{fill}</g></svg>"#
    );
    svg.into_bytes().into()
}

fn sys_widget(
    kind: WidgetKind,
    mode: WidgetMode,
    text: Option<String>,
    color: u32,
    tooltip: impl Fn(&SysMon, &App) -> SharedString + 'static,
    icon: Option<WidgetIcon>,
    tooltips_on: bool,
    sysmon: Entity<SysMon>,
) -> gpui::Stateful<Div> {
    div()
        .id(SharedString::from(format!("widget-{kind:?}")))
        .flex()
        .items_center()
        .gap_1()
        .px_1()
        .when(tooltips_on, |el| {
            el.tooltip(sysmon_tooltip(sysmon, tooltip))
        })
        .when(mode != WidgetMode::Text, |el| match icon {
            Some(icon) => {
                let icon_element = match icon {
                    WidgetIcon::Path(path) => svg().path(path).size(px(14.)),
                    WidgetIcon::Data(bytes) => svg().data(&bytes).size(px(14.)),
                };
                el.child(icon_element.text_color(rgb(color)))
            }
            None => el,
        })
        .when(mode != WidgetMode::Icon, |el| {
            el.child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(color))
                    .children(text),
            )
        })
}

fn workspaces_widget(
    items: &[WorkspaceItem],
    cx: &mut Context<ShellBar>,
    tooltips_on: bool,
) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .children(items.iter().map(|workspace| {
            let workspace_id = workspace.id;
            // outer box carries the bar-wide horizontal rhythm (px_1, same
            // as every sys_widget: edge and inter-widget gaps come out
            // uniform at 12px); the badge inside is the visual oval
            div()
                .id(workspace.id)
                .px_1()
                .on_click(cx.listener(move |_, _, _, cx| {
                    cx.background_spawn(async move {
                        if let Err(err) = crate::session::focus_workspace(workspace_id) {
                            error!("focus-workspace failed: {err:#}");
                        }
                    })
                    .detach();
                }))
                .when(tooltips_on, |el| {
                    el.tooltip(text_tooltip(
                        format!("Switch to workspace {}", workspace.label).into(),
                    ))
                })
                .child(
                    // the wide oval is only drawn on the focused workspace;
                    // the rest stay compact, sharing the bar's 12px rhythm
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .py_0p5()
                        .text_size(px(12.))
                        .when(workspace.focused, |el| {
                            el.min_w(px(34.))
                                .px_2()
                                .rounded_full()
                                .bg(rgb(ACCENT))
                                .text_color(rgb(ACCENT_TEXT))
                        })
                        .when(!workspace.focused, |el| {
                            el.px_1().text_color(if workspace.active {
                                rgb(TEXT)
                            } else {
                                rgb(TEXT_DIM)
                            })
                        })
                        .when(workspace.urgent, |el| el.text_color(rgb(URGENT)))
                        .child(workspace.label.clone()),
                )
        }))
}

fn window_title_widget(title: &str, tooltips_on: bool) -> gpui::Stateful<Div> {
    div()
        .id("widget-window-title")
        .min_w_0()
        .overflow_hidden()
        .text_size(px(12.))
        .text_color(rgb(TEXT))
        .when(tooltips_on, |el| el.tooltip(text_tooltip(title.into())))
        .truncate()
        .child(title.to_string())
}

fn recording_indicator(
    rec: &RecordingState,
    cx: &mut Context<ShellBar>,
    tooltips_on: bool,
) -> gpui::Stateful<Div> {
    let mins = rec.elapsed_secs / 60;
    let secs = rec.elapsed_secs % 60;
    div()
        .id("recording-indicator")
        .flex()
        .items_center()
        .gap_1()
        .px_1()
        .cursor_pointer()
        .when(tooltips_on, |el| {
            el.tooltip(text_tooltip("Screen recording: click to stop".into()))
        })
        .on_click(cx.listener(|_, _, _, cx| {
            cx.background_spawn(async move {
                if let Err(err) = std::process::Command::new("pkill")
                    .args(["-INT", "-x", "wf-recorder"])
                    .output()
                {
                    error!("stopping recorder failed: {err:#}");
                }
            })
            .detach();
        }))
        .child(div().size(px(8.)).rounded_full().bg(rgb(URGENT)))
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(URGENT))
                .child(format!("{mins}:{secs:02}")),
        )
}

fn gear_button(cx: &mut Context<ShellBar>, tooltips_on: bool) -> gpui::Stateful<Div> {
    div()
        .id("settings-gear")
        .px_1()
        .cursor_pointer()
        .when(tooltips_on, |el| {
            el.tooltip(text_tooltip("Settings".into()))
        })
        .on_click(cx.listener(|_, _, _, cx| {
            // the settings panel opens onto its quick page
            crate::panel::toggle_panel(crate::panel::PanelKind::Settings, cx)
        }))
        .child(
            svg()
                .path("icons/gear.svg")
                .size(px(14.))
                .text_color(rgb(TEXT)),
        )
}
