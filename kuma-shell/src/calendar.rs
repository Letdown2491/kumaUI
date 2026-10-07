use chrono::{Datelike, Local, NaiveDate};
use gpui::{App, Context, Div, Render, SharedString, Window, div, prelude::*, px, rgb};

use crate::panel_kit as kit;

/// A month view. `month_offset` navigates away from the month containing
/// today; today's date is highlighted wherever it actually falls. The panel's
/// height is measured, not fixed: the view refines `geometry.height` from the
/// laid-out grid (5 vs 6 rows, month header) and resizes the surface.
pub struct CalendarView {
    geometry: crate::panel::PanelGeometry,
    month_offset: i32,
}

impl CalendarView {
    pub fn new(
        _window: &mut Window,
        _cx: &mut Context<Self>,
        geometry: crate::panel::PanelGeometry,
    ) -> Self {
        Self {
            geometry,
            month_offset: 0,
        }
    }

    fn today() -> NaiveDate {
        Local::now().date_naive()
    }

    /// (year, month) shifted by whole months without chrono's
    /// overflow-on-month-end traps: arithmetic on the (year, month) pair.
    fn shifted(&self) -> (i32, u32) {
        let today = Self::today();
        let total = today.year() * 12 + today.month() as i32 - 1 + self.month_offset;
        (total.div_euclid(12), (total.rem_euclid(12) as u32) + 1)
    }

    fn step(&mut self, delta: i32, cx: &mut Context<Self>) {
        self.month_offset += delta;
        cx.notify();
    }
}

/// Monday-first weekday initials.
const WEEKDAYS: [&str; 7] = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];

impl Render for CalendarView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let today = Self::today();
        let (year, month) = self.shifted();
        let first = NaiveDate::from_ymd_opt(year, month, 1).expect("first of a real month");
        // Monday-first column of the 1st
        let lead = first.weekday().num_days_from_monday() as usize;
        let days: Vec<Option<NaiveDate>> = (0..lead)
            .map(|_| None::<NaiveDate>)
            .chain(
                (1..=31)
                    .filter_map(|day| NaiveDate::from_ymd_opt(year, month, day))
                    .map(Some),
            )
            .collect();

        let header = format!("{} {}", month_name(month), year);

        // content is intrinsic-height (no size_full): the measurement below
        // is the sum of its rows, which becomes the panel height
        let content = div()
            .flex()
            .flex_col()
            .pt(px(10.))
            .px(px(16.))
            .pb(px(14.))
            .gap_2()
            .child(
                kit::pane_header(&header).child(
                    div()
                        .flex()
                        .gap_1()
                        .child(nav_button(
                            "cal-prev",
                            "icons/chevron-left.svg",
                            cx.listener(|this, _, _, cx| this.step(-1, cx)),
                        ))
                        .child(nav_button(
                            "cal-next",
                            "icons/chevron-right.svg",
                            cx.listener(|this, _, _, cx| this.step(1, cx)),
                        )),
                ),
            )
            .child(weekday_row())
            .child(
                div()
                    .grid()
                    .grid_cols(7)
                    .gap_0p5()
                    .children(days.into_iter().map(|date| match date {
                        None => div().h(px(36.)).into_any_element(),
                        Some(date) => day_cell(date, date == today).into_any_element(),
                    })),
            );

        // report the laid-out content height back into this view's
        // geometry; converges when the delta is sub-pixel. The surface
        // is never resized: it spans the output, and the drawer reads
        // this height at render.
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

fn month_name(month: u32) -> &'static str {
    match month {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        _ => "December",
    }
}

/// One month-step button: a chevron glyph that hovers, the kit's ghost
/// affordance in a size the header can wear.
fn nav_button(
    id: &str,
    icon: &'static str,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<Div> {
    div()
        .id(SharedString::from(id))
        .px_2()
        .py_0p5()
        .rounded_sm()
        .cursor_pointer()
        .hover(|el| el.bg(rgb(crate::theme::current().surface)))
        .on_click(on_click)
        .child(
            gpui::svg()
                .path(icon)
                .size(px(14.))
                .text_color(rgb(crate::theme::current().text_dim)),
        )
}

fn weekday_row() -> Div {
    div()
        .grid()
        .grid_cols(7)
        .gap_0p5()
        .children(WEEKDAYS.map(|name| {
            div()
                .h(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(11.))
                .text_color(rgb(crate::theme::current().text_dim))
                .child(name.to_string())
        }))
}

fn day_cell(date: NaiveDate, is_today: bool) -> gpui::Stateful<Div> {
    let day = date.day().to_string();
    let cell = div()
        .id(SharedString::from(format!("day-{}", date)))
        .h(px(36.))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(12.))
        .text_color(if is_today {
            rgb(crate::theme::current().accent_text)
        } else {
            rgb(crate::theme::current().text)
        })
        .child(day);
    if is_today {
        // fill the whole cell and round it into the oval badge
        cell.bg(rgb(crate::theme::current().accent)).rounded_full()
    } else {
        cell.hover(|el| el.bg(rgb(crate::theme::current().surface_hover)))
    }
}
