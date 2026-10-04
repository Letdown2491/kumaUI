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

/// The rotation choices the backgrounds page offers: off, or minutes.
const ROTATIONS: [u32; 4] = [0, 10, 30, 60];

/// Spawn one power command: the idle.rs pattern, fire and forget.
fn power_action(
    command: &'static str,
    args: &'static [&'static str],
) -> impl Fn(&mut gpui::App) + 'static {
    move |cx: &mut gpui::App| {
        cx.background_spawn(async move {
            let _ = std::process::Command::new(command).args(args).output();
        })
        .detach();
    }
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
    Idle,
    Backgrounds,
    Weather,
    NightLight,
}

impl Page {
    fn label(self) -> &'static str {
        match self {
            Page::Quick => "Quick Settings",
            Page::Bar => "Bar",
            Page::Widgets => "Widgets",
            Page::Ordering => "Ordering",
            Page::Dock => "Dock",
            Page::Idle => "Idle",
            Page::Backgrounds => "Backgrounds",
            Page::Weather => "Weather",
            Page::NightLight => "Night Light",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Page::Quick => "icons/sliders.svg",
            Page::Bar => "icons/bar.svg",
            Page::Widgets => "icons/widgets.svg",
            Page::Ordering => "icons/order.svg",
            Page::Dock => "icons/dock.svg",
            Page::Idle => "icons/clock.svg",
            Page::Backgrounds => "icons/image.svg",
            Page::Weather => "icons/cloud.svg",
            Page::NightLight => "icons/moon.svg",
        }
    }

    const ALL: [Page; 9] = [
        Page::Quick,
        Page::Bar,
        Page::Widgets,
        Page::Ordering,
        Page::Dock,
        Page::Idle,
        Page::Backgrounds,
        Page::Weather,
        Page::NightLight,
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
            .bg(rgba(crate::theme::current().panel_bg))
            .border_1()
            .border_color(rgb(crate::theme::current().divider))
            .text_size(px(11.))
            .text_color(rgb(crate::theme::current().text))
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
    Mic,
}

/// Which field a numeric edit is driving: the idle page's two clocks
/// and the quick page's quiet-hours ends.
#[derive(Clone, Copy, PartialEq)]
enum EditField {
    Lock,
    ScreenOff,
    QuietFrom,
    QuietTo,
}

/// The open weather location field: the typed text and its focus.
/// Enter commits the query and starts the resolve.
struct WeatherEdit {
    text: String,
    focus: FocusHandle,
}

/// A location resolve's state: working, or why it failed last try.
enum WeatherResolve {
    Working,
    Failed(String),
}

/// An open idle edit: the digits typed so far and the field's focus.
/// Nothing is written until Enter commits the whole number.
struct NumEdit {
    which: EditField,
    text: String,
    focus: FocusHandle,
}

/// Which half of the window a dropdown edits.
#[derive(Clone, Copy, Debug, PartialEq)]
enum NightField {
    Start,
    End,
}

