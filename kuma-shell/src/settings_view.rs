use std::{collections::HashMap, path::PathBuf, sync::Arc};

use gpui::{
    Context, Div, DragMoveEvent, Entity, FocusHandle, ObjectFit, Render, RenderImage, SharedString,
    Window, div, img, prelude::*, px, rgb, rgba,
};

use crate::panel::PanelGeometry;
use crate::panel_kit::{self as kit, ButtonVariant};
use crate::settings::{
    BarAlign, BarRadius, BarWidth, Corner, SECTIONS, Section, Settings, WidgetMode,
};
use crate::theme;
use crate::theme::*;

const HEIGHTS: [f32; 6] = [24., 28., 32., 36., 40., 48.];
const OFFSETS: [f32; 5] = [0., 4., 8., 12., 16.];
const WIDTHS: [BarWidth; 4] = [
    BarWidth::Full,
    BarWidth::ThreeQuarter,
    BarWidth::TwoThirds,
    BarWidth::Half,
];
const ALIGNS: [BarAlign; 3] = [BarAlign::Left, BarAlign::Center, BarAlign::Right];
const RADII: [BarRadius; 5] = [
    BarRadius::None,
    BarRadius::Sm,
    BarRadius::Md,
    BarRadius::Lg,
    BarRadius::Xl,
];
const IMAGE_EXTENSIONS: [&str; 5] = ["jpg", "jpeg", "png", "webp", "avif"];

async fn pick_folder() -> anyhow::Result<Option<PathBuf>> {
    let files = ashpd::desktop::file_chooser::SelectedFiles::open_file()
        .title("Choose a wallpapers folder")
        .accept_label("Select")
        .directory(true)
        .send()
        .await?
        .response()?;
    let Some(uri) = files.uris().first() else {
        return Ok(None);
    };
    Ok(Some(percent_decode_path(uri.as_str())))
}

fn percent_decode_path(uri: &str) -> PathBuf {
    let path = uri.strip_prefix("file://").unwrap_or(uri);
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(value) = u8::from_str_radix(
                &format!("{}{}", bytes[i + 1] as char, bytes[i + 2] as char),
                16,
            )
        {
            decoded.push(value);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    PathBuf::from(String::from_utf8_lossy(&decoded).into_owned())
}

fn load_thumb(path: PathBuf) -> Option<Arc<RenderImage>> {
    crate::imaging::decode_thumbnail(&path, 240, 160).map(Arc::new)
}

/// One continuous drawer silhouette: concave coves flaring out to the bar at
/// the top, straight sides, convex rounded bottom corners.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Quick,
    Bar,
    Widgets,
    Ordering,
    Dock,
    Backgrounds,
}

impl Page {
    fn label(self) -> &'static str {
        match self {
            Page::Quick => "Quick Settings",
            Page::Bar => "Bar",
            Page::Widgets => "Widgets",
            Page::Ordering => "Ordering",
            Page::Dock => "Dock",
            Page::Backgrounds => "Backgrounds",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Page::Quick => "icons/sliders.svg",
            Page::Bar => "icons/bar.svg",
            Page::Widgets => "icons/widgets.svg",
            Page::Ordering => "icons/order.svg",
            Page::Dock => "icons/dock.svg",
            Page::Backgrounds => "icons/image.svg",
        }
    }

    const ALL: [Page; 6] = [
        Page::Quick,
        Page::Bar,
        Page::Widgets,
        Page::Ordering,
        Page::Dock,
        Page::Backgrounds,
    ];
}

/// Where a widget drag would land: before the given index in a section.
type DropTarget = (Section, usize);

/// The drag payload between widget chips: which widget moves, and
/// where it came from.
#[derive(Clone)]
struct WidgetDrag {
    kind: crate::settings::WidgetKind,
    from: Section,
    index: usize,
}

/// What follows the cursor during a widget drag: the widget's name.
struct WidgetDragGhost {
    label: String,
}

impl Render for WidgetDragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(rgba(PANEL_BG))
            .border_1()
            .border_color(rgb(DIVIDER))
            .text_size(px(11.))
            .text_color(rgb(TEXT))
            .child(self.label.clone())
    }
}

