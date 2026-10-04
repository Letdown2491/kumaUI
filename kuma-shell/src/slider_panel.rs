use std::collections::HashMap;

use gpui::{
    Context, Entity, ObjectFit, Pixels, Render, TextAlign, Window, div, img, prelude::*, px, rgb,
    rgba, size,
};

use crate::controls::{self, TrackStash};
use crate::imaging::{IconImage, build_icon_index, decode_icon_file, icon_roots};
use crate::panel_kit as kit;
use crate::sysmon::{Stream, SysMon};
use crate::theme::*;

/// One widget, one control: the mini panel a value widget opens under
/// itself. Volume adds a mute toggle; the microphone is the capture
/// gain plus its mute; brightness is just its slider.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SliderKind {
    Volume,
    Brightness,
    Mic,
}

impl SliderKind {
    fn title(self) -> &'static str {
        match self {
            SliderKind::Volume => "Volume",
            SliderKind::Brightness => "Brightness",
            SliderKind::Mic => "Microphone",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            SliderKind::Volume => "icons/volume.svg",
            SliderKind::Brightness => "icons/brightness.svg",
            SliderKind::Mic => "icons/mic.svg",
        }
    }

    fn percent(self, sysmon: &SysMon) -> Option<u8> {
        match self {
            SliderKind::Volume => sysmon.volume.map(|volume| volume.percent),
            SliderKind::Brightness => sysmon.brightness.map(|b| b.percent),
            SliderKind::Mic => sysmon.mic.map(|mic| mic.percent),
        }
    }
}

pub struct SliderPanelView {
    geometry: crate::panel::PanelGeometry,
    sysmon: Entity<SysMon>,
    kind: SliderKind,
    /// The track's bounds stash, live while the pointer drags the slider.
    dragging: Option<TrackStash>,
    /// The volume panel's per-stream drag: the stream id plus its
    /// track's bounds stash. One pointer drags at a time.
    stream_drag: Option<(u32, TrackStash)>,
    /// The volume panel's icon shelf: decoded desktop icons keyed by
    /// the desktop file's icon name, matched to streams at render time.
    app_icons: HashMap<String, Option<IconImage>>,
    /// The desktop app list the shelf was built from.
    apps: Vec<crate::launcher::AppEntry>,
}

impl SliderPanelView {
    pub fn new(
        kind: SliderKind,
        sysmon: Entity<SysMon>,
        _window: &mut Window,
        cx: &mut Context<Self>,
        geometry: crate::panel::PanelGeometry,
    ) -> Self {
        cx.observe(&sysmon, |_, _, cx| cx.notify()).detach();
        if kind == SliderKind::Volume {
            sysmon.update(cx, |sysmon, cx| sysmon.scan_streams_now(cx));
        }
        let mut this = Self {
            geometry,
            sysmon,
            kind,
            dragging: None,
            stream_drag: None,
            app_icons: HashMap::new(),
            apps: Vec::new(),
        };
        if kind == SliderKind::Volume {
            this.build_app_icons(cx);
        }
        this
    }

