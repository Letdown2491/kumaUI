use gpui::{
    Context, Div, Entity, FocusHandle, Render, Window, div, prelude::*, px, rgb, rgba, size,
};

use crate::controls;
use crate::panel_kit as kit;
use crate::sysmon::{AccessPoint, SysMon};
use crate::theme::*;

/// The Wi-Fi panel: the radio toggle, the network we are on, and the
/// visible APs. Opens from the Internet widget; the scan rides the
/// poll's panel-open gate plus a scan-now on open.
pub struct WifiPanelView {
    geometry: crate::panel::PanelGeometry,
    sysmon: Entity<SysMon>,
    /// The password prompt's state: which secured unknown network it
    /// belongs to, the typed text, and the field's focus.
    password: Option<PasswordEdit>,
}

struct PasswordEdit {
    ssid: String,
    text: String,
    focus: FocusHandle,
}

impl WifiPanelView {
    pub fn new(
        sysmon: Entity<SysMon>,
        _window: &mut Window,
        cx: &mut Context<Self>,
        geometry: crate::panel::PanelGeometry,
    ) -> Self {
        cx.observe(&sysmon, |_, _, cx| cx.notify()).detach();
        sysmon.update(cx, |sysmon, cx| sysmon.scan_wifi_now(cx));
        Self {
            geometry,
            sysmon,
            password: None,
        }
    }

