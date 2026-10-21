//! The system updates panel behind the chip: what moved, package by
//! package, with the versions each one rides between and the severity
//! `kuma` reports. Read-only on purpose: staging is `kuma update`'s
//! job, with its own authentication.

use std::time::Instant;

use gpui::{AnyElement, Context, Div, Entity, Render, SharedString, Window, div, prelude::*, px, rgba, rgb};

use crate::panel::PanelGeometry;
use crate::panel_kit as kit;
use crate::updates::{PackageUpdate, UpdatesState};

pub struct UpdatesPanelView {
    geometry: PanelGeometry,
    state: Entity<UpdatesState>,
}

impl UpdatesPanelView {
    pub fn new(
        state: Entity<UpdatesState>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
        geometry: PanelGeometry,
    ) -> Self {
        Self { geometry, state }
    }
}

impl Render for UpdatesPanelView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let updates = state.updates.clone();
        let security = state.security;
        let stale = state.stale;
        let checking = state.checking;
        let applying = state.applying;
        let applied = state.applied.clone();
        let checked_ago = state.checked_at.map(|at| ago(at, Instant::now()));

        let list = if updates.is_empty() {
            kit::empty_state(
                "icons/updates.svg",
                if checking {
                    "Checking..."
                } else if stale {
                    "The last check failed"
                } else {
                    "Everything is current"
                },
                "kuma update --check asks the registry what has moved",
            )
            .into_any_element()
        } else {
            div()
                .id("update-list")
                .flex()
                .flex_col()
                .gap_1()
                .overflow_y_scroll()
                .flex_1()
                .min_h_0()
                .children(updates.iter().enumerate().flat_map(|(ix, update)| {
                    // the launcher's section rule: hairlines between the
                    // rows, never a box around one
                    let divider = div()
                        .id(SharedString::from(format!("update-rule-{ix}")))
                        .h(px(1.))
                        .bg(rgba(crate::theme::current().divider_soft))
                        .into_any_element();
                    let row = update_row(ix, update).into_any_element();
                    if ix == 0 {
                        vec![row]
                    } else {
                        vec![divider, row]
                    }
                }))
                .into_any_element()
        };

        let subtitle = match (checking, stale, &checked_ago) {
            (true, _, _) => "checking...".to_string(),
            (false, true, Some(ago)) => format!("last check {ago} ago, it failed"),
            (false, true, None) => "the last check failed".to_string(),
            (false, false, Some(ago)) => format!("checked {ago} ago"),
            (false, false, None) => String::new(),
        };

        let header = kit::pane_header("Kuma Updates").child(
            div()
                .text_size(px(11.))
                .text_color(rgb(crate::theme::current().text_dim))
                .child(SharedString::from(subtitle)),
        );

        let content = div()
            .flex()
            .flex_col()
            .pt(px(10.))
            .px(px(12.))
            .pb(px(12.))
            .gap_2()
            .size_full()
            .child(header)
            .child(list)
            .when(!updates.is_empty() && !applying, |el| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(crate::theme::current().text_dim))
                                .child(format!(
                                    "{security} of {} are security updates",
                                    updates.len()
                                )),
                        )
                        .child(kit::button(
                            "apply-updates",
                            "Update now",
                            Some("icons/updates.svg"),
                            kit::ButtonVariant::Primary,
                            {
                                let state = self.state.clone();
                                move |_, _, cx| {
                                    state.update(cx, |state, cx| state.apply(cx));
                                }
                            },
                        )),
                )
            })
            .when(applying, |el| {
                el.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(crate::theme::current().text_dim))
                        .child(
                            "Updating: pulling, rebuilding, and staging the image; this can take a while",
                        ),
                )
            })
            .when_some(applied, |el, result| match result {
                Ok(()) => el.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(crate::theme::current().text_dim))
                        .child("Staged. It applies at the next reboot."),
                ),
                Err(err) => el.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(crate::theme::URGENT))
                        .child(SharedString::from(format!("The update failed: {err}"))),
                ),
            });

        crate::panel::chrome(self.geometry, window, cx, content)
    }
}

/// One moved package: a naked row on the drawer under a hairline rule,
/// the name with its severity badge on the first line, the versions it
/// rides between dim under. The list arrives worst-first from `kuma`,
/// so the badge colors confirm what the position already says:
/// critical paints urgent red, important paints the accent (update
/// today), moderate and low share the quiet chip, and a null severity
/// (bugfix or enhancement, no security advisory) earns no badge at all.
fn update_row(_ix: usize, update: &PackageUpdate) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_0p5()
        .py_2()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    div()
                        .min_w_0()
                        .text_size(px(12.))
                        .text_color(rgb(crate::theme::current().text))
                        .truncate()
                        .child(update.name.clone()),
                )
                .child(severity_badge(update.severity.as_deref())),
        )
        .child(
            div()
                .text_size(px(10.))
                .text_color(rgb(crate::theme::current().text_dim))
                .truncate()
                .child(SharedString::from(format!(
                    "{} → {}",
                    update.from, update.to
                ))),
        )
}

/// The severity badge: glyphs, not words, one per level of kuma's
/// scale plus the no-advisory null, tinted by the same three visual
/// classes. Critical rings the bell (urgent red), important locks the
/// shield (the accent: a security advisory that matters), moderate
/// holds the plain shield and low the info dot, both dim, and a null
/// severity (bugfix, enhancement) earns no badge at all: the absence
/// of news is not a color.
fn severity_badge(severity: Option<&str>) -> AnyElement {
    let (icon, color) = match severity {
        Some("critical") => ("icons/alert.svg", crate::theme::URGENT),
        Some("important") => ("icons/shield-lock.svg", crate::theme::current().accent),
        Some("moderate") => ("icons/shield.svg", crate::theme::current().text_dim),
        // low, and any severity a future kuma adds: the quiet class
        Some(_) => ("icons/info.svg", crate::theme::current().text_dim),
        None => return div().into_any_element(),
    };
    gpui::svg()
        .path(icon)
        .size(px(14.))
        .text_color(rgb(color))
        .into_any_element()
}

/// Wall-clock "how long ago", coarse on purpose: the panel refreshes
/// when it opens, not on a clock.
fn ago(then: Instant, now: Instant) -> String {
    let secs = now.duration_since(then).as_secs();
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} minutes", secs / 60),
        3600..=86399 => format!("{} hours", secs / 3600),
        _ => format!("{} days", secs / 86400),
    }
}