    /// The stream rows' icons: one background pass decodes every
    /// desktop file's icon once per panel open; matching a stream to
    /// its app stays a cheap lookup after that.
    fn build_app_icons(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let (apps, index) = cx
                .background_spawn(async move {
                    let apps = crate::launcher::load_apps();
                    let index = build_icon_index(&icon_roots());
                    (apps, index)
                })
                .await;
            let mut icons: HashMap<String, Option<IconImage>> = HashMap::new();
            for app in &apps {
                if app.icon.is_empty() || icons.contains_key(&app.icon) {
                    continue;
                }
                icons.insert(
                    app.icon.clone(),
                    index.get(&app.icon).and_then(|path| decode_icon_file(path)),
                );
            }
            let _ = this.update(cx, |this, cx| {
                this.apps = apps;
                this.app_icons = icons;
                cx.notify();
            });
        })
        .detach();
    }

    /// The icon for one stream: the client binary (then the reported
    /// name) matched against the desktop files, falling back to the
    /// generic volume glyph when nothing fits.
    fn stream_icon(&self, stream: &Stream) -> Option<IconImage> {
        let binary = stream.binary.to_lowercase();
        let name = stream.name.to_lowercase();
        let app = self.apps.iter().find(|app| {
            app.exec.split_whitespace().next().map(|token| token.to_lowercase())
                == Some(binary.clone())
                || app.name.to_lowercase() == binary
                || app.name.to_lowercase() == name
        })?;
        self.app_icons.get(&app.icon).cloned().flatten()
    }

    fn on_drag(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let Some(percent) = self
            .dragging
            .as_ref()
            .and_then(|stash| stash.get())
            .and_then(|bounds| controls::value_at(x, bounds))
        else {
            return;
        };
        self.sysmon.update(cx, |sysmon, cx| match self.kind {
            SliderKind::Volume => sysmon.request_set_volume(percent, cx),
            SliderKind::Brightness => sysmon.request_set_brightness(percent, cx),
            SliderKind::Mic => sysmon.request_set_mic_volume(percent, cx),
        });
    }

    /// A stream row's drag: same shape as the master slider's, but the
    /// request names the stream. A stream that vanishes mid-drag just
    /// leaves the writes pointing at a dead id: wpctl refuses them,
    /// the error logs, and the mouse-up clears the drag.
    fn on_stream_drag(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let Some((id, stash)) = self.stream_drag.as_ref() else {
            return;
        };
        let Some(percent) = stash.get().and_then(|bounds| controls::value_at(x, bounds)) else {
            return;
        };
        let id = *id;
        self.sysmon
            .update(cx, |sysmon, cx| sysmon.request_set_stream_volume(id, percent, cx));
    }
}

