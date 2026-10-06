//! Shared interactive controls: the canvas-painted slider and its row, and
//! the toggle pill. Quick settings and the per-widget mini panels use these.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    App, Bounds, Canvas, Div, Pixels, SharedString, TextAlign, Window, canvas, div, prelude::*, px,
    quad, rgb, size, svg,
};


/// Per-frame track-bounds stash, shared between the canvas (writer) and the
/// drag handlers (reader).
pub type TrackStash = Rc<Cell<Option<Bounds<Pixels>>>>;

pub fn track_stash() -> TrackStash {
    Rc::new(Cell::new(None))
}

/// Pointer x → slider percent, computed from the stashed track bounds.
pub fn value_at(x: Pixels, bounds: Bounds<Pixels>) -> Option<u8> {
    if bounds.size.width <= px(0.) {
        return None;
    }
    let fraction = ((x - bounds.left()) / bounds.size.width).clamp(0., 1.);
    Some((fraction * 100.).round() as u8)
}

/// The slider itself: a canvas-painted track, fill, and knob. Bounds land in
/// the stash every frame (prepaint precedes dispatch), so drag math reads
/// them from the shared cell.
pub fn slider_track(value: u8, stash: TrackStash) -> Canvas<Bounds<Pixels>> {
    let stash_painting = stash.clone();
    let value = f32::from(value) / 100.;
    canvas(
        move |bounds, _, _| {
            stash_painting.set(Some(bounds));
            bounds
        },
        move |bounds, _, window, _| {
            let track_height = bounds.size.height;

            // track background
            window.paint_quad(quad(
                bounds,
                px(3.),
                rgb(crate::theme::current().surface),
                px(0.),
                gpui::transparent_black(),
                gpui::BorderStyle::default(),
            ));

            // fill up to the value
            let fill_width = (bounds.size.width * value).max(px(0.));
            window.paint_quad(quad(
                Bounds {
                    origin: bounds.origin,
                    size: size(fill_width, track_height),
                },
                px(3.),
                rgb(crate::theme::current().accent),
                px(0.),
                gpui::transparent_black(),
                gpui::BorderStyle::default(),
            ));

            // knob centered on the fill edge
            let knob = px(12.);
            window.paint_quad(quad(
                Bounds {
                    origin: gpui::point(
                        bounds.left() + fill_width - knob / 2.,
                        bounds.origin.y + (track_height - knob) / 2.,
                    ),
                    size: size(knob, knob),
                },
                px(6.),
                rgb(crate::theme::current().text),
                px(0.),
                gpui::transparent_black(),
                gpui::BorderStyle::default(),
            ));
        },
    )
    .h(px(6.))
    .flex_1()
    .cursor_pointer()
}

/// icon + track + live percentage, in one row
pub fn slider_row(
    icon_path: &'static str,
    icon_color: u32,
    percent: u8,
    track: Canvas<Bounds<Pixels>>,
) -> gpui::Stateful<Div> {
    div()
        .id(gpui::SharedString::from(format!("slider-row-{icon_path}")))
        .flex()
        .items_center()
        .gap_2()
        .py_1()
        .child(
            svg()
                .path(icon_path)
                .size(px(16.))
                .text_color(rgb(icon_color)),
        )
        .child(track)
        .child(
            div()
                .w(px(32.))
                .text_size(px(11.))
                .text_color(rgb(crate::theme::current().text_dim))
                .text_align(TextAlign::Right)
                .child(format!("{percent}%")),
        )
}

/// A settings toggle: card row with icon, label, and a switch on the right.
/// The switch is the state; the card is the hit target.
pub fn toggle_row(
    id: &str,
    icon_path: &'static str,
    label: &str,
    enabled: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<Div> {
    div()
        .id(SharedString::from(id))
        .flex()
        .items_center()
        .gap_2()
        .px_3p5()
        .py_2p5()
        .rounded_lg()
        .bg(rgb(crate::theme::current().surface))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(crate::theme::current().surface_hover)))
        .on_click(on_click)
        .child(
            svg()
                .path(icon_path)
                .size(px(16.))
                .text_color(rgb(if enabled { crate::theme::current().text } else { crate::theme::current().text_dim })),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(12.))
                .text_color(rgb(if enabled { crate::theme::current().text } else { crate::theme::current().text_dim }))
                .child(label.to_string()),
        )
        .child(toggle_switch(enabled))
}

/// The pill-with-knob switch on the right of a toggle row. Public for the
/// notification panel's inline DND row.
pub fn toggle_switch(enabled: bool) -> Div {
    let knob = px(12.);
    let track = px(30.);
    div()
        .w(track)
        .h(px(16.))
        .rounded_full()
        .bg(rgb(if enabled { crate::theme::current().accent } else { crate::theme::current().inset }))
        .border_1()
        .border_color(rgb(crate::theme::current().divider))
        .flex()
        .items_center()
        .when(enabled, |el| el.justify_end())
        .px_1()
        .child(div().size(knob).rounded_full().bg(rgb(if enabled {
            crate::theme::current().accent_text
        } else {
            crate::theme::current().text_dim
        })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_at_maps_track_positions_to_percents() {
        let bounds = Bounds {
            origin: gpui::point(px(100.), px(0.)),
            size: size(px(200.), px(6.)),
        };
        assert_eq!(value_at(px(100.), bounds), Some(0));
        assert_eq!(value_at(px(200.), bounds), Some(50));
        assert_eq!(value_at(px(300.), bounds), Some(100));
        // clamped on both sides
        assert_eq!(value_at(px(0.), bounds), Some(0));
        assert_eq!(value_at(px(400.), bounds), Some(100));
        // degenerate track is no value at all
        assert_eq!(value_at(px(200.), Bounds::default()), None);
    }
}