    /// One AP row's signal glyph: three runs of the feather arcs.
    fn signal_icon(strength: u8) -> &'static str {
        if strength >= 70 {
            "icons/wifi.svg"
        } else if strength >= 35 {
            "icons/wifi-mid.svg"
        } else {
            "icons/wifi-low.svg"
        }
    }

    /// A password prompt's key handling: Enter joins with what is
    /// typed, escape backs out, characters type silently.
    fn password_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(edit) = self.password.as_mut() else {
            return;
        };
        match event.keystroke.key.as_str() {
            "enter" => {
                let ssid = edit.ssid.clone();
                let password = std::mem::take(&mut edit.text);
                self.password = None;
                self.sysmon.update(cx, |sysmon, cx| {
                    sysmon.request_wifi_connect(ssid, Some(password), cx)
                });
            }
            "escape" => {
                self.password = None;
            }
            "backspace" => {
                edit.text.pop();
            }
            "space" => {
                edit.text.push(' ');
            }
            other if other.chars().count() == 1 && !event.keystroke.modifiers.modified() => {
                edit.text.push_str(other);
            }
            _ => return,
        }
        cx.notify();
    }

    fn ap_row(&mut self, point: &AccessPoint, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let ssid = point.ssid.clone();
        // a network nmcli can join blind (open, or remembered): press
        // connects. A secured stranger asks for its password instead.
        let joins_blind = point.known || !point.secured;
        let joining = self.sysmon.read(cx).connecting_ssid.as_ref() == Some(&ssid);

        let mut row = div()
            .id(gpui::SharedString::from(format!("ap-{}", ssid)))
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .rounded_lg()
            .bg(rgb(if point.active { SURFACE } else { INSET }))
            .hover(|el| el.bg(rgb(SURFACE_HOVER)))
            .child(
                gpui::svg()
                    .path(Self::signal_icon(point.strength))
                    .size(px(15.))
                    .text_color(rgb(if point.active { ACCENT } else { TEXT })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.5))
                    .text_color(rgb(if point.active { ACCENT } else { TEXT }))
                    .truncate()
                    .child(ssid.clone()),
            );
        if joining {
            row = row.child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(TEXT_DIM))
                    .child("joining..."),
            );
        } else {
            if point.known {
                row = row.child(
                    gpui::svg()
                        .path("icons/check.svg")
                        .size(px(12.))
                        .text_color(rgb(TEXT_DIM)),
                );
            }
            if point.secured {
                row = row.child(
                    gpui::svg()
                        .path("icons/key.svg")
                        .size(px(12.))
                        .text_color(rgb(TEXT_DIM)),
                );
            }
        }
        if joins_blind {
            row = row.on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.sysmon.update(cx, |sysmon, cx| {
                        sysmon.request_wifi_connect(ssid.clone(), None, cx)
                    });
                }),
            );
        } else {
            row = row.on_click(cx.listener(move |this, _, window, cx| {
                if this
                    .password
                    .as_ref()
                    .is_some_and(|edit| edit.ssid == ssid)
                {
                    return;
                }
                let focus = cx.focus_handle();
                focus.focus(window, cx);
                this.password = Some(PasswordEdit {
                    ssid: ssid.clone(),
                    text: String::new(),
                    focus,
                });
                cx.notify();
            }));
        }
        row
    }

    fn password_row(&self, edit: &PasswordEdit) -> gpui::Stateful<Div> {
        div()
            .id("wifi-password")
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .rounded_lg()
            .bg(rgb(INSET))
            .border_1()
            .border_color(rgb(ACCENT))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(TEXT_DIM))
                    .child(format!("Password for {}", edit.ssid)),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(12.5))
                    .text_color(rgb(TEXT))
                    .child("•".repeat(edit.text.chars().count())),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(TEXT_DIM))
                    .child("Enter to join, Esc to cancel"),
            )
    }

    /// measured-panel flow: the content height refines the panel
    /// height, so the list's length sets the surface's size.
    fn measured(
        &self,
        content: gpui::Div,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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

impl Render for WifiPanelView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let network = self.sysmon.read(cx).network.clone();
        let enabled = network.as_ref().is_some_and(|net| net.wifi_enabled);
        let sysmon_handle = self.sysmon.clone();

        let mut content = div()
            .flex()
            .flex_col()
            .pt(px(10.))
            .px(px(16.))
            .pb(px(12.))
            .gap_2()
            .child(kit::pane_header("Wi-Fi"))
            .child(controls::toggle_row(
                "wifi-radio-toggle",
                "icons/wifi.svg",
                "Wi-Fi radio",
                enabled,
                move |_, _, cx| {
                    sysmon_handle
                        .update(cx, |sysmon, cx| sysmon.request_wifi_toggle(cx));
                },
            ));

        let Some(network) = network else {
            content = content.child(kit::empty_state(
                "icons/wifi.svg",
                "Network unknown",
                "nmcli did not answer",
            ));
            return self.measured(content, window, cx);
        };

        if !enabled {
            content = content.child(kit::empty_state(
                "icons/wifi.svg",
                "Wi-Fi is off",
                "Turn the radio on to see networks",
            ));
            return self.measured(content, window, cx);
        }

        // the network we are on: pinned, named as connected, with the
        // forget affordance when it is also a saved connection
        if let Some(ssid) = &network.ssid {
            let saved_here = network.saved.contains(ssid);
            let sysmon_forget = self.sysmon.clone();
            let forget_ssid = ssid.clone();
            content = content.child(kit::card("wifi-connected").child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .child(
                        gpui::svg()
                            .path("icons/wifi.svg")
                            .size(px(16.))
                            .text_color(rgb(ACCENT)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.5))
                            .text_color(rgb(TEXT))
                            .truncate()
                            .child(ssid.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(TEXT_DIM))
                            .child("connected"),
                    )
                    .when(saved_here, |el| {
                        el.child(kit::icon_button(
                            "wifi-forget",
                            "icons/trash.svg",
                            kit::ButtonVariant::Ghost,
                            move |_, _, cx| {
                                sysmon_forget.update(cx, |sysmon, cx| {
                                    sysmon.request_wifi_forget(forget_ssid.clone(), cx)
                                });
                            },
                        ))
                    }),
            ));
        }

        let mut points = network.access_points.clone();
        points.sort_by(|a, b| b.strength.cmp(&a.strength));
        points.dedup_by(|a, b| a.ssid == b.ssid);

        content = content
            .child(div().h(px(1.)).w_full().bg(rgba(DIVIDER_SOFT)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child("Networks"),
                    )
                    .child(kit::button(
                        "wifi-rescan",
                        "Rescan",
                        Some("icons/refresh.svg"),
                        kit::ButtonVariant::Ghost,
                        cx.listener(|this, _, _, cx| {
                            this.sysmon.update(cx, |sysmon, cx| {
                                sysmon.request_wifi_rescan(cx)
                            });
                        }),
                    )),
            )
            .when_some(self.sysmon.read(cx).wifi_error.clone(), |el, error| {
                el.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(URGENT))
                        .child(error),
                )
            })
            .children(if points.is_empty() {
                vec![kit::empty_state(
                    "icons/wifi.svg",
                    "No networks found",
                    "Try a rescan",
                )
                .into_any_element()]
            } else {
                // ten rows visible, the rest scroll: a long list must
                // not stretch the panel to the screen's edge
                vec![div()
                    .id("wifi-networks-list")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .max_h(px(392.))
                    .overflow_y_scroll()
                    .children(
                        points
                            .iter()
                            .map(|point| self.ap_row(point, cx).into_any_element())
                            .collect::<Vec<_>>(),
                    )
                    .into_any_element()]
            });

        // the open password prompt rides under the list
        if let Some(edit) = &self.password {
            content = content.child(
                div()
                    .track_focus(&edit.focus.clone())
                    .on_key_down(cx.listener(Self::password_key))
                    .child(self.password_row(edit)),
            );
        }

        self.measured(content, window, cx)
    }
}
