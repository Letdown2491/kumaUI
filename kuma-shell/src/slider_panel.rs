use gpui::{Context, Entity, Pixels, Render, Window, div, prelude::*, px, rgb, size};

use crate::controls::{self, TrackStash};
use crate::sysmon::SysMon;
use crate::theme::*;

/// One widget, one control: the mini panel a value widget opens under
/// itself. Volume adds a mute toggle; brightness is just its slider.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SliderKind {
    Volume,
    Brightness,
}

impl SliderKind {
    fn title(self) -> &'static str {
        match self {
            SliderKind::Volume => "Volume",
            SliderKind::Brightness => "Brightness",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            SliderKind::Volume => "icons/volume.svg",
            SliderKind::Brightness => "icons/brightness.svg",
        }
    }

    fn percent(self, sysmon: &SysMon) -> Option<u8> {
        match self {
            SliderKind::Volume => sysmon.volume.map(|volume| volume.percent),
            SliderKind::Brightness => sysmon.brightness.map(|b| b.percent),
        }
    }
}

pub struct SliderPanelView {
    geometry: crate::panel::PanelGeometry,
    sysmon: Entity<SysMon>,
    kind: SliderKind,
    /// The track's bounds stash, live while the pointer drags the slider.
    dragging: Option<TrackStash>,
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
        Self {
            geometry,
            sysmon,
            kind,
            dragging: None,
        }
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
        });
    }
}

impl Render for SliderPanelView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sysmon = self.sysmon.read(cx);
        let volume = sysmon.volume;
        let percent = self.kind.percent(sysmon);
        let muted = volume.is_some_and(|volume| volume.muted);

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
                }
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.dragging = None;
                }),
            )
            .child(
                div()
                    .text_size(px(14.))
                    .text_color(rgb(TEXT))
                    .child(self.kind.title()),
            )
            .children(percent.map(|percent| {
                controls::slider_row(
                    kind.icon(),
                    if kind == SliderKind::Volume && muted {
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
                            });
                        }
                    }),
                )
            }))
            .when(kind == SliderKind::Volume, |el| {
                el.when_some(volume, |el, volume| {
                    el.child(
                        div()
                            .id("mute-toggle")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .text_size(px(11.))
                            .text_color(if volume.muted {
                                rgb(URGENT)
                            } else {
                                rgb(TEXT_DIM)
                            })
                            .cursor_pointer()
                            .hover(|el| el.bg(rgb(SURFACE)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sysmon
                                    .update(cx, |sysmon, cx| sysmon.request_mute_toggle(cx));
                            }))
                            .child(if volume.muted { "unmute" } else { "mute" }.to_string()),
                    )
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
