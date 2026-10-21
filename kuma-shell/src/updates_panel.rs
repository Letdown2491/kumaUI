//! The system updates panel behind the chip: what moved, package by
//! package, with the versions each one rides between and the severity
//! `kuma` reports. Read-only on purpose: staging is `kuma update`'s
//! job, with its own authentication.

use std::time::Instant;

use gpui::{Context, Div, Entity, Render, SharedString, Window, div, prelude::*, px, rgb};

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
                .children(updates.iter().enumerate().map(|(ix, update)| update_row(ix, update)))
                .into_any_element()
        };

        let subtitle = match (checking, stale, &checked_ago) {
            (true, _, _) => "checking...".to_string(),
            (false, true, Some(ago)) => format!("last check {ago} ago, it failed"),
            (false, true, None) => "the last check failed".to_string(),
            (false, false, Some(ago)) => format!("checked {ago} ago"),
            (false, false, None) => String::new(),
        };

        let header = kit::pane_header("System updates").child(
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

/// One moved package: a flat row on the drawer (the launcher's fill
/// rule: inset at rest, no naked rows), the name and the severity chip
/// on the first line, the versions it rides between dim under.
fn update_row(ix: usize, update: &PackageUpdate) -> gpui::Stateful<Div> {
    let security = update.severity.as_deref() == Some("security");
    div()
        .id(SharedString::from(format!("update-{ix}")))
        .flex()
        .flex_col()
        .gap_0p5()
        .px_3()
        .py_2()
        .rounded_lg()
        .bg(rgb(crate::theme::current().inset))
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
                .when_some(update.severity.clone(), |el, severity| {
                    el.child(
                        div()
                            .px_1p5()
                            .py_px()
                            .rounded_sm()
                            .text_size(px(10.))
                            .text_color(rgb(if security {
                                crate::theme::current().accent_text
                            } else {
                                crate::theme::current().text_dim
                            }))
                            .bg(rgb(if security {
                                crate::theme::current().accent
                            } else {
                                crate::theme::current().panel_bg >> 8
                            }))
                            .child(severity),
                    )
                }),
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