impl Render for SliderPanelView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sysmon = self.sysmon.read(cx);
        let volume = sysmon.volume;
        let percent = self.kind.percent(sysmon);
        let muted = match self.kind {
            SliderKind::Volume => volume.is_some_and(|volume| volume.muted),
            SliderKind::Mic => sysmon.mic.is_some_and(|mic| mic.muted),
            SliderKind::Brightness => false,
        };

        let stash = controls::track_stash();
        let kind = self.kind;

        let content = div()
            .flex()
            .flex_col()
            .pt(px(10.))
            .px(px(16.))
            .pb(px(12.))
            .gap_2()
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                if this.dragging.is_some() {
                    this.on_drag(event.position.x, cx);
                } else if this.stream_drag.is_some() {
                    this.on_stream_drag(event.position.x, cx);
                }
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.dragging = None;
                    this.stream_drag = None;
                }),
            )
            .child(kit::pane_header(self.kind.title()))
            .children(percent.map(|percent| {
                controls::slider_row(
                    kind.icon(),
                    if muted && kind != SliderKind::Brightness {
                        URGENT
                    } else {
                        TEXT
                    },
                    percent,
                    controls::slider_track(percent, stash.clone()),
                )
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                        this.dragging = Some(stash.clone());
                        if let Some(percent) = stash
                            .get()
                            .and_then(|bounds| controls::value_at(event.position.x, bounds))
                        {
                            this.sysmon.update(cx, |sysmon, cx| match kind {
                                SliderKind::Volume => sysmon.request_set_volume(percent, cx),
                                SliderKind::Brightness => {
                                    sysmon.request_set_brightness(percent, cx)
                                }
                                SliderKind::Mic => sysmon.request_set_mic_volume(percent, cx),
                            });
                        }
                    }),
                )
            }))
            .when(percent.is_some() && kind != SliderKind::Brightness, |el| {
                let muted_icon = match kind {
                    SliderKind::Volume => "icons/volume.svg",
                    _ => "icons/mic.svg",
                };
                el.child(kit::button(
                    "mute-toggle",
                    if muted { "Unmute" } else { "Mute" },
                    Some(if muted { "icons/x.svg" } else { muted_icon }),
                    kit::ButtonVariant::Ghost,
                    cx.listener(|this, _, _, cx| match this.kind {
                        SliderKind::Volume => {
                            this.sysmon
                                .update(cx, |sysmon, cx| sysmon.request_mute_toggle(cx));
                        }
                        SliderKind::Mic => {
                            this.sysmon
                                .update(cx, |sysmon, cx| sysmon.request_mic_toggle(cx));
                        }
                        SliderKind::Brightness => {}
                    }),
                ))
            });

        // the per-app rows: one per playback stream, under a divider.
        // The scan rides only while the panel is open, so this reads
        // whatever the last poll caught.
        let content = content.when(kind == SliderKind::Volume, |el| {
            let streams = sysmon.streams.clone();
            el.child(div().mt_1().h(px(1.)).w_full().bg(rgba(DIVIDER_SOFT)))
                .children(if streams.is_empty() {
                    vec![div()
                        .id("no-streams")
                        .py_1()
                        .text_size(px(11.5))
                        .text_color(rgb(TEXT_DIM))
                        .child("No apps playing")]
                } else {
                    streams
                        .iter()
                        .map(|stream| {
                            let stash = controls::track_stash();
                            let id = stream.id;
                            let icon = self.stream_icon(stream);
                            let label = if stream.name.is_empty()
                                || stream.name == stream.binary
                            {
                                stream.binary.clone()
                            } else {
                                format!("{} ({})", stream.name, stream.binary)
                            };
                            div()
                                .id(gpui::SharedString::from(format!("stream-{id}")))
                                .flex()
                                .items_center()
                                .gap_2()
                                .py_1()
                                .child(
                                    div()
                                        .size(px(18.))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .overflow_hidden()
                                        .when_some(icon.clone(), |el, icon| match icon {
                                            IconImage::Raster(raster) => el.child(
                                                img(gpui::ImageSource::Render(raster))
                                                    .object_fit(ObjectFit::Cover)
                                                    .size_full(),
                                            ),
                                            IconImage::Svg(bytes) => el.child(
                                                gpui::svg()
                                                    .data(&bytes)
                                                    .size(px(16.))
                                                    .text_color(rgb(TEXT)),
                                            ),
                                        })
                                        .when_none(&icon, |el| {
                                            el.child(
                                                gpui::svg()
                                                    .path("icons/volume.svg")
                                                    .size(px(14.))
                                                    .text_color(rgb(TEXT_DIM)),
                                            )
                                        }),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_size(px(12.))
                                        .text_color(rgb(TEXT))
                                        .truncate()
                                        .child(label),
                                )
                                .child(controls::slider_track(stream.percent, stash.clone()))
                                .child(
                                    div()
                                        .w(px(32.))
                                        .text_size(px(11.))
                                        .text_color(rgb(TEXT_DIM))
                                        .text_align(TextAlign::Right)
                                        .child(format!("{}%", stream.percent)),
                                )
                                .child(kit::icon_button(
                                    gpui::SharedString::from(format!("stream-mute-{id}")),
                                    if stream.muted {
                                        "icons/x.svg"
                                    } else {
                                        "icons/volume.svg"
                                    },
                                    kit::ButtonVariant::Ghost,
                                    cx.listener(move |this, _, _, cx| {
                                        this.sysmon.update(cx, |sysmon, cx| {
                                            sysmon.request_stream_mute_toggle(id, cx)
                                        });
                                    }),
                                ))
                                .on_mouse_down(
                                    gpui::MouseButton::Left,
                                    cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                                        this.stream_drag = Some((id, stash.clone()));
                                        if let Some(percent) = stash
                                            .get()
                                            .and_then(|bounds| {
                                                controls::value_at(event.position.x, bounds)
                                            })
                                        {
                                            this.sysmon.update(cx, |sysmon, cx| {
                                                sysmon.request_set_stream_volume(
                                                    id, percent, cx,
                                                )
                                            });
                                        }
                                    }),
                                )
                        })
                        .collect::<Vec<_>>()
                })
        });

        // measured-panel flow: the content height refines the panel height
        let view = cx.weak_entity();
        let width = self.geometry.width;
        let measured = crate::panel::MeasureHeight::new(content, move |height, window, cx| {
            let Some(view) = view.upgrade() else {
                return;
            };
            let mut resized = None;
            view.update(cx, |this, cx| {
                if (this.geometry.height - height).abs() > 0.5 {
                    this.geometry.height = height;
                    cx.notify();
                    resized = Some(height);
                }
            });
            if let Some(height) = resized {
                window.resize(size(px(width), px(height)));
            }
        });

        crate::panel::chrome(self.geometry, window, measured)
    }
}
