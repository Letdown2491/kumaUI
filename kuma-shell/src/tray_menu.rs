//! The tray menu: a tray item's DBusMenu rendered as a small panel, in
//! the dock menu's plain-card language (menus are transient, not
//! drawers). The tree arrives whole at open time (`tray.rs`'s
//! GetLayout fetch), so submenu navigation is state-side pushes and
//! pops; a click speaks Event and closes the panel unless the entry is
//! a toggle (checkmark/radio), which stays open to show its new state.

use gpui::{
    div, img, px, rgb, svg, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement as _, Styled, Window,
};

use crate::panel_kit as kit;

pub struct TrayMenuView {
    tray: Option<Entity<crate::tray::TrayState>>,
    geometry: crate::panel::PanelGeometry,
}

impl TrayMenuView {
    pub fn new(
        tray: Option<Entity<crate::tray::TrayState>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
        geometry: crate::panel::PanelGeometry,
    ) -> Self {
        if let Some(tray) = &tray {
            // the tree lands (and refreshes) on the update channel
            cx.observe(tray, |_, _, cx| cx.notify()).detach();
        }
        Self { tray, geometry }
    }

    /// The menu card: loading while the fetch is in flight, the gone
    /// state when the fetch answered empty, otherwise the entries at
    /// the current navigation level (the back row first).
    fn menu_content(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let theme = crate::theme::current();
        let Some(tray) = &self.tray else {
            return kit::empty_state("icons/bar.svg", "No tray", "The tray is not running")
                .into_any_element();
        };
        let state = tray.read(cx);
        if state.menu_loading() {
            return div()
                .px_2()
                .py_1p5()
                .text_size(px(11.))
                .text_color(rgb(theme.text_dim))
                .child("loading the menu…")
                .into_any_element();
        }
        let entries: Vec<crate::tray::MenuItem> = state.menu_entries().to_vec();
        if entries.is_empty() {
            return kit::empty_state(
                "icons/bar.svg",
                "No menu",
                "The item's menu did not answer",
            )
            .into_any_element();
        }
        let depth = state.menu_depth();
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        if depth > 0 {
            let tray = tray.clone();
            rows.push(
                div()
                    .id("tray-menu-back")
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|el| el.bg(rgb(theme.surface)))
                    .text_size(px(12.))
                    .text_color(rgb(theme.text_dim))
                    .on_click(move |_, _, cx| {
                        tray.update(cx, |state, cx| {
                            state.menu_back();
                            cx.notify();
                        });
                    })
                    .child("‹ Back")
                    .into_any_element(),
            );
        }
        for entry in entries {
            if !entry.visible {
                continue;
            }
            if entry.separator {
                rows.push(
                    div()
                        .id(SharedString::from(format!("tray-menu-sep-{}", entry.id)))
                        .h(px(1.))
                        .mx_1()
                        .my_1()
                        .bg(gpui::rgba(crate::theme::current().divider_soft))
                        .into_any_element(),
                );
                continue;
            }
            let tray = tray.clone();
            let clickable = entry.clickable();
            let submenu = entry.is_submenu();
            let toggle = entry.toggle;
            let mut row = div()
                .id(SharedString::from(format!("tray-menu-{}", entry.id)))
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1p5()
                .rounded_md();
            if clickable || submenu {
                row = row.cursor_pointer().hover(|el| el.bg(rgb(theme.surface)));
            }
            if submenu {
                let tray = tray.clone();
                row = row.on_click(move |_, _, cx| {
                    tray.update(cx, |state, cx| {
                        state.menu_enter(entry.id);
                        cx.notify();
                    });
                });
            } else if clickable {
                // a toggle stays open to show its new state; a plain
                // leaf speaks Event and the panel closes
                let stays_open = toggle.is_some();
                row = row.on_click(move |_, _, cx| {
                    tray.update(cx, |state, _| state.menu_click(entry.id));
                    if !stays_open {
                        crate::panel::close_panels(cx);
                    }
                });
            }
            row = row
                .text_size(px(12.))
                .text_color(rgb(if entry.enabled { theme.text } else { theme.text_dim }))
                .child(row_icon(&entry.icon_name))
                // the label: an empty one still gives the row its height
                .child(
                    div().flex_1().truncate().child(if entry.label.is_empty() {
                        " ".to_string()
                    } else {
                        entry.label.clone()
                    }),
                )
                .child(toggle_mark(toggle));
            rows.push(row.into_any_element());
        }
        div()
            .id("tray-menu")
            .flex()
            .flex_col()
            .w_full()
            .max_h(px(480.))
            .overflow_y_scroll()
            .p(px(4.))
            .children(rows)
            .into_any_element()
    }
}

/// The entry's icon at row size: named icons resolve through the image
/// registry (the tray icon's own path); nothing named renders nothing.
fn row_icon(icon_name: &str) -> gpui::AnyElement {
    let size = px(14.);
    let resolved = if icon_name.is_empty() {
        None
    } else {
        crate::imaging::resolve(icon_name, 16)
    };
    match resolved.as_deref() {
        Some(crate::imaging::IconImage::Raster(raster)) => {
            img(gpui::ImageSource::Render(raster.clone()))
                .size(size)
                .into_any_element()
        }
        Some(crate::imaging::IconImage::Svg(bytes)) => {
            svg().data(bytes.as_ref()).size(size).into_any_element()
        }
        None => div().w(px(14.)).into_any_element(),
    }
}

/// The right-side toggle slot: the mark when the state is on, the
/// glyph dimmed when unknown, nothing when off.
fn toggle_mark(toggle: Option<(crate::tray::ToggleType, crate::tray::ToggleState)>) -> gpui::AnyElement {
    let theme = crate::theme::current();
    let (glyph, color) = match toggle {
        Some((_, crate::tray::ToggleState::On)) => (
            match toggle.map(|(kind, _)| kind) {
                Some(crate::tray::ToggleType::Radio) => "●",
                _ => "✓",
            },
            rgb(theme.text),
        ),
        Some((_, crate::tray::ToggleState::Unknown)) => ("✓", rgb(theme.text_dim)),
        _ => ("", rgb(theme.text_dim)),
    };
    div().w(px(14.)).text_size(px(11.)).text_color(color).child(glyph).into_any_element()
}

impl Render for TrayMenuView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = self.menu_content(cx);
        // measured-panel flow: the content height refines the panel
        // height; the surface itself is never resized (it spans the
        // output, and the drawer reads this height at render)
        let view = cx.weak_entity();
        let measured = crate::panel::MeasureHeight::new(content, move |height, _window, cx| {
            let Some(view) = view.upgrade() else {
                return;
            };
            view.update(cx, |this, cx| {
                if (this.geometry.height - height).abs() > 0.5 {
                    this.geometry.height = height;
                    cx.notify();
                }
            });
        });
        crate::panel::chrome(self.geometry, window, cx, measured)
    }
}
