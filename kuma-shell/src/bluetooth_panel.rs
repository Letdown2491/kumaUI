use gpui::{Context, Div, Entity, Render, Window, div, prelude::*, px, rgb, rgba};

use crate::controls;
use crate::panel_kit as kit;
use crate::sysmon::{BluetoothAct, BluetoothDevice, SysMon};

/// The Bluetooth panel: the radio toggle, and every device the
/// controller knows, paired ones first. Opens from the bar widget;
/// the device scan rides the poll's panel-open gate plus a
/// scan-now on open.
pub struct BluetoothPanelView {
    geometry: crate::panel::PanelGeometry,
    sysmon: Entity<SysMon>,
    /// The unpair button's armed confirm: the mac waiting for its
    /// second click, one arm at a time, expiring like the kit's row.
    armed_remove: Option<String>,
    /// Bumped on every arm; a pending disarm timer only disarms the
    /// arm it was minted for.
    remove_arm: u64,
}

impl BluetoothPanelView {
    pub fn new(
        sysmon: Entity<SysMon>,
        _window: &mut Window,
        cx: &mut Context<Self>,
        geometry: crate::panel::PanelGeometry,
    ) -> Self {
        cx.observe(&sysmon, |_, _, cx| cx.notify()).detach();
        sysmon.update(cx, |sysmon, cx| sysmon.scan_bluetooth_now(cx));
        Self {
            geometry,
            sysmon,
            armed_remove: None,
            remove_arm: 0,
        }
    }

