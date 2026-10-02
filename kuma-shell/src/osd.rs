//! The OSD: a small card, centered under the bar, for the changes a
//! user makes blind: volume, mute, mic, brightness, the notification
//! bell's sleep, the power profile. It watches SysMon and Settings and
//! diffs snapshots, so both the shell's own optimistic writes and
//! external changes (a keybind running wpctl, a hardware key) light it
//! up, and repeated changes re-arm the one window instead of stacking.

use std::time::Duration;

use gpui::{
    App, AppContext, Bounds, Context, Entity, Render, Window, WindowBackgroundAppearance,
    WindowBounds, WindowHandle, WindowKind, WindowOptions, div,
    layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
    point, prelude::*, px, relative, rgb, rgba, size, svg,
};

use crate::panel::PanelHost;
use crate::settings::Settings;
use crate::sysmon::{Brightness, Mic, PowerProfile, SysMon, Volume};
use crate::theme::*;

/// How long the OSD hangs after the last change.
const OSD_DURATION: Duration = Duration::from_millis(1500);
/// The window is as wide as the card; the height refines itself to the
/// content (the toast's measured-height flow).
const OSD_WIDTH: f32 = 240.;
const OSD_HEIGHT: f32 = 48.;

/// One card's content: what changed, how much, and whether to shout.
#[derive(Clone, Debug, PartialEq)]
pub struct OsdContent {
    icon: &'static str,
    label: &'static str,
    value: String,
    /// The bar's fill, 0..=100; None means no bar (the microphone).
    percent: Option<u8>,
    urgent: bool,
}

/// The slice of session state the OSD watches: a change in any of it
/// is a change a user made blind.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct OsdSnapshot {
    volume: Option<Volume>,
    mic: Option<Mic>,
    brightness: Option<Brightness>,
    /// The notification bell's sleep flag; None before the settings'
    /// first read.
    dnd: Option<bool>,
    profile: Option<PowerProfile>,
}

fn snapshot_of(sysmon: &SysMon, settings: &Settings) -> OsdSnapshot {
    OsdSnapshot {
        volume: sysmon.volume,
        mic: sysmon.mic,
        brightness: sysmon.brightness,
        dnd: Some(settings.notifications.dnd),
        profile: sysmon.power_profile,
    }
}

/// The card for one change; None when the two snapshots agree (the
/// full poll notifies even when nothing moved). When several changed
/// in one tick, volume wins: it is the commonest drumbeat. The dnd
/// and profile arms need a previous value to diff against: their
/// first sighting is the startup fill, not a thing the user did.
fn content_for_change(before: OsdSnapshot, after: OsdSnapshot) -> Option<OsdContent> {
    if before.volume != after.volume
        && let Some(volume) = after.volume
    {
        return Some(volume_content(volume));
    }
    if before.brightness != after.brightness
        && let Some(brightness) = after.brightness
    {
        return Some(brightness_content(brightness));
    }
    if before.mic != after.mic
        && let Some(mic) = after.mic
    {
        return Some(mic_content(mic));
    }
    if before.profile != after.profile
        && let Some(profile) = after.profile
        && before.profile.is_some()
    {
        return Some(profile_content(profile));
    }
    if before.dnd != after.dnd
        && let Some(dnd) = after.dnd
        && before.dnd.is_some()
    {
        return Some(dnd_content(dnd));
    }
    None
}

fn volume_content(volume: Volume) -> OsdContent {
    OsdContent {
        icon: "icons/volume.svg",
        label: "Volume",
        value: if volume.muted {
            "Muted".into()
        } else {
            format!("{}%", volume.percent)
        },
        percent: Some(volume.percent),
        urgent: volume.muted,
    }
}

fn mic_content(mic: Mic) -> OsdContent {
    OsdContent {
        icon: "icons/mic.svg",
        label: "Microphone",
        value: if mic.muted { "Muted".into() } else { "On".into() },
        percent: None,
        urgent: mic.muted,
    }
}

fn brightness_content(brightness: Brightness) -> OsdContent {
    OsdContent {
        icon: "icons/brightness.svg",
        label: "Brightness",
        value: format!("{}%", brightness.percent),
        percent: Some(brightness.percent),
        urgent: false,
    }
}

fn profile_content(profile: PowerProfile) -> OsdContent {
    OsdContent {
        icon: "icons/power-profile.svg",
        label: "Power Profile",
        value: profile.title().into(),
        percent: None,
        urgent: profile == PowerProfile::Performance,
    }
}

fn dnd_content(dnd: bool) -> OsdContent {
    OsdContent {
        icon: "icons/bell.svg",
        label: "Do Not Disturb",
        value: if dnd { "On".into() } else { "Off".into() },
        percent: None,
        urgent: dnd,
    }
}

