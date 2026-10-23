use std::rc::Rc;
use std::time::Duration;

use chrono::Local;
use gpui::{
    AnyElement, AnyView, App, Bounds, Context, Div, Entity, MouseMoveEvent, Pixels, Point, Render,
    SharedString, Window, div, img, point, prelude::*, px, rgb, rgba, size, svg,
};
use log::error;

use crate::calendar::clock_locale;
use crate::session::{Cast, CastTarget, SessionState};
use crate::settings::{
    BarAlign, BarPosition, BarRadius, CornerRounding, Section, Settings, WidgetConfig,
    WidgetIconSpec, WidgetKind, WidgetMode,
};
use crate::sysmon::{Playback, RecordingState, SysMon};
use crate::weather::WeatherState;

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
    weather: Entity<WeatherState>,
    widgets: Entity<crate::widgets::CustomWidgets>,
    updates: Entity<crate::updates::UpdatesState>,
    applied_geometry: Option<(f32, f32, f32, f32, f32, BarPosition)>,
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
        weather: Entity<WeatherState>,
        widgets: Entity<crate::widgets::CustomWidgets>,
        updates: Entity<crate::updates::UpdatesState>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&niri, |_, _, cx| cx.notify()).detach();
        cx.observe(&sysmon, |_, _, cx| cx.notify()).detach();
        cx.observe(&settings, |_, _, cx| cx.notify()).detach();
        cx.observe(&notifications, |_, _, cx| cx.notify()).detach();
        cx.observe(&tray, |_, _, cx| cx.notify()).detach();
        cx.observe(&nostr, |_, _, cx| cx.notify()).detach();
        cx.observe(&weather, |_, _, cx| cx.notify()).detach();
        cx.observe(&widgets, |_, _, cx| cx.notify()).detach();
        cx.observe(&updates, |_, _, cx| cx.notify()).detach();

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
            weather,
            widgets,
            updates,
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
        // only the bar's own section rides the render: the whole
        // settings document used to deep-clone here every frame (the
        // weather, dock, and idle configs included), and the sections
        // re-read the entity for their slices anyway
        let bar = self.settings.read(cx).bar.clone();

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
        // the surface stacks [offset][strip][tooltip room] for a top
        // bar and [tooltip room][strip][offset] for a bottom one: the
        // strip's offset inside the surface flips with the bar (a
        // bottom bar's tooltips flip above the cursor via gpui's own
        // bottom-overflow handling)
        let strip_top = match bar.position {
            BarPosition::Top => f32::from(bar.offset_top),
            BarPosition::Bottom => TOOLTIP_ROOM,
        };
        // content_x rides in the tuple: an align change moves the
        // content without touching height, width, or viewport, and
        // panels must hear about it or they keep centering on the old
        // alignment forever; position rides too, since it flips the
        // edge panels hang from
        let geometry = (
            f32::from(bar.height),
            f32::from(bar.offset_top),
            f32::from(content_width),
            f32::from(viewport.width),
            f32::from(content_x),
            bar.position,
        );
        let changed = match self.applied_geometry {
            None => true,
            Some(last) => {
                (last.0 - geometry.0).abs() > 0.5
                    || (last.1 - geometry.1).abs() > 0.5
                    || (last.2 - geometry.2).abs() > 0.5
                    || (last.3 - geometry.3).abs() > 0.5
                    || (last.4 - geometry.4).abs() > 0.5
                    || last.5 != geometry.5
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
                // panels center on the bar content's center line and sit
                // flush against the bar's inner face, measured from the
                // edge the bar hangs from
                cx.global_mut::<crate::panel::PanelHost>()
                    .report_bar_geometry(crate::panel::BarGeometry {
                        content_x: content_x.into(),
                        content_width: content_width.into(),
                        bar_edge: (f32::from(bar.offset_top) + f32::from(bar.height)).into(),
                        position: bar.position,
                    });
                // re-anchor any open panel to the fresh geometry: its
                // drawer positions at render from this value
                crate::panel::refresh_open_panels(cx);
            }
            window.set_exclusive_zone(px(bar.height + bar.offset_top));
            if content_width > px(0.) {
                window.set_input_region(Some(&[Bounds {
                    origin: point(content_x, px(strip_top)),
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
                .top(px(strip_top))
                .left(content_x)
                .w(content_width)
                .h(px(bar.height))
                .grid()
                .grid_cols(3)
                .items_center()
                .px_2()
                .bg(rgba(crate::theme::current().panel_bg)),
            bar.radius,
            bar.corners,
        )
        // the three sections clip at their track edge like the center
        // one does: one overflowing widget (a long title, a pile of
        // tray icons) costs itself some pixels, never the bar's layout
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .child(self.render_section(Section::Left, cx, tooltips_on)),
        )
        .child(
            div()
                .min_w_0()
                .flex()
                .justify_center()
                .gap_1p5()
                .overflow_hidden()
                .children(
                    bar.center
                        .iter()
                        .filter_map(|widget| self.render_widget(widget, cx, tooltips_on)),
                ),
        )
        .child(
            div()
                .min_w_0()
                .flex()
                .justify_end()
                .gap_1p5()
                .overflow_hidden()
                .child(self.render_section(Section::Right, cx, tooltips_on))
                .when_some(self.sysmon.read(cx).recording.clone(), |el, rec| {
                    el.child(recording_indicator(&rec, cx, tooltips_on))
                })
                .when_some(sharing_state(self, cx), |el, (casts, what)| {
                    el.child(sharing_indicator(&casts, &what, cx, tooltips_on))
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
            .text_color(rgb(crate::theme::current().text))
            // one line by design: these are short status labels, and a
            // wrapped tail ("held" alone on line two) reads broken
            .whitespace_nowrap()
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
    fn render_section(&self, section: Section, cx: &Context<Self>, tooltips_on: bool) -> Div {
        // the settings' own slice, borrowed: the widgets clone was a
        // per-render copy of a Vec that never changed between frames
        let settings = self.settings.read(cx);
        div().flex().flex_row().items_center().gap_1p5().children(
            settings
                .widgets(section)
                .iter()
                .filter_map(|widget| self.render_widget(widget, cx, tooltips_on)),
        )
    }
}

impl ShellBar {
    fn render_widget(
        &self,
        widget: &WidgetConfig,
        cx: &Context<Self>,
        tooltips_on: bool,
    ) -> Option<AnyElement> {
        let niri = self.niri.read(cx);
        let focused_output = niri.focused_output().unwrap_or_default().to_owned();
        let sysmon = self.sysmon.read(cx);
        // the built-ins' arms all read the kind: extract it once, and a
        // custom placement hands off to its own arm
        let kind: WidgetKind = match widget.kind.builtin() {
            Some(kind) => kind,
            None => return self.render_custom(widget, cx, tooltips_on),
        };
        match kind {
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
            WidgetKind::KeyboardLayout => {
                // the mirror's layout report: a session with no report,
                // or with a single layout, has nothing to indicate
                let layouts = niri
                    .keyboard_layouts()
                    .filter(|layouts| layouts.names.len() >= 2)?;
                let name = layouts
                    .names
                    .get(layouts.current_idx)
                    .cloned()
                    .unwrap_or_else(|| "layout".into());
                Some(
                    div()
                        .id("widget-keyboard-layout")
                        .flex()
                        .items_center()
                        .gap_1()
                        .cursor_pointer()
                        .on_click(cx.listener(|_, _, _, cx| {
                            // the click cycles: the compositor's own
                            // next is the native model for an ordered
                            // list, and the tooltip says so
                            cx.background_spawn(async move {
                                if let Err(err) = crate::session::switch_layout(
                                    crate::session::LayoutTarget::Next,
                                ) {
                                    error!("switch-layout failed: {err:#}");
                                }
                            })
                            .detach();
                        }))
                        .when(tooltips_on, |el| {
                            el.tooltip(text_tooltip(
                                format!("Keyboard layout: {name} (click to cycle)").into(),
                            ))
                        })
                        .when(widget.mode != WidgetMode::Text, |el| {
                            el.child(
                                svg()
                                    .path("icons/keyboard.svg")
                                    .size(px(16.))
                                    .text_color(rgb(crate::theme::current().text)),
                            )
                        })
                        .when(widget.mode != WidgetMode::Icon, |el| {
                            el.child(
                                div()
                                    .h(px(16.))
                                    .flex()
                                    .items_center()
                                    .text_size(px(12.))
                                    .text_color(rgb(crate::theme::current().text))
                                    .child(name),
                            )
                        })
                        .into_any_element(),
                )
            }
            WidgetKind::Apps => Some(
                div()
                    .id("widget-apps")
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .when(tooltips_on, |el| el.tooltip(text_tooltip("Apps".into())))
                    .on_click(cx.listener(|_, _, _, cx| {
                        crate::panel::toggle_panel(crate::panel::PanelKind::Launcher, cx)
                    }))
                    .child(
                        svg()
                            .path("icons/apps.svg")
                            .size(px(16.))
                            .text_color(rgb(crate::theme::current().text)),
                    )
                    .into_any_element(),
            ),
            WidgetKind::Cpu => sysmon.cpu.map(|usage| {
                let percent = (usage * 100.0).round() as u32;
                // the bar stays lean: the icon says CPU, the text only
                // has to say how much; detail lives in the panel the
                // click opens
                let text = match widget.mode {
                    // the clock's two faces are text-only dials and
                    // never arrive on a readout: the load clamps a
                    // stray placement to its kind's default, but the
                    // arms stay honest to the enum
                    WidgetMode::Text | WidgetMode::DateTime | WidgetMode::Time | WidgetMode::Date => {
                        Some(format!("CPU: {percent}%"))
                    }
                    WidgetMode::IconText => Some(format!("{percent}%")),
                    WidgetMode::Icon => None,
                };
                sys_widget(
                    kind,
                    widget.mode,
                    text,
                    crate::theme::current().text,
                    move |sysmon: &SysMon, _| {
                        let percent = sysmon
                            .cpu
                            .map(|usage| (usage * 100.0).round() as u32)
                            .unwrap_or(0);
                        let mut lines = vec![format!("CPU usage: {percent}%")];
                        if let Some(cores) = &sysmon.cpu_cores {
                            let per_core: Vec<String> = cores
                                .iter()
                                .map(|core| format!("{}%", (core * 100.0).round()))
                                .collect();
                            lines.push(format!("Per core: {}", per_core.join(" ")));
                        }
                        if let Some(temp) = sysmon.temp {
                            lines.push(format!("Temperature: {temp}°C"));
                        }
                        if let Some((loads, procs)) = sysmon.loadavg {
                            lines.push(format!(
                                "Load: {:.2} {:.2} {:.2} ({procs} processes)",
                                loads[0], loads[1], loads[2]
                            ));
                        }
                        if let Some(secs) = sysmon.uptime {
                            let days = secs / 86_400;
                            let hours = (secs % 86_400) / 3600;
                            let minutes = (secs % 3600) / 60;
                            lines.push(if days > 0 {
                                format!("Up: {days}d {hours}h")
                            } else if hours > 0 {
                                format!("Up: {hours}h {minutes}m")
                            } else {
                                format!("Up: {minutes}m")
                            });
                        }
                        lines.join("\n").into()
                    },
                    widget_icon(kind),
                    tooltips_on,
                    self.sysmon.clone(),
                    None,
                )
                .cursor_pointer()
                .on_click(cx.listener(|_, event: &gpui::ClickEvent, _, cx| {
                    let anchor = f32::from(event.position().x);
                    crate::panel::toggle_panel_anchored(crate::panel::PanelKind::Cpu, anchor, cx)
                }))
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
                    kind,
                    widget.mode,
                    Some(text),
                    if muted { crate::theme::URGENT } else { crate::theme::current().text },
                    move |sysmon: &SysMon, _| match sysmon.volume {
                        Some(volume) if volume.muted => "Volume muted".into(),
                        Some(volume) => format!("Volume: {}%", volume.percent).into(),
                        None => "Volume unknown".into(),
                    },
                    widget_icon(kind),
                    tooltips_on,
                    self.sysmon.clone(),
                    None,
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
                    kind,
                    widget.mode,
                    Some(format!("{}%", brightness.percent)),
                    crate::theme::current().text,
                    move |sysmon: &SysMon, _| {
                        match sysmon.brightness {
                            Some(brightness) => {
                                format!("Brightness: {}%", brightness.percent).into()
                            }
                            None => "Brightness unknown".into(),
                        }
                    },
                    widget_icon(kind),
                    tooltips_on,
                    self.sysmon.clone(),
                    None,
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
            WidgetKind::Ram => sysmon.ram.map(|ram| {
                let percent = ram.percent;
                let text = match widget.mode {
                    WidgetMode::Text | WidgetMode::DateTime | WidgetMode::Time | WidgetMode::Date => {
                        Some(format!("RAM: {percent}%"))
                    }
                    WidgetMode::IconText => Some(format!("{percent}%")),
                    WidgetMode::Icon => None,
                };
                sys_widget(
                    kind,
                    widget.mode,
                    text,
                    crate::theme::current().text,
                    move |sysmon: &SysMon, _| match sysmon.ram {
                        Some(ram) => format!(
                            "Memory: {:.1} GiB of {:.1} GiB",
                            ram.used_mib as f32 / 1024.,
                            ram.total_mib as f32 / 1024.
                        )
                        .into(),
                        None => "Memory unknown".into(),
                    },
                    widget_icon(kind),
                    tooltips_on,
                    self.sysmon.clone(),
                    None,
                )
                .cursor_pointer()
                .on_click(cx.listener(|_, event: &gpui::ClickEvent, _, cx| {
                    let anchor = f32::from(event.position().x);
                    crate::panel::toggle_panel_anchored(crate::panel::PanelKind::Ram, anchor, cx)
                }))
                .into_any_element()
            }),
            WidgetKind::Temp => sysmon.temp.map(|temp| {
                // hot enough to care: the alarm color says the fans
                // are losing
                let color = if temp >= 85 { crate::theme::URGENT } else { crate::theme::current().text };
                let text = match widget.mode {
                    WidgetMode::Text | WidgetMode::DateTime | WidgetMode::Time | WidgetMode::Date => {
                        Some(format!("TEMP: {temp}°C"))
                    }
                    WidgetMode::IconText => Some(format!("{temp}°C")),
                    WidgetMode::Icon => None,
                };
                sys_widget(
                    kind,
                    widget.mode,
                    text,
                    color,
                    move |sysmon: &SysMon, _| match sysmon.temp {
                        Some(temp) => format!("CPU temperature: {temp}°C").into(),
                        None => "CPU temperature unknown".into(),
                    },
                    widget_icon(kind),
                    tooltips_on,
                    self.sysmon.clone(),
                    None,
                )
                .cursor_pointer()
                .on_click(cx.listener(|_, event: &gpui::ClickEvent, _, cx| {
                    let anchor = f32::from(event.position().x);
                    crate::panel::toggle_panel_anchored(crate::panel::PanelKind::Temp, anchor, cx)
                }))
                .into_any_element()
            }),
            WidgetKind::Disk => sysmon.disk.clone().map(|disk| {
                let percent = disk.percent;
                let text = match widget.mode {
                    WidgetMode::Text | WidgetMode::DateTime | WidgetMode::Time | WidgetMode::Date => {
                        Some(format!("DISK: {percent}%"))
                    }
                    WidgetMode::IconText => Some(format!("{percent}%")),
                    WidgetMode::Icon => None,
                };
                sys_widget(
                    kind,
                    widget.mode,
                    text,
                    if percent >= 90 { crate::theme::URGENT } else { crate::theme::current().text },
                    move |sysmon: &SysMon, _| match sysmon.disk.clone() {
                        Some(disk) => format!(
                            "{}: {:.0} GiB of {:.0} GiB used",
                            disk.mount,
                            disk.used_mib as f32 / 1024.,
                            disk.total_mib as f32 / 1024.
                        )
                        .into(),
                        None => "Disk unknown".into(),
                    },
                    widget_icon(kind),
                    tooltips_on,
                    self.sysmon.clone(),
                    None,
                )
                .cursor_pointer()
                .on_click(cx.listener(|_, event: &gpui::ClickEvent, _, cx| {
                    let anchor = f32::from(event.position().x);
                    crate::panel::toggle_panel_anchored(crate::panel::PanelKind::Disk, anchor, cx)
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
                    kind,
                    widget.mode,
                    Some(text),
                    if muted { crate::theme::URGENT } else { crate::theme::current().text },
                    move |sysmon: &SysMon, _| match sysmon.mic {
                        Some(mic) if mic.muted => "Microphone muted".into(),
                        Some(mic) => format!("Microphone: {}%", mic.percent).into(),
                        None => "Microphone unknown".into(),
                    },
                    widget_icon(kind),
                    tooltips_on,
                    self.sysmon.clone(),
                    None,
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
                    kind,
                    widget.mode,
                    Some(label.to_string()),
                    if profile == crate::sysmon::PowerProfile::Performance {
                        crate::theme::URGENT
                    } else {
                        crate::theme::current().text
                    },
                    move |sysmon: &SysMon, _| match sysmon.power_profile {
                        Some(profile) => {
                            format!("Power profile: {}", profile.title()).into()
                        }
                        None => "Power profile unknown".into(),
                    },
                    widget_icon(kind),
                    tooltips_on,
                    self.sysmon.clone(),
                    None,
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
                    Playback::Playing => crate::theme::current().accent,
                    Playback::Paused | Playback::Stopped => crate::theme::current().text_dim,
                };
                sys_widget(
                    kind,
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
                    widget_icon(kind),
                    tooltips_on,
                    self.sysmon.clone(),
                    None,
                )
                // it plays/pauses on click: the cursor says so
                .cursor_pointer()
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
                // the kernel's capacity field is 0-100 by ABI, but a
                // nonstandard driver can report otherwise: the icon
                // clamps its level, the text clamps too, so neither can
                // render a nonsense "150%"
                let percent = battery.percent.min(100);
                let text = format!(
                    "{}{}%",
                    if battery.charging { "↑" } else { "" },
                    percent
                );
                sys_widget(
                    kind,
                    widget.mode,
                    Some(text),
                    if low { crate::theme::URGENT } else { crate::theme::current().text },
                    move |sysmon: &SysMon, _| {
                        let status = sysmon.battery.map(|battery| {
                            if battery.percent >= 100 {
                                "full"
                            } else if battery.charging {
                                "charging"
                            } else if battery.on_ac {
                                "on AC (held)"
                            } else {
                                "discharging"
                            }
                        });
                        match (sysmon.battery, status) {
                            (Some(battery), Some(status)) => {
                                format!("Battery: {}% ({status})", battery.percent.min(100)).into()
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
                    None,
                )
                .cursor_pointer()
                .on_click(cx.listener(|_, event: &gpui::ClickEvent, _, cx| {
                    let anchor = f32::from(event.position().x);
                    crate::panel::toggle_panel_anchored(
                        crate::panel::PanelKind::Battery,
                        anchor,
                        cx,
                    )
                }))
                .into_any_element()
            }),
            WidgetKind::Weather => {
                let state = self.weather.read(cx);
                let weather_cfg = &self.settings.read(cx).weather;
                let fahrenheit = weather_cfg.fahrenheit;
                let _location = weather_cfg
                    .resolved
                    .as_ref()
                    .map(|resolved| resolved.label.clone());
                let stale = state.stale();
                let color = if stale { crate::theme::current().text_dim } else { crate::theme::current().text };
                // no location configured reads as a hidden widget until
                // the settings page fills one in
                let current = state.current.as_ref()?;
                let text = match widget.mode {
                    WidgetMode::Icon => None,
                    WidgetMode::IconText => Some(
                        WeatherState::format_temp(fahrenheit, current.temp_c)
                            .trim_end_matches(['F', 'C'])
                            .to_string(),
                    ),
                    WidgetMode::Text | WidgetMode::DateTime | WidgetMode::Time | WidgetMode::Date => {
                        Some(WeatherState::format_temp(fahrenheit, current.temp_c))
                    }
                };
                let (_, condition_icon) = WeatherState::condition(current.code);
                let weather_entity = self.weather.clone();
                let settings_entity = self.settings.clone();
                Some(
                    sys_widget(
                        kind,
                        widget.mode,
                        text,
                        color,
                        move |_: &SysMon, cx: &gpui::App| {
                            let state = weather_entity.read(cx);
                            let weather_cfg = &settings_entity.read(cx).weather;
                            match (&state.current, &weather_cfg.resolved) {
                                (Some(current), Some(resolved)) => format!(
                                    "{}: {}{}",
                                    resolved.label,
                                    WeatherState::format_temp(
                                        weather_cfg.fahrenheit,
                                        current.temp_c
                                    ),
                                    if state.stale() { " (stale)" } else { "" }
                                )
                                .into(),
                                (_, None) => "Weather: no location set".into(),
                                (None, Some(_)) => "Weather: waiting for first read".into(),
                            }
                        },
                        Some(WidgetIcon::Path(condition_icon)),
                        tooltips_on,
                        self.sysmon.clone(),
                        None,
                    )
                    .cursor_pointer()
                    .on_click(cx.listener(|_, event: &gpui::ClickEvent, _, cx| {
                        let anchor = f32::from(event.position().x);
                        crate::panel::toggle_panel_anchored(
                            crate::panel::PanelKind::Weather,
                            anchor,
                            cx,
                        )
                    }))
                    .into_any_element(),
                )
            }
            WidgetKind::Clock => {
                let locale = clock_locale();
                let date = Local::now().format_localized("%A, %B %e", locale).to_string();
                // the dial is what the clock says: the fuller face
                // carries the compact day beside the time, the bare
                // face the time alone, the date face the day alone,
                // and none of them wears an icon. The date names
                // follow the system locale like the time follows the
                // system zone.
                let text = match widget.mode {
                    WidgetMode::Time => self.clock.clone(),
                    WidgetMode::Date => {
                        Local::now().format_localized("%a %b %e", locale).to_string()
                    }
                    _ => format!(
                        "{} · {}",
                        Local::now().format_localized("%a %b %e", locale),
                        self.clock.clone()
                    ),
                };
                Some(
                    sys_widget(
                        kind,
                        WidgetMode::Text,
                        Some(text),
                        crate::theme::current().text,
                        move |_: &SysMon, _| date.clone().into(),
                        widget_icon(kind),
                        tooltips_on,
                        self.sysmon.clone(),
                    None,
                    )
                    .cursor_pointer()
                    .on_click(cx.listener(|_, _, _, cx| {
                        crate::panel::toggle_panel(crate::panel::PanelKind::Calendar, cx)
                    }))
                    .into_any_element(),
                )
            }
            WidgetKind::Bluetooth => sysmon.bluetooth.clone().map(|bt| {
                let connected: Vec<&crate::sysmon::BluetoothDevice> =
                    bt.devices.iter().filter(|device| device.connected).collect();
                let (color, text) = if !bt.enabled {
                    (crate::theme::current().text_dim, None)
                } else if connected.is_empty() {
                    (crate::theme::current().text, None)
                } else {
                    (crate::theme::current().accent, Some(connected.len().to_string()))
                };
                sys_widget(
                    kind,
                    widget.mode,
                    text,
                    color,
                    move |sysmon: &SysMon, _| {
                        let Some(bt) = &sysmon.bluetooth else {
                            return "Bluetooth: unknown".into();
                        };
                        if !bt.enabled {
                            "Bluetooth: off".into()
                        } else {
                            let names: Vec<String> = bt
                                .devices
                                .iter()
                                .filter(|device| device.connected)
                                .map(|device| device.alias.clone())
                                .collect();
                            if names.is_empty() {
                                "Bluetooth: on, no devices connected".into()
                            } else {
                                format!("Bluetooth: {}", names.join(", ")).into()
                            }
                        }
                    },
                    widget_icon(kind),
                    tooltips_on,
                    self.sysmon.clone(),
                    None,
                )
                .cursor_pointer()
                .on_click(cx.listener(|_, event: &gpui::ClickEvent, _, cx| {
                    let anchor = f32::from(event.position().x);
                    crate::panel::toggle_panel_anchored(
                        crate::panel::PanelKind::Bluetooth,
                        anchor,
                        cx,
                    )
                }))
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
                    kind,
                    widget.mode,
                    text,
                    if net.online { crate::theme::current().text } else { crate::theme::URGENT },
                    move |sysmon: &SysMon, _| {
                        let Some(net) = &sysmon.network else {
                            return "Network: unknown".into();
                        };
                        if !net.online {
                            "Network: offline".into()
                        } else if net.wifi {
                            match net.ssid.as_deref() {
                                Some(ssid) if !ssid.is_empty() => format!("Wi-Fi: {ssid}").into(),
                                // a hidden network broadcasts no name:
                                // the tooltip says connected, not a blank
                                _ => "Wi-Fi: connected (hidden network)".into(),
                            }
                        } else {
                            "Ethernet: connected".into()
                        }
                    },
                    widget_icon(kind),
                    tooltips_on,
                    self.sysmon.clone(),
                    None,
                )
                .cursor_pointer()
                .on_click(cx.listener(|_, event: &gpui::ClickEvent, _, cx| {
                    let anchor = f32::from(event.position().x);
                    crate::panel::toggle_panel_anchored(
                        crate::panel::PanelKind::Wifi,
                        anchor,
                        cx,
                    )
                }))
                .into_any_element()
            }),
            WidgetKind::Vpn => {
                let net = sysmon.network.clone();
                // the existence gate: no VPN connections defined in
                // NetworkManager means this widget never renders, not
                // even as a dim chip
                let defined = net.as_ref().is_some_and(|net| !net.vpn_defined.is_empty());
                if !defined {
                    return None;
                }
                let active = net.as_ref().and_then(|net| net.vpn.clone());
                let lit = active.is_some();
                Some(
                    sys_widget(
                        kind,
                        widget.mode,
                        if lit { active } else { Some("off".into()) },
                        if lit { crate::theme::current().text } else { crate::theme::TEXT_DIM },
                        move |sysmon: &SysMon, _| match sysmon
                            .network
                            .as_ref()
                            .and_then(|net| net.vpn.clone())
                        {
                            Some(name) => format!("VPN: {name}").into(),
                            None => "VPN: off".into(),
                        },
                        widget_icon(kind),
                        tooltips_on,
                        self.sysmon.clone(),
                        None,
                    )
                    .cursor_pointer()
                    .on_click(cx.listener(|_, event: &gpui::ClickEvent, _, cx| {
                        let anchor = f32::from(event.position().x);
                        crate::panel::toggle_panel_anchored(
                            crate::panel::PanelKind::Wifi,
                            anchor,
                            cx,
                        )
                    }))
                    .into_any_element(),
                )
            }
            WidgetKind::Notifications => {
                let state = self.notifications.read(cx);
                let unread = state.unread;
                let dnd = state.dnd_effective();
                let text = if unread > 0 {
                    Some(unread.to_string())
                } else {
                    None
                };
                let color = if unread > 0 {
                    crate::theme::current().accent
                } else if dnd {
                    crate::theme::current().text_dim
                } else {
                    crate::theme::current().text
                };
                // live tooltip: re-reads the state each time it renders
                let notifications = self.notifications.clone();
                let tooltip = move |_: &SysMon, cx: &App| {
                    let state = notifications.read(cx);
                    let mode = if state.scheduled {
                        "do not disturb (quiet hours)"
                    } else if state.dnd {
                        "do not disturb"
                    } else {
                        "on"
                    };
                    if state.dnd_effective() {
                        format!(
                            "Notifications: {} ({} in history)",
                            mode,
                            state.notifications.len()
                        )
                        .into()
                    } else {
                        format!(
                            "Notifications: {} ({} in history, {} unseen)",
                            mode,
                            state.notifications.len(),
                            state.unread
                        )
                        .into()
                    }
                };
                Some(
                    sys_widget(
                        kind,
                        widget.mode,
                        text,
                        color,
                        tooltip,
                        widget_icon(kind),
                        tooltips_on,
                        self.sysmon.clone(),
                    None,
                    )
                    .cursor_pointer()
                    .on_click(cx.listener(|_, _, _, cx| {
                        crate::panel::toggle_panel(crate::panel::PanelKind::Notifications, cx)
                    }))
                    .into_any_element(),
                )
            }
            WidgetKind::Updates => {
                // the count beside the icon, gone entirely when the
                // base is current: a widget with nothing to say says
                // nothing. Security updates tint it the accent, the
                // same tell the bell carries.
                let state = self.updates.read(cx);
                // kumaOS machinery: without `kuma` there is nothing to
                // drive, and a widget with nothing to say says nothing
                if !state.available {
                    return None;
                }
                let count = state.updates.len();
                if count == 0 && !state.stale {
                    return None;
                }
                let text = (count > 0).then(|| count.to_string());
                // the accent means "there is work waiting for you"; once
                // the build has staged it, the waiting is the machine's
                let staged = matches!(state.applied, Some(crate::updates::ApplyOutcome::Staged));
                let color = if state.stale {
                    crate::theme::current().text_dim
                } else if state.security > 0 && !staged {
                    crate::theme::current().accent
                } else {
                    crate::theme::current().text
                };
                let updates = self.updates.clone();
                let tooltip = move |_: &SysMon, cx: &App| {
                    let state = updates.read(cx);
                    let staged =
                        matches!(state.applied, Some(crate::updates::ApplyOutcome::Staged));
                    match state.updates.len() {
                        0 if state.stale => "Kuma Updates: the last check failed".into(),
                        0 => "Kuma Updates: everything is current".into(),
                        n if staged => {
                            format!("Kuma Updates: {n} staged, restart to apply").into()
                        }
                        n => format!(
                            "Kuma Updates: {} available ({} security)",
                            n, state.security
                        )
                        .into(),
                    }
                };
                Some(
                    sys_widget(
                        kind,
                        widget.mode,
                        text,
                        color,
                        tooltip,
                        widget_icon(kind),
                        tooltips_on,
                        self.sysmon.clone(),
                        None,
                    )
                    .cursor_pointer()
                    .on_click(cx.listener(|_, _, _, cx| {
                        // centered like the launcher and settings: the
                        // anchored hang looked off, and the panel is a
                        // destination, not a glance
                        crate::panel::toggle_panel(crate::panel::PanelKind::Updates, cx)
                    }))
                    .into_any_element(),
                )
            }
            WidgetKind::Nostr => {
                // The signer's tell: the pending count beside the locked
                // shield, quiet when the queue is empty.
                let pending = self.nostr.read(cx).prompts.len();
                let text = (pending > 0).then(|| pending.to_string());
                let color = if pending > 0 { crate::theme::current().accent } else { crate::theme::current().text };
                // live tooltip: re-reads the state each time it renders
                let nostr = self.nostr.clone();
                let tooltip = move |_: &SysMon, cx: &App| {
                    let pending = nostr.read(cx).prompts.len();
                    format!("Nostr Signer: {pending} pending").into()
                };
                Some(
                    sys_widget(
                        kind,
                        widget.mode,
                        text,
                        color,
                        tooltip,
                        widget_icon(kind),
                        tooltips_on,
                        self.sysmon.clone(),
                    None,
                    )
                    .cursor_pointer()
                    .on_click(cx.listener(|_, _, _, cx| {
                        crate::panel::toggle_panel(crate::panel::PanelKind::Nostr, cx)
                    }))
                    .into_any_element(),
                )
            }
            WidgetKind::Tray => {
                // borrowed, not copied: the item list is stable between
                // polls and the render only reads it
                let items = self.tray.read(cx).items();
                if items.is_empty() {
                    return None;
                }
                Some(
                    div()
                        .id("widget-tray")
                        .flex()
                        .items_center()
                        .gap_0p5()
                        .children(items.iter().map(|item| tray_icon(item, cx, tooltips_on)))
                        .into_any_element(),
                )
            }
        }
    }

    /// A user-made widget's bar arm: the last read as the label, the
    /// manifest's icon beside it, the manifest's click line behind the
    /// whole thing. An uninstalled id renders nothing (the inert
    /// placement riding in the config); an empty read hides; a stale
    /// read dims and tells the truth in the tooltip. The mode pills
    /// apply as they do for built-ins, the manifest's icon being
    /// optional.
    fn render_custom(
        &self,
        widget: &WidgetConfig,
        cx: &Context<Self>,
        tooltips_on: bool,
    ) -> Option<AnyElement> {
        let id = widget.kind.custom()?.to_string();
        // clone the snapshot out: the read guard borrows cx, and the
        // element build below wants it
        let (manifest, read) = {
            let state = self.widgets.read(cx);
            let manifest = state.widgets().iter().find(|w| w.id == id)?.clone();
            (manifest, state.read(&id)?.clone())
        };
        let text = read.text.clone()?;
        let color = if read.stale {
            crate::theme::current().text_dim
        } else {
            crate::theme::current().text
        };
        let mut tooltip = read.tooltip.clone().unwrap_or(manifest.label.clone());
        if read.stale {
            tooltip = format!("{tooltip} (stale: the source failed)");
        }

        let icon = manifest.icon.as_ref().and_then(|icon| {
            let key = crate::widgets::icon_key(icon, &manifest.dir);
            // an svg rides the tint pipeline (the glyph pipeline the
            // built-in icons share); a raster paints full color
            crate::imaging::resolve_svg(&key)
                .or_else(|| crate::imaging::resolve(&key, 16))
        });

        let element = div()
            .id(SharedString::from(format!("widget-{}", widget.kind.key())))
            .flex()
            .items_center()
            .gap_1()
            .when(manifest.click.is_some(), |el| el.cursor_pointer())
            .when(tooltips_on, |el| {
                el.tooltip(text_tooltip(SharedString::from(tooltip)))
            })
            .when(widget.mode != WidgetMode::Text, |el| match &icon {
                Some(icon) => el.child(match &**icon {
                    crate::imaging::IconImage::Raster(raster) => img(
                        gpui::ImageSource::Render(raster.clone()),
                    )
                    .size(px(16.))
                    .into_any_element(),
                    crate::imaging::IconImage::Svg(bytes) => svg()
                        .data(bytes)
                        .size(px(16.))
                        // the sys widgets tint their icons with the
                        // widget color; an untinted svg inherits
                        // whatever ambient color is lying around
                        .text_color(rgb(color))
                        .into_any_element(),
                }),
                None => el,
            })
            .when(widget.mode != WidgetMode::Icon, |el| {
                // the same fixed 16px text box the sys widgets use:
                // custom and built-in text stand the same height in one
                // bar, and a script's long line truncates like theirs
                el.child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .h(px(16.))
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(color))
                                .truncate()
                                .child(text),
                        ),
                )
            })
            .when(manifest.click.is_some(), {
                let id = id.clone();
                move |el| {
                    el.on_click(cx.listener(move |this, _, _, cx| {
                        this.widgets.update(cx, |state, _| state.click(&id));
                    }))
                }
            });
        Some(element.into_any_element())
    }
}

/// One tray item: its icon, tooltip, and click-through to dbus Activate.
fn tray_icon(
    item: &crate::tray::TrayItem,
    cx: &Context<ShellBar>,
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
                .bg(rgb(crate::theme::current().text_dim))
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

/// The battery glyph cache: the level moves one point at a time and
/// the shape only changes at thresholds, so each (percent, charging,
/// on-ac) triple is rendered once and re-parsed never. The keys are
/// bounded (101 x 2 x 2); a full map is smaller than one frame's
/// string churn.
fn battery_icon_svg(percent: u8, charging: bool, on_ac: bool) -> std::sync::Arc<[u8]> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<(u8, bool, bool), std::sync::Arc<[u8]>>>,
    > = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut cache = cache.lock().expect("battery glyph cache");
    cache
        .entry((percent, charging, on_ac))
        .or_insert_with(|| render_battery_svg(percent, charging, on_ac))
        .clone()
}

/// Pure core of [`battery_icon_svg`]: the traditional narrow glyph
/// (body x 6..18, like the original): it is not a feather icon, it is
/// a battery, and the level readout inside is what carries the
/// meaning. Level fills the body interior (y 7..19) in steps; while
/// charging the interior is full with a bolt cut out of it; on AC
/// (held at threshold) it also shows full.
fn render_battery_svg(percent: u8, charging: bool, on_ac: bool) -> std::sync::Arc<[u8]> {
    let level = (percent.min(100) as f32) / 100. * 12.;
    let y = 19. - level;
    let fill = if charging || on_ac {
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
    extra: Option<gpui::Stateful<Div>>,
) -> gpui::Stateful<Div> {
    div()
        .id(SharedString::from(format!("widget-{kind:?}")))
        .flex()
        .items_center()
        .gap_1()
        .when(tooltips_on, |el| {
            el.tooltip(sysmon_tooltip(sysmon, tooltip))
        })
        .when(mode != WidgetMode::Text, |el| match icon {
            Some(icon) => {
                let icon_element = match icon {
                    WidgetIcon::Path(path) => svg().path(path).size(px(16.)),
                    WidgetIcon::Data(bytes) => svg().data(&bytes).size(px(16.)),
                };
                el.child(icon_element.text_color(rgb(color)))
            }
            None => el,
        })
        .when_some(extra, |el, extra| el.child(extra))
        .when(mode != WidgetMode::Icon, |el| {
            el.child(
                // a fixed 16px box with the text centered in it: the
                // widget's text stands the same height as its icons
                // and rides the vertical middle, not the baseline
                div()
                    // long values (a media title, a widget script's
                    // line, an ssid) ellipsize inside the section
                    // instead of shoving neighbors off the bar, exactly
                    // as the window title truncates
                    .min_w_0()
                    .overflow_hidden()
                    .h(px(16.))
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(color))
                            .truncate()
                            .children(text),
                    ),
            )
        })
}

fn workspaces_widget(
    items: &[WorkspaceItem],
    cx: &Context<ShellBar>,
    tooltips_on: bool,
) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .children(items.iter().map(|workspace| {
            let workspace_id = workspace.id;
            // spacing is the section's gap, nothing per-widget: one
            // source of truth, so inter-widget rhythm cannot drift
            div()
                .id(workspace.id)
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
                        // same 16px middle-aligned box every widget
                        // stands on
                        .h(px(16.))
                        .when(workspace.focused, |el| {
                            el.min_w(px(34.))
                                .px_2()
                                .rounded_full()
                                .bg(rgb(crate::theme::current().accent))
                                .text_color(rgb(crate::theme::current().accent_text))
                        })
                        .when(!workspace.focused, |el| {
                            el.px_1().text_color(if workspace.active {
                                rgb(crate::theme::current().text)
                            } else {
                                rgb(crate::theme::current().text_dim)
                            })
                        })
                        .when(workspace.urgent, |el| el.text_color(rgb(crate::theme::URGENT)))
                        .child(workspace.label.clone()),
                )
        }))
}

fn window_title_widget(title: &str, tooltips_on: bool) -> gpui::Stateful<Div> {
    div()
        .id("widget-window-title")
        .min_w_0()
        .overflow_hidden()
        // a 16px box with the text centered, like every widget's text
        .h(px(16.))
        .flex()
        .items_center()
        .text_size(px(12.))
        .text_color(rgb(crate::theme::current().text))
        .when(tooltips_on, |el| el.tooltip(text_tooltip(title.into())))
        .truncate()
        .child(title.to_string())
}

fn recording_indicator(
    rec: &RecordingState,
    cx: &Context<ShellBar>,
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
        .child(div().size(px(8.)).rounded_full().bg(rgb(crate::theme::URGENT)))
        .child(
            div()
                .h(px(16.))
                .flex()
                .items_center()
                .text_size(px(12.))
                .text_color(rgb(crate::theme::URGENT))
                .child(format!("{mins}:{secs:02}")),
        )
}

/// The sharing indicator's inputs, or None when nothing is streaming:
/// the casts plus a human phrase for what they point at (output name,
/// window title resolved against the session mirror).
fn sharing_state(bar: &ShellBar, cx: &App) -> Option<(Vec<Cast>, String)> {
    let niri = bar.niri.read(cx);
    let casts = niri.casts();
    if casts.is_empty() {
        return None;
    }
    let what = casts
        .iter()
        .map(|cast| match &cast.target {
            CastTarget::Output { name } => format!("output {name}"),
            CastTarget::Window { id } => niri
                .windows()
                .into_iter()
                .find(|window| window.id == *id)
                .and_then(|window| window.title.clone())
                .unwrap_or_else(|| format!("window {id}")),
            CastTarget::Nothing {} => "starting".to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some((casts, what))
}

/// The screen-sharing indicator: an amber dot beside the text of what
/// is being shared, click to stop. Distinct from the recording dot on
/// purpose: a wf-recorder and a browser share can run at once, and
/// their stops are different actions.
fn sharing_indicator(
    casts: &[Cast],
    what: &str,
    cx: &Context<ShellBar>,
    tooltips_on: bool,
) -> gpui::Stateful<Div> {
    // Dedup: one session can carry several streams, and a single stop
    // ends them all. Only PipeWire sessions accept a stop; a
    // wlr-screencopy consumer owns its stream, so the click sends what
    // niri accepts and no more.
    let mut sessions: Vec<u64> = casts
        .iter()
        .filter(|cast| cast.kind.stoppable())
        .map(|cast| cast.session_id)
        .collect();
    sessions.sort_unstable();
    sessions.dedup();
    // the listener is an Fn (a click can happen again), so the list
    // rides an Rc and each click clones it into its spawn
    let sessions = Rc::new(sessions);
    let paused = casts.iter().any(|cast| !cast.is_active);
    let tooltip = if paused {
        "Screen sharing (paused): click to stop"
    } else {
        "Screen sharing: click to stop"
    };
    div()
        .id("sharing-indicator")
        .flex()
        .items_center()
        .gap_1()
        .px_1()
        .cursor_pointer()
        .when(tooltips_on, |el| el.tooltip(text_tooltip(tooltip.into())))
        .on_click(cx.listener({
            let sessions = sessions.clone();
            move |_, _, _, cx| {
                let sessions = sessions.as_ref().clone();
                cx.background_spawn(async move {
                    for session_id in sessions {
                        if let Err(err) = crate::niri::stop_cast(session_id) {
                            error!("stopping screencast session {session_id} failed: {err:#}");
                        }
                    }
                })
                .detach();
            }
        }))
        .child(
            div()
                .size(px(8.))
                .rounded_full()
                .bg(rgb(crate::theme::SHARING)),
        )
        .child(
            div()
                .h(px(16.))
                .flex()
                .items_center()
                .text_size(px(12.))
                .text_color(rgb(crate::theme::SHARING))
                .child(what.to_string()),
        )
}

fn gear_button(cx: &Context<ShellBar>, tooltips_on: bool) -> gpui::Stateful<Div> {
    div()
        .id("settings-gear")
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
                .size(px(16.))
                .text_color(rgb(crate::theme::current().text)),
        )
}

#[cfg(test)]
mod tests {
    use crate::calendar::{locale_from_name, locale_tag};

    #[test]
    fn locale_tags_cut_encoding_and_modifier() {
        assert_eq!(locale_tag("de_DE.UTF-8"), "de_DE");
        assert_eq!(locale_tag("fr_FR@euro"), "fr_FR");
        assert_eq!(locale_tag("pt_BR.utf8"), "pt_BR");
        assert_eq!(locale_tag("C"), "C");
    }

    #[test]
    fn locale_names_resolve_and_unknown_falls_back_to_english() {
        assert_eq!(locale_from_name("de_DE"), chrono::Locale::de_DE);
        assert_eq!(locale_from_name("nonsense"), chrono::Locale::en_US);
        assert_eq!(locale_from_name("C"), chrono::Locale::en_US);
    }
}