    /// Arm (or confirm) the unpair for one device: the first click
    /// arms (the trash swaps to the question, self-disarms in 5s),
    /// the second fires. The kit's arm-then-confirm, keyed to a
    /// dynamic row.
    fn remove_clicked(&mut self, mac: &str, cx: &mut Context<Self>) {
        if self.armed_remove.as_deref() == Some(mac) {
            let mac = mac.to_string();
            self.armed_remove = None;
            self.sysmon.update(cx, |sysmon, cx| {
                sysmon.request_bluetooth_act(BluetoothAct::Remove, mac, cx)
            });
            cx.notify();
        } else {
            self.armed_remove = Some(mac.to_string());
            self.remove_arm += 1;
            let stamp = self.remove_arm;
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(kit::CONFIRM_DISARM)
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if this.remove_arm == stamp {
                        this.armed_remove = None;
                        cx.notify();
                    }
                });
            })
            .detach();
            cx.notify();
        }
    }

    /// One device row: glyph, name, battery, state word, and the
    /// affordances its state earns. Press acts; a paired device's
    /// press toggles its connection, a nearby stranger's pairs.
    fn device_row(
        &mut self,
        device: &BluetoothDevice,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let mac = device.mac.clone();
        let busy = self.sysmon.read(cx).bt_busy.as_ref() == Some(&device.mac);
        let act = if device.paired {
            if device.connected {
                BluetoothAct::Disconnect
            } else {
                BluetoothAct::Connect
            }
        } else {
            BluetoothAct::Pair
        };
        let state_word = if busy {
            "working..."
        } else if device.connected {
            "connected"
        } else if device.paired {
            if device.trusted {
                "paired"
            } else {
                "paired, untrusted"
            }
        } else {
            "nearby"
        };

        let mut row = div()
            .id(gpui::SharedString::from(format!("bt-{}", device.mac)))
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .rounded_lg()
            .bg(rgb(if device.connected { crate::theme::current().surface } else { crate::theme::current().inset }))
            .hover(|el| el.bg(rgb(crate::theme::current().surface_hover)))
            .child(
                gpui::svg()
                    .path("icons/bluetooth.svg")
                    .size(px(15.))
                    .text_color(rgb(if device.connected { crate::theme::current().accent } else { crate::theme::current().text })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.5))
                    .text_color(rgb(if device.connected { crate::theme::current().accent } else { crate::theme::current().text }))
                    .truncate()
                    .child(device.alias.clone()),
            );
        if let Some(battery) = device.battery {
            row = row.child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(crate::theme::current().text_dim))
                    .child(format!("{battery}%")),
            );
        }
        row = row.child(
            div()
                .text_size(px(11.))
                .text_color(rgb(if device.connected { crate::theme::current().text } else { crate::theme::current().text_dim }))
                .child(state_word),
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            this.sysmon.update(cx, |sysmon, cx| {
                sysmon.request_bluetooth_act(act, mac.clone(), cx)
            });
        }));

        // the paired device's long-leave: remove the pairing entirely
        // (one mis-tap must not unpair a device: the kit's
        // arm-then-confirm, keyed to this row)
        if device.paired {
            let remove_mac = device.mac.clone();
            let armed = self.armed_remove.as_deref() == Some(device.mac.as_str());
            if armed {
                row = row.child(kit::button(
                    gpui::SharedString::from(format!("bt-remove-{}", device.mac)),
                    "Unpair this device?",
                    Some("icons/trash.svg"),
                    kit::ButtonVariant::Destructive,
                    cx.listener(move |this, _, _, cx| {
                        this.remove_clicked(&remove_mac, cx);
                    }),
                ));
            } else {
                row = row.child(kit::icon_button(
                    gpui::SharedString::from(format!("bt-remove-{}", device.mac)),
                    "icons/trash.svg",
                    kit::ButtonVariant::Ghost,
                    cx.listener(move |this, _, _, cx| {
                        this.remove_clicked(&remove_mac, cx);
                    }),
                ));
            }
        }
        row
    }

    /// measured-panel flow: the content height refines the panel
    /// height, so the device list's length sets the surface's size.
    fn measured(
        &self,
        content: Div,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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

impl Render for BluetoothPanelView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.sysmon.read(cx).bluetooth.clone();
        let enabled = state.as_ref().is_some_and(|state| state.enabled);
        let sysmon_handle = self.sysmon.clone();

        let mut content = div()
            .flex()
            .flex_col()
            .pt(px(10.))
            .px(px(16.))
            .pb(px(12.))
            .gap_2()
            .child(kit::pane_header("Bluetooth"))
            .child(controls::toggle_row(
                "bluetooth-radio-toggle",
                "icons/bluetooth.svg",
                "Bluetooth radio",
                enabled,
                move |_, _, cx| {
                    sysmon_handle.update(cx, |sysmon, cx| {
                        sysmon.request_bluetooth_toggle(cx)
                    });
                },
            ));

        let Some(state) = state else {
            content = content.child(kit::empty_state(
                "icons/bluetooth.svg",
                "Bluetooth unknown",
                "bluetoothctl did not answer",
            ));
            return self.measured(content, window, cx);
        };

        if !enabled {
            content = content.child(kit::empty_state(
                "icons/bluetooth.svg",
                "Bluetooth is off",
                "Turn the radio on to see devices",
            ));
            return self.measured(content, window, cx);
        }

        content = content
            .child(div().h(px(1.)).w_full().bg(rgba(crate::theme::current().divider_soft)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(crate::theme::current().text))
                            .child("Devices"),
                    )
                    .child(kit::button(
                        "bluetooth-scan",
                        "Scan",
                        Some("icons/search.svg"),
                        kit::ButtonVariant::Ghost,
                        cx.listener(|this, _, _, cx| {
                            this.sysmon.update(cx, |sysmon, cx| {
                                sysmon.request_bluetooth_discovery(cx)
                            });
                        }),
                    )),
            )
            .when_some(self.sysmon.read(cx).bt_error.clone(), |el, error| {
                el.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(crate::theme::URGENT))
                        .child(error),
                )
            })
            .children(if state.devices.is_empty() {
                vec![kit::empty_state(
                    "icons/bluetooth.svg",
                    "No devices found",
                    "Scan to find nearby devices",
                )
                .into_any_element()]
            } else {
                // ten rows visible, the rest scroll, the Wi-Fi list's
                // cap
                vec![div()
                    .id("bluetooth-device-list")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .max_h(px(392.))
                    .overflow_y_scroll()
                    .children(
                        state
                            .devices
                            .iter()
                            .map(|device| self.device_row(device, cx).into_any_element())
                            .collect::<Vec<_>>(),
                    )
                    .into_any_element()]
            });

        self.measured(content, window, cx)
    }
}