pub struct Osd {
    sysmon: Entity<SysMon>,
    settings: Entity<Settings>,
    /// The previous snapshot: first sight records, later ones diff.
    last: Option<OsdSnapshot>,
    window: Option<WindowHandle<OsdView>>,
    /// Bumped on every show; the expiry timer only dismisses the
    /// generation it was armed for, so re-shows outlive stale timers.
    generation: u64,
}

impl Osd {
    pub fn new(sysmon: Entity<SysMon>, settings: Entity<Settings>) -> Self {
        Osd {
            sysmon,
            settings,
            last: None,
            window: None,
            generation: 0,
        }
    }

    /// The observers' body: take the new snapshot, and a change
    /// (and only a change) raises the card.
    fn reconcile(&mut self, cx: &mut Context<Self>) {
        let next = snapshot_of(self.sysmon.read(cx), self.settings.read(cx));
        // the all-empty snapshot is the state before the poll's first
        // confirmed read lands, not a thing that happened: recording
        // it as first sight would turn the startup fill into a toast
        if self.last.is_none() && next == OsdSnapshot::default() {
            return;
        }
        match self.last.replace(next) {
            // the first real snapshot says nothing: showing "Volume
            // 64%" at login is noise, not feedback
            None => {}
            Some(before) => {
                if let Some(content) = content_for_change(before, next) {
                    log::info!("osd: {} {}", content.label, content.value);
                    self.show(content, cx);
                }
            }
        }
    }

    fn show(&mut self, content: OsdContent, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        // the card is pushed into the view, never read back from this
        // entity: the window's first render happens inside open_window,
        // while this update is still on the stack
        match &mut self.window {
            Some(handle) => {
                let content = content.clone();
                if let Err(err) = handle.update(cx, |view, _, cx| {
                    view.content = Some(content);
                    cx.notify();
                }) {
                    log::error!("osd update failed: {err:#}");
                }
            }
            None => {
                let top = cx
                    .try_global::<PanelHost>()
                    .map(|host| host.bar().panel_top + 8.)
                    .unwrap_or(8.);
                match cx.open_window(osd_window_options(top), |_, cx| {
                    cx.new(|_| OsdView {
                        content: Some(content.clone()),
                        height: OSD_HEIGHT,
                    })
                }) {
                    Ok(handle) => self.window = Some(handle),
                    Err(err) => log::error!("failed to open osd window: {err:#}"),
                }
            }
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(OSD_DURATION).await;
            let _ = this.update(cx, |this, cx| {
                if this.generation == generation {
                    this.dismiss(cx);
                }
            });
        })
        .detach();
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.window.take() {
            crate::panel::defer_close(
                Box::new(move |cx| {
                    if let Err(err) = handle.update(cx, |_, window, _| window.remove_window()) {
                        log::error!("closing osd failed: {err:#}");
                    }
                }),
                cx,
            );
        }
    }
}

/// Wire the OSD to its sources: every notify is a chance to diff.
pub fn run(
    osd: &Entity<Osd>,
    sysmon: &Entity<SysMon>,
    settings: &Entity<Settings>,
    cx: &mut App,
) {
    let watcher = osd.clone();
    cx.observe(sysmon, move |_, cx| {
        watcher.update(cx, |osd, cx| osd.reconcile(cx));
    })
    .detach();
    let watcher = osd.clone();
    cx.observe(settings, move |_, cx| {
        watcher.update(cx, |osd, cx| osd.reconcile(cx));
    })
    .detach();
}

/// The card, hanging centered under the bar: icon, name, value, and a
/// bar where there is a quantity to show. The content arrives pushed
/// from `Osd` (this view reads no entity), so it can render safely
/// inside the window's opening update.
pub struct OsdView {
    content: Option<OsdContent>,
    /// The live window height: refined by the measured-height flow.
    height: f32,
}