/// Which slider the quick page is dragging, and its track stash.
struct DragState {
    which: Drag,
    track: crate::controls::TrackStash,
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    Brightness,
    Volume,
}

pub struct SettingsView {
    settings: Entity<Settings>,
    sysmon: Entity<crate::sysmon::SysMon>,
    focus_handle: FocusHandle,
    geometry: PanelGeometry,
    page: Page,
    dragging: Option<DragState>,
    /// where a widget drag would land right now, for the insertion line
    drop_preview: Option<DropTarget>,
    thumbs: HashMap<PathBuf, Option<Arc<RenderImage>>>,
}

impl SettingsView {
    pub fn new(
        settings: Entity<Settings>,
        sysmon: Entity<crate::sysmon::SysMon>,
        window: &mut Window,
        cx: &mut Context<Self>,
        geometry: PanelGeometry,
    ) -> Self {
        cx.observe(&settings, |_, _, cx| cx.notify()).detach();
        cx.observe(&sysmon, |_, _, cx| cx.notify()).detach();
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        Self {
            settings,
            sysmon,
            focus_handle,
            geometry,
            // the gear leads with the quick page
            page: Page::Quick,
            dragging: None,
            drop_preview: None,
            thumbs: HashMap::new(),
        }
    }

    fn segmented<T: Copy + PartialEq + std::fmt::Debug + 'static>(
        &self,
        id: &str,
        options: &[T],
        current: T,
        label_of: impl Fn(T) -> String,
        on_pick: impl Fn(&mut Settings, T, &mut gpui::Context<Settings>) + Clone + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        div()
            .id(SharedString::from(format!("seg-{id}")))
            .flex()
            .gap_0p5()
            .p_0p5()
            .rounded_md()
            .bg(rgb(INSET))
            .children(options.iter().map(|&option| {
                let active = option == current;
                let on_pick = on_pick.clone();
                div()
                    .id(SharedString::from(format!("seg-{id}-{option:?}")))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .text_size(px(11.))
                    .cursor_pointer()
                    .bg(rgb(if active { ACCENT } else { INSET }))
                    .text_color(rgb(if active { ACCENT_TEXT } else { TEXT_DIM }))
                    .hover(|style| {
                        style
                            .bg(rgb(if active { ACCENT } else { SURFACE }))
                            .text_color(rgb(if active { ACCENT_TEXT } else { TEXT }))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings
                            .update(cx, |settings, cx| on_pick(settings, option, cx));
                    }))
                    .child(label_of(option))
            }))
    }

    /// The quick page: sliders and toggles that ride the SysMon request
    /// seams, one click deep from the gear.
    fn quick_page(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let sysmon = self.sysmon.read(cx);
        let volume = sysmon.volume;
        let brightness = sysmon.brightness;
        let wifi = sysmon.network.as_ref().map(|network| network.wifi_enabled);
        let bluetooth = sysmon.bluetooth.as_ref().map(|bluetooth| bluetooth.enabled);
        let profile = sysmon.power_profile;
        let sysmon_handle = self.sysmon.clone();

        let brightness_row = brightness.map(|brightness| {
            let track = crate::controls::track_stash();
            crate::controls::slider_row(
                "icons/brightness.svg",
                TEXT,
                brightness.percent,
                crate::controls::slider_track(brightness.percent, track.clone()),
            )
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    this.dragging = Some(DragState {
                        which: Drag::Brightness,
                        track: track.clone(),
                    });
                    if let Some(percent) = track
                        .get()
                        .and_then(|bounds| crate::controls::value_at(event.position.x, bounds))
                    {
                        this.sysmon
                            .update(cx, |sysmon, cx| sysmon.request_set_brightness(percent, cx));
                    }
                }),
            )
        });

        let volume_row = volume.map(|volume| {
            let track = crate::controls::track_stash();
            let muted = volume.muted;
            crate::controls::slider_row(
                "icons/volume.svg",
                if muted { URGENT } else { TEXT },
                volume.percent,
                crate::controls::slider_track(volume.percent, track.clone()),
            )
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    this.dragging = Some(DragState {
                        which: Drag::Volume,
                        track: track.clone(),
                    });
                    if let Some(percent) = track
                        .get()
                        .and_then(|bounds| crate::controls::value_at(event.position.x, bounds))
                    {
                        this.sysmon
                            .update(cx, |sysmon, cx| sysmon.request_set_volume(percent, cx));
                    }
                }),
            )
            .child(if muted {
                kit::button(
                    "mute-toggle",
                    "Unmute",
                    Some("icons/volume.svg"),
                    ButtonVariant::Ghost,
                    cx.listener(|this, _, _, cx| {
                        this.sysmon
                            .update(cx, |sysmon, cx| sysmon.request_mute_toggle(cx));
                    }),
                )
            } else {
                kit::button(
                    "mute-toggle",
                    "Mute",
                    Some("icons/x.svg"),
                    ButtonVariant::Ghost,
                    cx.listener(|this, _, _, cx| {
                        this.sysmon
                            .update(cx, |sysmon, cx| sysmon.request_mute_toggle(cx));
                    }),
                )
            })
        });

        let profile_segmented = self.segmented(
            "power-profile",
            &crate::sysmon::PROFILES,
            profile.unwrap_or_default(),
            |profile| profile.as_str().to_string().replace('-', " "),
            {
                move |_, profile: crate::sysmon::PowerProfile, cx| {
                    sysmon_handle
                        .update(cx, |sysmon, cx| sysmon.request_power_profile(profile, cx));
                }
            },
            cx,
        );

        div()
            .id("quick-page")
            .flex()
            .flex_col()
            .gap_3()
            .overflow_y_scroll()
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                if let Some(drag) = &this.dragging
                    && let Some(percent) = drag
                        .track
                        .get()
                        .and_then(|bounds| crate::controls::value_at(event.position.x, bounds))
                {
                    let which = drag.which;
                    this.sysmon.update(cx, |sysmon, cx| match which {
                        Drag::Brightness => sysmon.request_set_brightness(percent, cx),
                        Drag::Volume => sysmon.request_set_volume(percent, cx),
                    });
                }
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.dragging = None;
                }),
            )
            .when(brightness.is_some() || volume.is_some(), |el| {
                el.child(
                    // the sliders live in one card so the quick page reads
                    // as a stack of cards, the toggles are cards of their
                    // own; skipped outright when there is nothing to slide
                    kit::card("quick-sliders")
                        .when_some(brightness_row, |el, row| el.child(row))
                        .when_some(volume_row, |el, row| el.child(row)),
                )
            })
            .when_some(wifi, |el, enabled| {
                el.child(crate::controls::toggle_row(
                    "toggle-wifi",
                    "icons/wifi.svg",
                    "Wi-Fi",
                    enabled,
                    cx.listener(|this, _, _, cx| {
                        this.sysmon
                            .update(cx, |sysmon, cx| sysmon.request_wifi_toggle(cx));
                    }),
                ))
            })
            .when_some(bluetooth, |el, enabled| {
                el.child(crate::controls::toggle_row(
                    "toggle-bluetooth",
                    "icons/bluetooth.svg",
                    "Bluetooth",
                    enabled,
                    cx.listener(|this, _, _, cx| {
                        this.sysmon
                            .update(cx, |sysmon, cx| sysmon.request_bluetooth_toggle(cx));
                    }),
                ))
            })
            .child({
                let dnd = self.settings.read(cx).notifications.dnd;
                let settings = self.settings.clone();
                crate::controls::toggle_row(
                    "toggle-dnd",
                    "icons/bell.svg",
                    "Do not disturb",
                    dnd,
                    move |_, _, cx| {
                        settings.update(cx, |settings, cx| {
                            let dnd = !settings.notifications.dnd;
                            settings.set_notifications_dnd(dnd, cx);
                        });
                    },
                )
            })
            .when_some(profile, |el, _| {
                el.child(kit::setting_row("Power profile", profile_segmented))
            })
    }

    fn bar_page(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let bar = self.settings.read(cx).bar.clone();
        div()
            .id("page-bar")
            .flex_1()
            .flex()
            .flex_col()
            .gap_3()
            .overflow_y_scroll()
            .child(kit::setting_row(
                "Height",
                self.segmented(
                    "height",
                    &HEIGHTS,
                    bar.height,
                    |v| format!("{v}"),
                    move |settings, value, cx| settings.set_height(value, cx),
                    cx,
                ),
            ))
            .child(kit::setting_row(
                "Top offset",
                self.segmented(
                    "offset",
                    &OFFSETS,
                    bar.offset_top,
                    |v| format!("{v}"),
                    move |settings, value, cx| settings.set_offset_top(value, cx),
                    cx,
                ),
            ))
            .child(kit::setting_row(
                "Width",
                self.segmented(
                    "width",
                    &WIDTHS,
                    bar.width,
                    |v| v.label().to_string(),
                    move |settings, value, cx| settings.set_width(value, cx),
                    cx,
                ),
            ))
            .when(bar.width != BarWidth::Full, |el| {
                el.child(kit::setting_row(
                    "Align",
                    self.segmented(
                        "align",
                        &ALIGNS,
                        bar.align,
                        |v| v.label().to_string(),
                        move |settings, value, cx| settings.set_align(value, cx),
                        cx,
                    ),
                ))
            })
            .child(kit::setting_row(
                "Corner radius",
                self.segmented(
                    "radius",
                    &RADII,
                    bar.radius,
                    |v| v.label().to_string(),
                    move |settings, value, cx| settings.set_radius(value, cx),
                    cx,
                ),
            ))
            .child(kit::setting_row("Rounded corners", self.corner_toggles(cx)))
    }

    fn corner_toggles(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let bar = self.settings.read(cx).bar.clone();
        let corners = [
            ("TL", Corner::TopLeft, bar.corners.top_left),
            ("TR", Corner::TopRight, bar.corners.top_right),
            ("BL", Corner::BottomLeft, bar.corners.bottom_left),
            ("BR", Corner::BottomRight, bar.corners.bottom_right),
        ];
        // the segmented control's idiom: an inset tray, the active
        // answer fills the accent
        div()
            .id("corner-toggles")
            .flex()
            .gap_0p5()
            .p_0p5()
            .rounded_md()
            .bg(rgb(INSET))
            .children(corners.iter().map(|&(label, corner, active)| {
                div()
                    .id(SharedString::from(format!(
                        "corner-{}",
                        format!("{corner:?}")
                    )))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .text_size(px(11.))
                    .cursor_pointer()
                    .bg(rgb(if active { ACCENT } else { INSET }))
                    .text_color(rgb(if active { ACCENT_TEXT } else { TEXT_DIM }))
                    .hover(|style| {
                        style
                            .bg(rgb(if active { ACCENT } else { SURFACE }))
                            .text_color(rgb(if active { ACCENT_TEXT } else { TEXT }))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings
                            .update(cx, |settings, cx| settings.toggle_corner(corner, cx));
                    }))
                    .child(label.to_string())
            }))
    }

    /// The dock page: enable/disable and the screen edge it hangs from.
    fn dock_page(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let settings = self.settings.read(cx);
        let enabled = settings.dock.enabled;
        let position = settings.dock.position;

        let positions = [
            crate::settings::DockPosition::Bottom,
            crate::settings::DockPosition::Top,
            crate::settings::DockPosition::Left,
            crate::settings::DockPosition::Right,
        ];
        let position_segmented = self.segmented(
            "dock-position",
            &positions,
            position,
            |position| position.label().to_string(),
            |settings, position: crate::settings::DockPosition, cx| {
                settings.set_dock_position(position, cx);
            },
            cx,
        );

        let settings_handle = self.settings.clone();
        div()
            .id("page-dock")
            .flex_1()
            .flex()
            .flex_col()
            .gap_3()
            .overflow_y_scroll()
            .child(crate::controls::toggle_row(
                "dock-enabled",
                "icons/dock.svg",
                "Show the app dock",
                enabled,
                move |_, _, cx| {
                    settings_handle.update(cx, |settings, cx| {
                        let enabled = !settings.dock.enabled;
                        settings.set_dock_enabled(enabled, cx);
                    });
                },
            ))
            .child(kit::setting_row("Position", position_segmented))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(TEXT_DIM))
                    .child("Right-click an app in the dock to pin or unpin it."),
            )
    }

    /// The widgets page: every registered kind, one row each in
    /// alphabetical order. The switch is the bar: on means it renders,
    /// off means it waits in reserve; the mode pills choose how an
    /// enabled one presents.
    fn widgets_page(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let mut kinds: Vec<crate::settings::WidgetKind> = crate::settings::WIDGETS
            .iter()
            .map(|spec| spec.kind)
            .collect();
        kinds.sort_by_key(|kind| kind.label().to_lowercase());
        div()
            .id("page-widgets")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_2()
            .overflow_y_scroll()
            .children(kinds.iter().map(|&kind| self.widget_row(kind, cx)))
    }

    fn widget_row(
        &self,
        kind: crate::settings::WidgetKind,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let position = self.settings.read(cx).position(kind);
        let enabled = position.is_some();
        let mode = position.map(|(section, index)| {
            (
                section,
                index,
                self.settings.read(cx).widgets(section)[index].mode,
            )
        });
        div()
            .id(SharedString::from(format!("widget-row-{kind:?}")))
            .flex()
            .items_center()
            .gap_2()
            .px_3p5()
            .py_2()
            .rounded_lg()
            .bg(rgb(SURFACE))
            .child(
                gpui::svg()
                    .path(match kind.icon_spec() {
                        Some(crate::settings::WidgetIconSpec::Path(path)) => path,
                        _ => "icons/puzzle.svg",
                    })
                    .size(px(16.))
                    .text_color(rgb(if enabled { TEXT } else { TEXT_DIM })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.))
                    .text_color(rgb(if enabled { TEXT } else { TEXT_DIM }))
                    .truncate()
                    .child(kind.label()),
            )
            .children(mode.map(|(section, index, mode)| {
                self.mode_segmented(&format!("mode-{kind:?}"), kind, mode, section, index, cx)
            }))
            .child(
                div()
                    .id(SharedString::from(format!("widget-toggle-{kind:?}")))
                    .p_1()
                    .cursor_pointer()
                    .child(crate::controls::toggle_switch(enabled))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings
                            .update(cx, |settings, cx| match settings.position(kind) {
                                Some((section, index)) => settings.remove(section, index, cx),
                                None => settings.add(kind, cx),
                            });
                    })),
            )
    }

    /// The ordering page: the bar mirrored as three columns of chips.
    /// Placement and order are drag and drop: a chip dropped on another
    /// takes its place (its lower half lands after it), a chip dropped
    /// on a column's empty stretch appends.
    fn ordering_page(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        div()
            .id("page-ordering")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_3()
            .overflow_y_scroll()
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| this.drop_preview = None),
            )
            .child(
                div().id("widget-columns").flex().gap_2().children(
                    SECTIONS
                        .iter()
                        .map(|&section| self.section_list(section, cx)),
                ),
            )
            .child(
                div().text_size(px(11.)).text_color(rgb(TEXT_DIM)).child(
                    "Drag between sections to place them; drop on a chip to slot it before.",
                ),
            )
    }

    fn section_header(&self, label: String) -> Div {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_size(px(10.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(rgb(TEXT_DIM))
                    .child(label.to_uppercase()),
            )
            .child(div().flex_1().h(px(1.)).bg(rgba(theme::SOFT_DIVIDER)))
    }

    /// The insertion indicator: the accent line a drag leaves between
    /// chips, saying where the widget will land.
    fn drop_line(&self) -> Div {
        div().w_full().h(px(2.)).rounded_full().bg(rgb(ACCENT))
    }

    fn section_list(&self, section: Section, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let settings = self.settings.read(cx).clone();
        let count = settings.widgets(section).len();
        let dragging = cx.has_active_drag();
        let preview = self.drop_preview;
        div()
            .id(SharedString::from(format!("section-{section:?}")))
            .flex()
            .flex_col()
            .gap_1p5()
            .flex_1()
            .min_w_0()
            .min_h(px(64.))
            .child(self.section_header(format!("{section:?}")))
            .children(
                settings
                    .widgets(section)
                    .iter()
                    .enumerate()
                    .flat_map(|(index, widget)| {
                        let mut items: Vec<gpui::AnyElement> = Vec::new();
                        if dragging && preview == Some((section, index)) {
                            items.push(self.drop_line().into_any_element());
                        }
                        items.push(
                            self.widget_chip(section, index, widget.kind, cx)
                                .into_any_element(),
                        );
                        items
                    }),
            )
            .when(dragging && preview == Some((section, count)), |el| {
                el.child(self.drop_line())
            })
            .when(settings.widgets(section).is_empty(), |el| {
                el.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(TEXT_DIM))
                        .child("no widgets: drop one here"),
                )
            })
            // the column's own stretch appends at the end; its capture
            // listener fires before the chips', so a chip overwrites it
            .on_drag_move(
                cx.listener(move |this, _: &DragMoveEvent<WidgetDrag>, _, cx| {
                    this.drop_preview = Some((section, count));
                    cx.notify();
                }),
            )
            .on_drop(cx.listener(move |this, drag: &WidgetDrag, _, cx| {
                this.settings.update(cx, |settings, cx| {
                    settings.move_widget(drag.from, drag.index, section, count, cx);
                });
                this.drop_preview = None;
            }))
    }

    fn widget_chip(
        &self,
        section: Section,
        index: usize,
        kind: crate::settings::WidgetKind,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let tag = |name: &str| format!("{name}-{section:?}-{index}");
        kit::card(tag("chip"))
            .px_2()
            .py_1p5()
            .cursor_pointer()
            .child(
                div()
                    .text_size(px(11.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(TEXT))
                    .truncate()
                    .child(kind.label()),
            )
            .on_drag(
                WidgetDrag {
                    kind,
                    from: section,
                    index,
                },
                |drag, _, _, cx| {
                    cx.new(|_| WidgetDragGhost {
                        label: drag.kind.label().to_string(),
                    })
                },
            )
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<WidgetDrag>, _, cx| {
                    // the chip's lower half means "land after it"
                    let midline = event.bounds.origin.y + event.bounds.size.height / 2.;
                    let after = event.event.position.y > midline;
                    this.drop_preview = Some((section, index + usize::from(after)));
                    cx.notify();
                }),
            )
            // a drop on a chip applies the standing preview: this chip's
            // place, or the line under its lower half
            .on_drop(cx.listener(move |this, drag: &WidgetDrag, _, cx| {
                if let Some(to) = this.drop_preview.take() {
                    this.settings.update(cx, |settings, cx| {
                        settings.move_widget(drag.from, drag.index, to.0, to.1, cx);
                    });
                }
            }))
    }

    /// The mode control: three glyph buttons, the active mode lit.
    /// Icons instead of the text pills: a column chip has no width to
    /// spare, and the glyphs read at a glance. Hover names them.
    /// The mode control: three pills that preview the answer instead
    /// of naming it. Icon shows the widget's own glyph, icon+text adds
    /// "Aa" beside it, text is "Aa" alone; what you click is what the
    /// widget renders.
    fn mode_segmented(
        &self,
        id: &str,
        kind: crate::settings::WidgetKind,
        current: WidgetMode,
        section: Section,
        index: usize,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let icon_path = match kind.icon_spec() {
            Some(crate::settings::WidgetIconSpec::Path(path)) => path,
            _ => "icons/puzzle.svg",
        };
        div()
            .id(SharedString::from(format!("mode-{id}")))
            .flex()
            .gap_0p5()
            .p_0p5()
            .rounded_sm()
            .bg(rgb(INSET))
            .children(
                [WidgetMode::Icon, WidgetMode::IconText, WidgetMode::Text].map(|mode| {
                    let active = mode == current;
                    div()
                        .id(SharedString::from(format!("mode-{id}-{mode:?}")))
                        .flex()
                        .items_center()
                        .gap_1()
                        .px_1p5()
                        .py_0p5()
                        .rounded_sm()
                        .cursor_pointer()
                        .bg(rgb(if active { ACCENT } else { INSET }))
                        .hover(|style| style.bg(rgb(if active { ACCENT } else { SURFACE })))
                        .tooltip(kit::text_tooltip(mode.label().into()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.settings.update(cx, |settings, cx| {
                                settings.set_mode(section, index, mode, cx)
                            });
                        }))
                        .when(mode != WidgetMode::Text, |el| {
                            el.child(
                                gpui::svg()
                                    .path(icon_path)
                                    .size(px(12.))
                                    .text_color(rgb(if active { ACCENT_TEXT } else { TEXT_DIM })),
                            )
                        })
                        .when(mode != WidgetMode::Icon, |el| {
                            el.child(
                                div()
                                    .text_size(px(10.))
                                    .line_height(px(12.))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(rgb(if active { ACCENT_TEXT } else { TEXT_DIM }))
                                    .child("Aa"),
                            )
                        })
                }),
            )
    }

    fn backgrounds_page(&mut self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let background = self.settings.read(cx).background.clone();
        let mut entries = vec![("default".to_string(), crate::settings::default_wallpaper())];
        if let Ok(folder) = std::fs::read_dir(&background.folder) {
            let mut files: Vec<(String, PathBuf)> = folder
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| {
                    path.is_file()
                        && path.extension().is_some_and(|ext| {
                            IMAGE_EXTENSIONS
                                .contains(&ext.to_string_lossy().to_lowercase().as_str())
                        })
                })
                .filter_map(|path| {
                    let name = path.file_name()?.to_string_lossy().to_string();
                    Some((name, path))
                })
                .collect();
            files.sort();
            entries.extend(files);
        }

        for (_, path) in entries.clone() {
            self.ensure_thumb(path, cx);
        }

        div()
            .id("page-backgrounds")
            .flex_1()
            .flex()
            .flex_col()
            .gap_3()
            .overflow_y_scroll()
            .child(
                kit::card("backgrounds-folder").child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(11.))
                                .text_color(rgb(TEXT_DIM))
                                .truncate()
                                .child(format!("Folder: {}", background.folder.display())),
                        )
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .child(kit::button(
                                    "pick-folder",
                                    "Choose folder",
                                    Some("icons/image.svg"),
                                    ButtonVariant::Primary,
                                    cx.listener(|this, _, _, cx| {
                                        let settings = this.settings.clone();
                                        cx.spawn(async move |this, cx| {
                                            let picked = cx
                                                .background_spawn(
                                                    async move { pick_folder().await },
                                                )
                                                .await;
                                            match picked {
                                                Ok(Some(folder)) => {
                                                    settings.update(cx, |settings, cx| {
                                                        settings.set_background_folder(folder, cx)
                                                    });
                                                    this.update(cx, |this, cx| {
                                                        this.thumbs.clear();
                                                        cx.notify();
                                                    })
                                                    .ok();
                                                }
                                                Ok(None) => {}
                                                Err(err) => {
                                                    log::error!("folder picker failed: {err:#}")
                                                }
                                            }
                                        })
                                        .detach();
                                    }),
                                ))
                                .child(kit::button(
                                    "reload-config",
                                    "Reload config",
                                    Some("icons/refresh.svg"),
                                    ButtonVariant::Ghost,
                                    cx.listener(|this, _, _, cx| {
                                        this.thumbs.clear();
                                        this.settings
                                            .update(cx, |settings, cx| settings.reload(cx));
                                    }),
                                )),
                        ),
                ),
            )
            .child(
                div().id("gallery").grid().grid_cols(3).gap_2().children(
                    entries
                        .into_iter()
                        .map(|(name, path)| self.background_tile(name, path, cx)),
                ),
            )
    }

    fn ensure_thumb(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.thumbs.contains_key(&path) {
            return;
        }
        self.thumbs.insert(path.clone(), None);
        cx.spawn(async move |this, cx| {
            let path_for_task = path.clone();
            let thumb = cx
                .background_spawn(async move { load_thumb(path_for_task) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.thumbs.insert(path, thumb);
                cx.notify();
            });
        })
        .detach();
    }

    fn background_tile(
        &self,
        name: String,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let background = self.settings.read(cx).background.clone();
        let selected = background.current == name;
        let thumb = self.thumbs.get(&path).cloned().flatten();
        let has_thumb = thumb.is_some();
        let is_default = name == "default";
        let label = if is_default {
            "System".to_string()
        } else {
            name.trim_end_matches(".jpg")
                .trim_end_matches(".png")
                .to_string()
        };
        div()
            .id(SharedString::from(format!("bg-{name}")))
            .flex()
            .flex_col()
            .gap_0p5()
            .child(
                div()
                    .id(SharedString::from(format!("bg-img-{name}")))
                    .h(px(88.))
                    .w_full()
                    .rounded_sm()
                    .overflow_hidden()
                    .border_2()
                    .border_color(rgb(if selected { ACCENT } else { DIVIDER }))
                    .when_some(thumb, |el, thumb| {
                        el.child(
                            img(gpui::ImageSource::Render(thumb))
                                .object_fit(ObjectFit::Cover)
                                .size_full(),
                        )
                    })
                    .when(!has_thumb, |el| el.bg(rgb(INSET)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let name = name.clone();
                        this.settings
                            .update(cx, |settings, cx| settings.set_background(name, cx));
                    }))
                    .cursor_pointer(),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(if selected { TEXT } else { TEXT_DIM }))
                    .truncate()
                    .child(label),
            )
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // the pane header names the page and rides above the pane body
        // alone, never above the sidebar: the nostr panel's arrangement,
        // kit::tabbed_pane's default for every panel with tabs. The
        // sidebar holds the identity (Settings), the pane says where
        // in it you are
        let header = kit::pane_header(self.page.label());

        let sidebar = div()
            .id("sidebar")
            .w(px(48.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_2()
            .children(Page::ALL.iter().map(|&page| {
                kit::rail_tab(
                    format!("nav-{page:?}"),
                    page.icon(),
                    page.label(),
                    self.page == page,
                    None,
                    cx.listener(move |this, _, _, cx| {
                        this.page = page;
                        cx.notify();
                    }),
                )
            }));

        let content = div()
            .id("content")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .when(self.page == Page::Quick, |el| el.child(self.quick_page(cx)))
            .when(self.page == Page::Bar, |el| el.child(self.bar_page(cx)))
            .when(self.page == Page::Widgets, |el| {
                el.child(self.widgets_page(cx))
            })
            .when(self.page == Page::Ordering, |el| {
                el.child(self.ordering_page(cx))
            })
            .when(self.page == Page::Dock, |el| el.child(self.dock_page(cx)))
            .when(self.page == Page::Backgrounds, |el| {
                el.child(self.backgrounds_page(cx))
            });

        crate::panel::chrome(
            self.geometry,
            window,
            div()
                .id("settings-panel")
                .size_full()
                .flex()
                .flex_row()
                .gap_3()
                .px(px(12.))
                .pt(px(10.))
                .pb(px(12.))
                .track_focus(&self.focus_handle)
                .child(sidebar)
                .child(div().w(px(1.)).bg(rgba(theme::SOFT_DIVIDER)))
                .child(kit::tabbed_pane(header, content)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_handles_escapes() {
        assert_eq!(
            percent_decode_path("file:///home/m/My%20Wallpapers"),
            PathBuf::from("/home/m/My Wallpapers")
        );
        assert_eq!(
            percent_decode_path("/plain/path"),
            PathBuf::from("/plain/path")
        );
        assert_eq!(
            percent_decode_path("file:///truncated%2"),
            PathBuf::from("/truncated%2")
        );
    }
}