/// Which of the three dropdowns in a half.
#[derive(Clone, Copy, Debug, PartialEq)]
enum NightPart {
    Hour,
    Minute,
    Meridiem,
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
    /// the power row, minted once: the armed state lives in the row's
    /// own view, and a view recreated per render would forget its arm
    /// between SysMon's polls
    power_row: Entity<kit::ConfirmActions>,
    /// the idle field being edited, if any
    num_edit: Option<NumEdit>,
    /// the weather location field, if open, and the resolve's state
    weather_edit: Option<WeatherEdit>,
    weather_resolve: Option<WeatherResolve>,
    /// the open night light dropdown, if any
    night_menu: Option<(NightField, NightPart)>,
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
        let power_row = kit::confirm_actions(
            vec![
                kit::ConfirmAction::new(
                    "power-logout",
                    "Log out",
                    "Log out, end the session?",
                    Some("icons/logout.svg"),
                    power_action("niri", &["msg", "action", "quit"]),
                ),
                kit::ConfirmAction::new(
                    "power-reboot",
                    "Reboot",
                    "Reboot, confirm?",
                    Some("icons/power.svg"),
                    power_action("systemctl", &["reboot"]),
                ),
                kit::ConfirmAction::new(
                    "power-poweroff",
                    "Power off",
                    "Power off, confirm?",
                    Some("icons/power.svg"),
                    power_action("systemctl", &["poweroff"]),
                ),
            ],
            cx,
        );
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
            power_row,
            num_edit: None,
            weather_edit: None,
            weather_resolve: None,
            night_menu: None,
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
            .bg(rgb(crate::theme::current().inset))
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
                    .bg(rgb(if active { crate::theme::current().accent } else { crate::theme::current().inset }))
                    .text_color(rgb(if active { crate::theme::current().accent_text } else { crate::theme::current().text_dim }))
                    .hover(|style| {
                        style
                            .bg(rgb(if active { crate::theme::current().accent } else { crate::theme::current().surface }))
                            .text_color(rgb(if active { crate::theme::current().accent_text } else { crate::theme::current().text }))
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
    fn quick_page(&mut self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let sysmon = self.sysmon.read(cx);
        let volume = sysmon.volume;
        let mic = sysmon.mic;
        let brightness = sysmon.brightness;
        let wifi = sysmon.network.as_ref().map(|network| network.wifi_enabled);
        let bluetooth = sysmon.bluetooth.as_ref().map(|bluetooth| bluetooth.enabled);
        let profile = sysmon.power_profile;
        let sysmon_handle = self.sysmon.clone();

        let brightness_row = brightness.map(|brightness| {
            let track = crate::controls::track_stash();
            crate::controls::slider_row(
                "icons/brightness.svg",
                crate::theme::current().text,
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
                if muted { crate::theme::URGENT } else { crate::theme::current().text },
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

        let mic_row = mic.map(|mic| {
            let track = crate::controls::track_stash();
            let muted = mic.muted;
            crate::controls::slider_row(
                "icons/mic.svg",
                if muted { crate::theme::URGENT } else { crate::theme::current().text },
                mic.percent,
                crate::controls::slider_track(mic.percent, track.clone()),
            )
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    this.dragging = Some(DragState {
                        which: Drag::Mic,
                        track: track.clone(),
                    });
                    if let Some(percent) = track
                        .get()
                        .and_then(|bounds| crate::controls::value_at(event.position.x, bounds))
                    {
                        this.sysmon
                            .update(cx, |sysmon, cx| sysmon.request_set_mic_volume(percent, cx));
                    }
                }),
            )
            .child(if muted {
                kit::button(
                    "mic-toggle",
                    "Unmute",
                    Some("icons/mic.svg"),
                    ButtonVariant::Ghost,
                    cx.listener(|this, _, _, cx| {
                        this.sysmon
                            .update(cx, |sysmon, cx| sysmon.request_mic_toggle(cx));
                    }),
                )
            } else {
                kit::button(
                    "mic-toggle",
                    "Mute",
                    Some("icons/x.svg"),
                    ButtonVariant::Ghost,
                    cx.listener(|this, _, _, cx| {
                        this.sysmon
                            .update(cx, |sysmon, cx| sysmon.request_mic_toggle(cx));
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
                        Drag::Mic => sysmon.request_set_mic_volume(percent, cx),
                    });
                }
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.dragging = None;
                }),
            )
            .when(
                brightness.is_some() || volume.is_some() || mic.is_some(),
                |el| {
                    el.child(
                        // the sliders live in one card so the quick page reads
                        // as a stack of cards, the toggles are cards of their
                        // own; skipped outright when there is nothing to slide
                        kit::card("quick-sliders")
                            .when_some(brightness_row, |el, row| el.child(row))
                            .when_some(volume_row, |el, row| el.child(row))
                            .when_some(mic_row, |el, row| el.child(row)),
                    )
                },
            )
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
            .child(self.dnd_section(cx))
            .when_some(profile, |el, _| {
                el.child(kit::setting_row("Power profile", profile_segmented))
            })
            .child(self.power_section(cx))
    }

    /// The DND section: the manual toggle as its own card like the
    /// page's other toggles, with the quiet-hours schedule revealed
    /// beneath it (the schedule toggle appears when DND is on, the
    /// two end fields and the urgent pass-through when the schedule
    /// is on; first switch-on seeds 22 to 7). The fields reuse the
    /// numeric-edit machinery, committed by Enter or a click anywhere.
    fn dnd_section(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let notifications = self.settings.read(cx).notifications;
        let dnd = notifications.dnd;
        let enabled =
            notifications.quiet_from.is_some() && notifications.quiet_to.is_some();
        let from_row = self.quiet_time_row(
            "quiet-from",
            "From",
            EditField::QuietFrom,
            notifications.quiet_from,
            cx,
        );
        let to_row = self.quiet_time_row(
            "quiet-to",
            "To",
            EditField::QuietTo,
            notifications.quiet_to,
            cx,
        );
        let settings = self.settings.clone();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(crate::controls::toggle_row(
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
            ))
            .when(dnd, |el| {
                el.child({
                    let settings = self.settings.clone();
                    crate::controls::toggle_row(
                        "toggle-quiet",
                        "icons/moon.svg",
                        "Schedule quiet hours",
                        enabled,
                        move |_, _, cx| {
                            settings.update(cx, |settings, cx| {
                                let on = !(settings.notifications.quiet_from.is_some()
                                    && settings.notifications.quiet_to.is_some());
                                let (from, to) = if on {
                                    (Some(22), Some(7))
                                } else {
                                    (None, None)
                                };
                                settings.set_quiet_hours(from, to, cx);
                            });
                        },
                    )
                })
            })
            .when(dnd && enabled, |el| {
                el.child(from_row)
                    .child(to_row)
                    .child({
                        let settings = self.settings.clone();
                        crate::controls::toggle_row(
                            "toggle-quiet-urgent",
                            "icons/shield-check.svg",
                            "Let critical through",
                            notifications.quiet_urgent,
                            move |_, _, cx| {
                                settings.update(cx, |settings, cx| {
                                    let urgent = !settings.notifications.quiet_urgent;
                                    settings.set_quiet_urgent(urgent, cx);
                                });
                            },
                        )
                    })
                    .when(self.num_edit.is_some(), |el| {
                        el.child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(crate::theme::current().text_dim))
                                .child("Enter or click away to apply. Whole hours, 24h."),
                        )
                    })
            })
    }

    /// One quiet-hours end: the hour as "22:00", click to edit the
    /// bare hour number, Enter or click-away commits.
    fn quiet_time_row(
        &mut self,
        id: &str,
        name: &str,
        which: EditField,
        hour: Option<u8>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let editing = self.num_edit.as_ref().filter(|edit| edit.which == which);
        let text = editing.map(|edit| edit.text.clone()).unwrap_or_default();
        div()
            .id(SharedString::from(id))
            .flex()
            .items_center()
            .gap_2()
            .px_3p5()
            .py_2p5()
            .rounded_lg()
            .bg(rgb(crate::theme::current().surface))
            .child(
                div()
                    .flex_1()
                    .text_size(px(12.))
                    .text_color(rgb(crate::theme::current().text))
                    .child(name.to_string()),
            )
            .child(
                div()
                    .id(SharedString::from(format!("{id}-field")))
                    .when_some(editing.map(|edit| edit.focus.clone()), |el, focus| {
                        el.track_focus(&focus)
                            .on_key_down(cx.listener(Self::num_edit_key))
                    })
                    .cursor_text()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if this.num_edit.as_ref().is_some_and(|edit| edit.which == which) {
                            return;
                        }
                        let focus = cx.focus_handle();
                        focus.focus(window, cx);
                        this.num_edit = Some(NumEdit {
                            which,
                            // seeded with the committed hour, so
                            // backspace edits what is there
                            text: hour.map(|h| h.to_string()).unwrap_or_default(),
                            focus,
                        });
                        cx.notify();
                    }))
                    .w(px(72.))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(crate::theme::current().inset))
                    .border_1()
                    .border_color(rgb(if editing.is_some() { crate::theme::current().accent } else { crate::theme::current().divider }))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(if editing.is_some() {
                                crate::theme::current().text
                            } else {
                                crate::theme::current().text_dim
                            }))
                            .when(editing.is_some(), |el| el.child(text.clone()))
                            .when(editing.is_none(), |el| {
                                el.child(
                                    hour.map(|h| format!("{h}:00")).unwrap_or_else(|| "Off".into()),
                                )
                            }),
                    ),
            )
    }

    /// The power row: logout, reboot, poweroff, behind the kit's
    /// arm-then-confirm row (held as a field, minted once in `new`).
    /// The commands spawn exactly the way idle.rs powers off monitors:
    /// fire, log nothing, the spawn is the whole conversation. Logind's
    /// shipped policy permits the active seat user; no polkit rules, no
    /// helper units.
    fn power_section(&self, _cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        kit::card("quick-power")
            .child(kit::card_title("Power"))
            .child(kit::card_note(
                "Ends the session. Click an action, click again to confirm.",
            ))
            .child(self.power_row.clone())
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
            .bg(rgb(crate::theme::current().inset))
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
                    .bg(rgb(if active { crate::theme::current().accent } else { crate::theme::current().inset }))
                    .text_color(rgb(if active { crate::theme::current().accent_text } else { crate::theme::current().text_dim }))
                    .hover(|style| {
                        style
                            .bg(rgb(if active { crate::theme::current().accent } else { crate::theme::current().surface }))
                            .text_color(rgb(if active { crate::theme::current().accent_text } else { crate::theme::current().text }))
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
                    .text_color(rgb(crate::theme::current().text_dim))
                    .child("Right-click an app in the dock to pin or unpin it."),
            )
    }

    /// The idle page: the three clauses of the idle contract, in
    /// execution order. Each clock wears an on/off toggle and a
    /// click-to-edit minute field; Enter or a click anywhere else
    /// commits the whole number, nothing is written per keystroke.
    /// The clocks are independent, never clamped, and a dim note says
    /// when the order would surprise.
    /// The night light page: the toggle, the temperature, and the
    /// optional window ("HH:MM" each). The window rules are pinned in
    /// settings.rs tests: start == end is the whole day, start > end
    /// spans midnight.
    /// The night light page: the toggle, the temperature, and the
    /// optional window as dropdown trios (hour, minute, AM/PM). The
    /// window rules are pinned in settings.rs tests: start == end is
    /// the whole day, start > end spans midnight.
    fn night_light_page(&mut self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        const TEMPS: [u32; 6] = [6500, 5000, 4000, 3400, 2700, 2500];
        let config = self.settings.read(cx).night_light.clone();
        let settings_handle = self.settings.clone();

        /// One dropdown of the trio, wired to the view's menu state
        /// and the settings mutator.
        fn time_dropdown(
            this: &mut SettingsView,
            which: NightField,
            part: NightPart,
            value: Option<&str>,
            cx: &mut Context<SettingsView>,
        ) -> gpui::Stateful<Div> {
            let (hour, minute, meridiem) = crate::settings::to_12h(value);
            let (options, current): (Vec<String>, usize) = match part {
                NightPart::Hour => (
                    crate::settings::HOUR_CHOICES
                        .iter()
                        .map(|h| h.to_string())
                        .collect(),
                    hour,
                ),
                NightPart::Minute => (
                    crate::settings::MINUTE_CHOICES
                        .iter()
                        .map(|m| m.to_string())
                        .collect(),
                    minute,
                ),
                NightPart::Meridiem => (
                    crate::settings::MERIDIEM_CHOICES
                        .iter()
                        .map(|m| m.to_string())
                        .collect(),
                    meridiem,
                ),
            };
            let label = options[current].clone();
            let id = format!("night-{which:?}-{part:?}");
            let open = this.night_menu == Some((which, part));
            kit::dropdown(
                cx,
                &id,
                label,
                options,
                current,
                open,
                // toggle
                move |this: &mut SettingsView, _, cx| {
                    this.night_menu =
                        if this.night_menu == Some((which, part)) {
                            None
                        } else {
                            Some((which, part))
                        };
                    cx.notify();
                },
                // pick: recompute this half's "HH:MM", keep the other
                move |index, this: &mut SettingsView, _, cx| {
                    this.night_menu = None;
                    this.settings.update(cx, |settings, cx| {
                        let s = &settings.night_light;
                        let (hour, minute, meridiem) = crate::settings::to_12h(
                            match which {
                                NightField::Start => s.window_start.as_deref(),
                                NightField::End => s.window_end.as_deref(),
                            },
                        );
                        let (hour, minute, meridiem) = match part {
                            NightPart::Hour => (index, minute, meridiem),
                            NightPart::Minute => (hour, index, meridiem),
                            NightPart::Meridiem => (hour, minute, index),
                        };
                        let value = crate::settings::from_12h(hour, minute, meridiem);
                        let (start, end) = match which {
                            NightField::Start => (Some(value), s.window_end.clone()),
                            NightField::End => (s.window_start.clone(), Some(value)),
                        };
                        settings.set_night_light_window(start, end, cx);
                    });
                },
                // close
                move |this: &mut SettingsView, _, cx| {
                    this.night_menu = None;
                    cx.notify();
                },
            )
        }

        let start = config.window_start.clone();
        let end = config.window_end.clone();
        let start_row = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(crate::theme::current().text))
                    .child("From"),
            )
            .child(time_dropdown(
                self, NightField::Start, NightPart::Hour, start.as_deref(), cx,
            ))
            .child(time_dropdown(
                self, NightField::Start, NightPart::Minute, start.as_deref(), cx,
            ))
            .child(time_dropdown(
                self, NightField::Start, NightPart::Meridiem, start.as_deref(), cx,
            ));
        let end_row = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(crate::theme::current().text))
                    .child("To"),
            )
            .child(time_dropdown(
                self, NightField::End, NightPart::Hour, end.as_deref(), cx,
            ))
            .child(time_dropdown(
                self, NightField::End, NightPart::Minute, end.as_deref(), cx,
            ))
            .child(time_dropdown(
                self, NightField::End, NightPart::Meridiem, end.as_deref(), cx,
            ));

        div()
            .id("page-night-light")
            .flex_1()
            .flex()
            .flex_col()
            .gap_3()
            .overflow_y_scroll()
            .child(crate::controls::toggle_row(
                "night-light-toggle",
                "icons/moon.svg",
                "Night light",
                config.enabled,
                move |_, _, cx| {
                    settings_handle.update(cx, |settings, cx| {
                        settings.set_night_light_enabled(!settings.night_light.enabled, cx)
                    });
                },
            ))
            .child(kit::setting_row(
                "Temperature",
                self.segmented(
                    "night-kelvin",
                    &TEMPS,
                    config.kelvin,
                    |kelvin| format!("{kelvin}K"),
                    |settings, kelvin, cx| settings.set_night_light_kelvin(kelvin, cx),
                    cx,
                ),
            ))
            .child(
                kit::card("night-light-window")
                    .child(kit::card_note(
                        "Only tint between these times; both dropdowns set means the window is on. 9:00 PM to 7:00 AM spans midnight.",
                    ))
                    .child(start_row)
                    .child(end_row),
            )
    }
    fn idle_page(&mut self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let idle = self.settings.read(cx).idle;
        let lock_row = self.idle_number_row(
            "idle-lock",
            "icons/lock.svg",
            "Lock after",
            EditField::Lock,
            idle.lock_timeout,
            900,
            cx,
        );
        let screen_row = self.idle_number_row(
            "idle-screen-off",
            "icons/brightness.svg",
            "Screens off after",
            EditField::ScreenOff,
            idle.screen_off_timeout,
            960,
            cx,
        );

        let settings_handle = self.settings.clone();
        div()
            .id("page-idle")
            .flex_1()
            .flex()
            .flex_col()
            .gap_3()
            .overflow_y_scroll()
            // a press anywhere commits the open field: click-away is
            // apply, not discard
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| this.commit_num_edit(cx)),
            )
            .child(
                kit::card("idle-timeouts")
                    .child(lock_row)
                    .child(screen_row)
                    .when(self.num_edit.is_some(), |el| {
                        el.child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(crate::theme::current().text_dim))
                                .child("Enter or click away to apply."),
                        )
                    }),
            )
            .child(crate::controls::toggle_row(
                "idle-lock-before-suspend",
                "icons/shield.svg",
                "Lock before sleep",
                idle.lock_before_suspend,
                move |_, _, cx| {
                    settings_handle.update(cx, |settings, cx| {
                        let enabled = !settings.idle.lock_before_suspend;
                        settings.set_idle_lock_before_suspend(enabled, cx);
                    });
                },
            ))
            .when(
                idle.lock_timeout > 0 && idle.screen_off_timeout > 0,
                |el| {
                    el.when(idle.screen_off_timeout < idle.lock_timeout, |el| {
                        el.child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(crate::theme::current().text_dim))
                                .child(
                                    "Screens blank before the lock engages. The clocks are independent, nothing is clamped.",
                                ),
                        )
                    })
                },
            )
    }

    /// The weather page: the location query with its resolved match,
    /// and the unit. Enter on the field commits the query and runs
    /// the resolve; the coordinates cache into the settings so the
    /// poll never geocodes.
    fn weather_page(&mut self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let weather = self.settings.read(cx).weather.clone();
        let settings_handle = self.settings.clone();
        let location_card = kit::card("weather-location")
            .child(kit::card_title("Location"))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(crate::theme::current().text_dim))
                    .child("A city with its state, or a postal code."),
            )
            .child(
                div()
                    .id("weather-location-field")
                    .when_some(
                        self.weather_edit.as_ref().map(|edit| edit.focus.clone()),
                        |el, focus| {
                            el.track_focus(&focus)
                                .on_key_down(cx.listener(Self::weather_key))
                        },
                    )
                    .cursor_text()
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.weather_edit.is_some() {
                            return;
                        }
                        let focus = cx.focus_handle();
                        focus.focus(window, cx);
                        this.weather_edit = Some(WeatherEdit {
                            // seeded with the committed query, so
                            // backspace edits what is there
                            text: this.settings.read(cx).weather.query.clone(),
                            focus,
                        });
                        this.weather_resolve = None;
                        cx.notify();
                    }))
                    .w_full()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(crate::theme::current().inset))
                    .border_1()
                    .border_color(rgb(if self.weather_edit.is_some() {
                        crate::theme::current().accent
                    } else {
                        crate::theme::current().divider
                    }))
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(rgb(if self.weather_edit.is_some() {
                                crate::theme::current().text
                            } else if weather.query.is_empty() {
                                crate::theme::current().text_dim
                            } else {
                                crate::theme::current().text
                            }))
                            .when_some(
                                self.weather_edit.as_ref().map(|edit| edit.text.clone()),
                                |el, text| el.child(text),
                            )
                            .when(self.weather_edit.is_none(), |el| {
                                el.child(if weather.query.is_empty() {
                                    "Click to type a location".to_string()
                                } else {
                                    weather.query.clone()
                                })
                            }),
                    ),
            )
            .children(match (&self.weather_resolve, &weather.resolved) {
                (Some(WeatherResolve::Working), _) => {
                    vec![div()
                        .text_size(px(11.))
                        .text_color(rgb(crate::theme::current().text_dim))
                        .child("Resolving...")]
                }
                (Some(WeatherResolve::Failed(err)), _) => {
                    vec![div()
                        .text_size(px(11.))
                        .text_color(rgb(crate::theme::URGENT))
                        .child(err.clone())]
                }
                (None, Some(resolved)) => vec![div()
                    .text_size(px(11.))
                    .text_color(rgb(crate::theme::current().text_dim))
                    .child(format!("Resolved: {}", resolved.label))],
                (None, None) => vec![],
            });
        let fahrenheit = weather.fahrenheit;

        div()
            .id("page-weather")
            .flex_1()
            .flex()
            .flex_col()
            .gap_3()
            .overflow_y_scroll()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    // click-away closes the field without resolving
                    if this.weather_edit.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .child(location_card)
            .child(crate::controls::toggle_row(
                "weather-fahrenheit",
                "icons/temp.svg",
                "Fahrenheit",
                fahrenheit,
                move |_, _, cx| {
                    settings_handle.update(cx, |settings, cx| {
                        settings.set_weather_fahrenheit(!settings.weather.fahrenheit, cx);
                    });
                },
            ))
            .when(weather.resolved.is_none(), |el| {
                el.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(crate::theme::current().text_dim))
                        .child(
                            "The weather widget stays hidden until a location resolves.",
                        ),
                )
            })
    }

    /// The weather field's keys: Enter commits (and starts the
    /// resolve), backspace edits, plain characters type.
    fn weather_key(&mut self, event: &gpui::KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.weather_edit.as_mut() else {
            return;
        };
        match event.keystroke.key.as_str() {
            "enter" => {
                let query = std::mem::take(&mut edit.text).trim().to_string();
                self.weather_edit = None;
                if query.is_empty() {
                    self.settings.update(cx, |settings, cx| {
                        settings.set_weather_location(String::new(), None, cx)
                    });
                    return;
                }
                self.weather_resolve = Some(WeatherResolve::Working);
                cx.notify();
                let query_text = query.clone();
                cx.spawn(async move |this, cx| {
                    let result =
                        cx.background_spawn(async move { crate::weather::geolocate(&query) })
                            .await;
                    let _ = this.update(cx, |this, cx| {
                        match result {
                            Ok(resolved) => {
                                this.weather_resolve = None;
                                this.settings.update(cx, |settings, cx| {
                                    settings.set_weather_location(query_text, Some(resolved), cx)
                                });
                            }
                            Err(err) => {
                                this.weather_resolve = Some(WeatherResolve::Failed(format!(
                                    "Resolve failed: {err}"
                                )));
                            }
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            "escape" => {
                self.weather_edit = None;
                cx.notify();
            }
            "backspace" => {
                edit.text.pop();
                cx.notify();
            }
            "space" => {
                edit.text.push(' ');
                cx.notify();
            }
            // shift alone is a capital: the key name is the lowercase
            // letter, the shift flag says to raise it. Control and alt
            // stay shortcuts, not text.
            other if other.chars().count() == 1
                && (!event.keystroke.modifiers.modified()
                    || (event.keystroke.modifiers.shift
                        && !event.keystroke.modifiers.control
                        && !event.keystroke.modifiers.alt
                        && !event.keystroke.modifiers.platform)) =>
            {
                if event.keystroke.modifiers.shift {
                    edit.text.push_str(&other.to_uppercase());
                } else {
                    edit.text.push_str(other);
                }
                cx.notify();
            }
            _ => {}
        }
    }


    /// One idle row: icon, name, an on/off toggle, the minute field,
    /// in that order. Off disables the field (the clock is 0); on
    /// again revives the clock at its default. Click the field to
    /// edit; Enter or a click elsewhere applies.
    #[allow(clippy::too_many_arguments)]
    fn idle_number_row(
        &mut self,
        id: &str,
        icon: &'static str,
        name: &str,
        which: EditField,
        seconds: u64,
        default_seconds: u64,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let enabled = seconds > 0;
        let editing = self
            .num_edit
            .as_ref()
            .filter(|edit| edit.which == which && enabled);
        let text = editing
            .map(|edit| edit.text.clone())
            .unwrap_or_else(|| (seconds / 60).to_string());
        let toggle_id = SharedString::from(format!("{id}-enabled"));
        let settings = self.settings.clone();
        div()
            .id(SharedString::from(id))
            .flex()
            .items_center()
            .gap_2()
            .py_1()
            .child(gpui::svg().path(icon).size(px(16.)).text_color(rgb(
                if enabled { crate::theme::current().text } else { crate::theme::current().text_dim },
            )))
            .child(
                div()
                    .flex_1()
                    .text_size(px(12.))
                    .text_color(rgb(if enabled { crate::theme::current().text } else { crate::theme::current().text_dim }))
                    .child(name.to_string()),
            )
            .child(
                div()
                    .id(toggle_id)
                    .cursor_pointer()
                    .child(crate::controls::toggle_switch(enabled))
                    .on_click(move |_, _, cx| {
                        // toggling off is 0 (Off); on again revives the
                        // clock at its default
                        settings.update(cx, |settings, cx| {
                            let on = match which {
                                EditField::Lock => settings.idle.lock_timeout == 0,
                                EditField::ScreenOff => settings.idle.screen_off_timeout == 0,
                                // the quiet ends never pass through here
                                _ => unreachable!("quiet fields have no idle toggle"),
                            };
                            let seconds = if on { default_seconds } else { 0 };
                            match which {
                                EditField::Lock => {
                                    settings.set_idle_lock_timeout(seconds, cx)
                                }
                                EditField::ScreenOff => {
                                    settings.set_idle_screen_off_timeout(seconds, cx)
                                }
                                _ => unreachable!("quiet fields have no idle toggle"),
                            }
                        });
                    }),
            )
            .child(
                div()
                    .id(SharedString::from(format!("{id}-field")))
                    .when_some(editing.map(|edit| edit.focus.clone()), |el, focus| {
                        el.track_focus(&focus)
                            .on_key_down(cx.listener(Self::num_edit_key))
                    })
                    .when(enabled, |el| {
                        el.cursor_text().on_click(cx.listener(
                            move |this, _, window, cx| {
                                if this
                                    .num_edit
                                    .as_ref()
                                    .is_some_and(|edit| edit.which == which)
                                {
                                    return;
                                }
                                let focus = cx.focus_handle();
                                focus.focus(window, cx);
                                this.num_edit = Some(NumEdit {
                                    which,
                                    // seeded with the committed minutes,
                                    // so backspace edits what is there
                                    text: (seconds / 60).to_string(),
                                    focus,
                                });
                                cx.notify();
                            },
                        ))
                    })
                    .w(px(72.))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(crate::theme::current().inset))
                    .border_1()
                    .border_color(rgb(if editing.is_some() { crate::theme::current().accent } else { crate::theme::current().divider }))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(if editing.is_some() {
                                crate::theme::current().text
                            } else if enabled {
                                crate::theme::current().text_dim
                            } else {
                                crate::theme::current().text_dim
                            }))
                            .text_align(gpui::TextAlign::Right)
                            .when(editing.is_some(), |el| el.child(text.clone()))
                            .when(editing.is_none(), |el| {
                                el.child(if enabled {
                                    format!("{} min", seconds / 60)
                                } else {
                                    "Off".to_string()
                                })
                            }),
                    ),
            )
    }

    /// Commit the open idle edit, if any: the digits are minutes, the
    /// settings seam gets seconds. An unparsable field reverts.
    fn commit_num_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.num_edit.take() else {
            return;
        };
        let Ok(minutes) = edit.text.trim().parse::<u64>() else {
            cx.notify();
            return;
        };
        let seconds = minutes * 60;
        let which = edit.which;
        self.settings.update(cx, |settings, cx| match which {
            EditField::Lock => settings.set_idle_lock_timeout(seconds, cx),
            EditField::ScreenOff => settings.set_idle_screen_off_timeout(seconds, cx),
            // the quiet ends are hours of the day; anything past 23
            // (or empty) reverts
            EditField::QuietFrom | EditField::QuietTo => {
                // the quiet ends are hours of the day; anything past
                // 23 (or empty) reverts
                let Some(hour) =
                    edit.text.trim().parse::<u8>().ok().filter(|h| *h <= 23)
                else {
                    cx.notify();
                    return;
                };
                let s = &settings.notifications;
                let (from, to) = match which {
                    EditField::QuietFrom => (Some(hour), s.quiet_to),
                    _ => (s.quiet_from, Some(hour)),
                };
                settings.set_quiet_hours(from, to, cx);
            }
        });
    }

    /// Keys for an open idle edit: digits build the number, Enter
    /// commits, and anything else is ignored while typing.
    fn num_edit_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        if key == "enter" {
            self.commit_num_edit(cx);
            return;
        }
        if let Some(edit) = &mut self.num_edit {
            match key {
                "backspace" => {
                    edit.text.pop();
                    cx.notify();
                }
                digit if digit.chars().count() == 1 && digit.as_bytes()[0].is_ascii_digit() && !event.keystroke.modifiers.modified() => {
                    if edit.text.len() < 5 {
                        edit.text.push_str(digit);
                        cx.notify();
                    }
                }
                _ => {}
            }
        }
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
            .bg(rgb(crate::theme::current().surface))
            .child(
                gpui::svg()
                    .path(match kind.icon_spec() {
                        Some(crate::settings::WidgetIconSpec::Path(path)) => path,
                        _ => "icons/puzzle.svg",
                    })
                    .size(px(16.))
                    .text_color(rgb(if enabled { crate::theme::current().text } else { crate::theme::current().text_dim })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.))
                    .text_color(rgb(if enabled { crate::theme::current().text } else { crate::theme::current().text_dim }))
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
                div().text_size(px(11.)).text_color(rgb(crate::theme::current().text_dim)).child(
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
                    .text_color(rgb(crate::theme::current().text_dim))
                    .child(label.to_uppercase()),
            )
            .child(div().flex_1().h(px(1.)).bg(rgba(crate::theme::current().divider_soft)))
    }

    /// The insertion indicator: the accent line a drag leaves between
    /// chips, saying where the widget will land.
    fn drop_line(&self) -> Div {
        div().w_full().h(px(2.)).rounded_full().bg(rgb(crate::theme::current().accent))
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
                        .text_color(rgb(crate::theme::current().text_dim))
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
                    .text_color(rgb(crate::theme::current().text))
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
            .bg(rgb(crate::theme::current().inset))
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
                        .bg(rgb(if active { crate::theme::current().accent } else { crate::theme::current().inset }))
                        .hover(|style| style.bg(rgb(if active { crate::theme::current().accent } else { crate::theme::current().surface })))
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
                                    .text_color(rgb(if active { crate::theme::current().accent_text } else { crate::theme::current().text_dim })),
                            )
                        })
                        .when(mode != WidgetMode::Icon, |el| {
                            el.child(
                                div()
                                    .text_size(px(10.))
                                    .line_height(px(12.))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(rgb(if active { crate::theme::current().accent_text } else { crate::theme::current().text_dim }))
                                    .child("Aa"),
                            )
                        })
                }),
            )
    }

    fn backgrounds_page(&mut self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let background = self.settings.read(cx).background.clone();
        let mut entries = vec![("default".to_string(), crate::settings::default_wallpaper())];
        entries.extend(crate::settings::background_images(&background.folder));

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
                                .text_color(rgb(crate::theme::current().text_dim))
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
            .child(kit::setting_row(
                "Rotate",
                self.segmented(
                    "rotate",
                    &ROTATIONS,
                    background.rotate_minutes,
                    |minutes| {
                        if minutes == 0 {
                            "Off".to_string()
                        } else {
                            format!("{minutes} min")
                        }
                    },
                    |settings, minutes, cx| settings.set_background_rotate(minutes, cx),
                    cx,
                ),
            ))
            .child(crate::controls::toggle_row(
                "theme-derived",
                "icons/sun.svg",
                "Derive colors from wallpaper",
                self.settings.read(cx).theme.wallpaper_derived,
                cx.listener(|this, _, _, cx| {
                    this.settings.update(cx, |settings, cx| {
                        settings.set_theme_derived(!settings.theme.wallpaper_derived, cx);
                    });
                    this.thumbs.clear();
                    cx.notify();
                }),
            ))
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
                    .border_color(rgb(if selected { crate::theme::current().accent } else { crate::theme::current().divider }))
                    .when_some(thumb, |el, thumb| {
                        el.child(
                            img(gpui::ImageSource::Render(thumb))
                                .object_fit(ObjectFit::Cover)
                                .size_full(),
                        )
                    })
                    .when(!has_thumb, |el| el.bg(rgb(crate::theme::current().inset)))
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
                    .text_color(rgb(if selected { crate::theme::current().text } else { crate::theme::current().text_dim }))
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
            .when(self.page == Page::Idle, |el| el.child(self.idle_page(cx)))
            .when(self.page == Page::Backgrounds, |el| {
                el.child(self.backgrounds_page(cx))
            })
            .when(self.page == Page::Weather, |el| {
                el.child(self.weather_page(cx))
            })
            .when(self.page == Page::NightLight, |el| {
                el.child(self.night_light_page(cx))
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
                .child(div().w(px(1.)).bg(rgba(crate::theme::current().divider_soft)))
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