impl Render for OsdView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(content) = self.content.clone() else {
            return div().size_full();
        };
        let color = if content.urgent { URGENT } else { ACCENT };
        let icon_color = if content.urgent { URGENT } else { TEXT };
        let card = div()
            .w(px(OSD_WIDTH))
            .flex()
            .flex_col()
            .gap_1p5()
            .px(px(14.))
            .py(px(10.))
            .rounded_xl()
            .bg(rgba(PANEL_BG))
            .border_1()
            .border_color(rgb(DIVIDER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        svg()
                            .path(content.icon)
                            .size(px(16.))
                            .text_color(rgb(icon_color)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .text_color(rgb(TEXT))
                            .truncate()
                            .child(content.label),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(icon_color))
                            .child(content.value),
                    ),
            )
            .when_some(content.percent, |el, percent| {
                el.child(
                    div()
                        .h(px(4.))
                        .w_full()
                        .rounded_full()
                        .bg(rgb(INSET))
                        .overflow_hidden()
                        .child(
                            div()
                                .h_full()
                                .w(relative(percent as f32 / 100.))
                                .rounded_full()
                                .bg(rgb(color)),
                        ),
                )
            });

        let view = cx.weak_entity();
        let measured =
            crate::panel::MeasureHeight::new(card, move |content_height, window, cx| {
                let Some(view) = view.upgrade() else {
                    return;
                };
                let mut resized = None;
                view.update(cx, |this, cx| {
                    let new_height = content_height.clamp(40., 96.);
                    if (this.height - new_height).abs() > 0.5 {
                        this.height = new_height;
                        cx.notify();
                        resized = Some(new_height);
                    }
                });
                if let Some(height) = resized {
                    window.resize(size(px(OSD_WIDTH), px(height)));
                }
            });

        div()
            .size_full()
            .flex()
            .child(div().w(px(OSD_WIDTH)).h(px(self.height)).child(measured))
    }
}

/// Centered under the bar's bottom edge: anchored to the top edge
/// only, since an unanchored axis centers the surface.
fn osd_window_options(top: f32) -> WindowOptions {
    WindowOptions {
        titlebar: None,
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(OSD_WIDTH), px(OSD_HEIGHT)),
        })),
        app_id: Some("kuma-shell-osd".into()),
        window_background: WindowBackgroundAppearance::Transparent,
        kind: WindowKind::LayerShell(LayerShellOptions {
            namespace: "kuma-shell-osd".into(),
            layer: Layer::Overlay,
            exclusive_zone: Some(px(-1.)),
            anchor: Anchor::TOP,
            keyboard_interactivity: KeyboardInteractivity::None,
            margin: Some((px(top), px(0.), px(0.), px(0.))),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_rises_and_agreement_shows_nothing() {
        let quiet = OsdSnapshot::default();
        let loud = OsdSnapshot {
            volume: Some(Volume {
                percent: 64,
                muted: false,
            }),
            ..Default::default()
        };
        let content = content_for_change(quiet, loud).expect("a change yields content");
        assert_eq!(content.label, "Volume");
        assert_eq!(content.value, "64%");
        // the poll that found nothing new shows nothing
        assert_eq!(content_for_change(loud, loud), None);
    }

    #[test]
    fn muted_volume_is_urgent_and_keeps_its_bar() {
        let content = volume_content(Volume {
            percent: 30,
            muted: true,
        });
        assert!(content.urgent);
        assert_eq!(content.value, "Muted");
        assert_eq!(content.percent, Some(30));
    }

    #[test]
    fn the_microphone_has_no_bar_but_names_its_state() {
        let muted = mic_content(Mic { muted: true });
        assert_eq!(muted.value, "Muted");
        assert!(muted.urgent);
        assert_eq!(muted.percent, None);
        let on = mic_content(Mic { muted: false });
        assert_eq!(on.value, "On");
        assert!(!on.urgent);
    }

    #[test]
    fn volume_outranks_brightness_when_both_move() {
        let before = OsdSnapshot::default();
        let after = OsdSnapshot {
            volume: Some(Volume {
                percent: 50,
                muted: false,
            }),
            brightness: Some(Brightness { percent: 70 }),
            ..Default::default()
        };
        assert_eq!(
            content_for_change(before, after).map(|content| content.label),
            Some("Volume")
        );
    }

    #[test]
    fn dnd_and_profile_name_their_change_but_not_their_first_sighting() {
        // the startup fill: a value arriving with no previous one is
        // the poll finding the world, not the user pressing anything
        let fill = OsdSnapshot {
            profile: Some(PowerProfile::Balanced),
            dnd: Some(false),
            ..Default::default()
        };
        assert_eq!(content_for_change(OsdSnapshot::default(), fill), None);

        // a real toggle names itself
        let on = OsdSnapshot { dnd: Some(true), ..fill };
        let content = content_for_change(fill, on).expect("a dnd toggle yields content");
        assert_eq!(content.label, "Do Not Disturb");
        assert_eq!(content.value, "On");
        assert!(content.urgent);

        let fast = OsdSnapshot {
            profile: Some(PowerProfile::Performance),
            ..fill
        };
        let content = content_for_change(fill, fast).expect("a profile change yields content");
        assert_eq!(content.label, "Power Profile");
        assert_eq!(content.value, "Performance");
        assert!(content.urgent);
    }
}
