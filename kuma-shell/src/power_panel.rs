use gpui::{Context, Entity, Render, SharedString, Window, div, prelude::*, px, rgb, svg};

use crate::panel_kit as kit;
use crate::sysmon::{PROFILES, SysMon};

/// The power profile popup: the mini panel the Power profile bar widget
/// opens under itself. One row per profile, the active one checked;
/// clicking a row requests it and the optimistic write carries the
/// rollback plus next-poll reconciliation (ADR-0009's request seam).
pub struct PowerProfileView {
    geometry: crate::panel::PanelGeometry,
    sysmon: Entity<SysMon>,
}

impl PowerProfileView {
    pub fn new(
        sysmon: Entity<SysMon>,
        _window: &mut Window,
        cx: &mut Context<Self>,
        geometry: crate::panel::PanelGeometry,
    ) -> Self {
        cx.observe(&sysmon, |_, _, cx| cx.notify()).detach();
        Self { geometry, sysmon }
    }
}

impl Render for PowerProfileView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.sysmon.read(cx).power_profile;

        let rows = div().flex().flex_col().gap_1().children(
            PROFILES.iter().map(|&profile| {
                let active = current == Some(profile);
                div()
                    .id(SharedString::from(format!("profile-{}", profile.as_str())))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(rgb(if active { crate::theme::current().surface } else { crate::theme::current().inset }))
                    .hover(|style| style.bg(rgb(crate::theme::current().surface)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.sysmon
                            .update(cx, |sysmon, cx| sysmon.request_power_profile(profile, cx));
                    }))
                    .child(
                        svg()
                            .path("icons/power-profile.svg")
                            .size(px(14.))
                            .text_color(rgb(if active { crate::theme::current().accent } else { crate::theme::current().text_dim })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.))
                            .text_color(rgb(if active { crate::theme::current().text } else { crate::theme::current().text_dim }))
                            .child(profile.title()),
                    )
                    .when(active, |el| {
                        el.child(
                            svg()
                                .path("icons/check.svg")
                                .size(px(12.))
                                .text_color(rgb(crate::theme::current().accent)),
                        )
                    })
            }),
        );

        let content = div()
            .flex()
            .flex_col()
            .pt(px(10.))
            .px(px(16.))
            .pb(px(12.))
            .gap_2()
            .child(kit::pane_header("Power profile"))
            .child(rows);

        crate::panel::chrome(self.geometry, window, content)
    }
}
