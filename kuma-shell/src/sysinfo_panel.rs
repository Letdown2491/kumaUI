use gpui::{Context, Entity, Render, Window, div, prelude::*, px, rgb, size};

use crate::panel_kit as kit;
use crate::sysmon::SysMon;

/// Which sysinfo mini panel to render: one view, four tunings. The
/// bar's CPU, RAM, temperature, and disk widgets all open one of
/// these under themselves: the same 60-sample history the widget
/// reads, drawn big, and the cheap stats.
#[derive(Clone, Copy, PartialEq)]
pub enum SysPanel {
    Cpu,
    Ram,
    Temp,
    Disk,
    Battery,
}

impl SysPanel {
    fn title(self) -> &'static str {
        match self {
            SysPanel::Cpu => "CPU",
            SysPanel::Ram => "Memory",
            SysPanel::Temp => "Temperature",
            SysPanel::Disk => "Disk",
            SysPanel::Battery => "Battery",
        }
    }
}

/// The mini panel: the history sparkline, a caption line, and the
/// stats. Stats sit in rows of two, the first cell hugging left, the
/// second a fixed-width column with right-aligned text so both rows'
/// labels and values line up on their ends.
pub struct SysPanelView {
    which: SysPanel,
    geometry: crate::panel::PanelGeometry,
    sysmon: Entity<SysMon>,
}

impl SysPanelView {
    pub fn new(
        which: SysPanel,
        sysmon: Entity<SysMon>,
        _window: &mut Window,
        cx: &mut Context<Self>,
        geometry: crate::panel::PanelGeometry,
    ) -> Self {
        cx.observe(&sysmon, |_, _, cx| cx.notify()).detach();
        Self {
            which,
            geometry,
            sysmon,
        }
    }

    fn history(&self, cx: &Context<Self>) -> Vec<u32> {
        let sysmon = self.sysmon.read(cx);
        match self.which {
            SysPanel::Cpu => sysmon
                .cpu_history
                .iter()
                .map(|usage| (usage.clamp(0.0, 1.0) * 100.0) as u32)
                .collect(),
            SysPanel::Ram => sysmon
                .ram_history
                .iter()
                .map(|percent| u32::from(*percent))
                .collect(),
            SysPanel::Temp => sysmon
                .temp_history
                .iter()
                .map(|sample| *sample)
                .collect(),
            SysPanel::Disk => sysmon
                .disk_history
                .iter()
                .map(|sample| *sample)
                .collect(),
            SysPanel::Battery => sysmon
                .battery_history
                .iter()
                .map(|sample| *sample)
                .collect(),
        }
    }

    fn percent(&self, cx: &Context<Self>) -> Option<u32> {
        let sysmon = self.sysmon.read(cx);
        match self.which {
            SysPanel::Cpu => sysmon.cpu.map(|usage| (usage * 100.0).round() as u32),
            SysPanel::Ram => sysmon.ram.as_ref().map(|ram| u32::from(ram.percent)),
            SysPanel::Disk => sysmon.disk.as_ref().map(|disk| u32::from(disk.percent)),
            SysPanel::Temp => sysmon.temp,
            SysPanel::Battery => sysmon
                .battery
                .as_ref()
                .map(|battery| u32::from(battery.percent)),
        }
    }

    /// The stats block: rows of two (label, value, right-aligned?).
    fn stats(&self, cx: &Context<Self>) -> gpui::Div {
        let sysmon = self.sysmon.read(cx);
        let stat = |label: &str, value: String, right: bool| {
            let align = if right {
                gpui::TextAlign::Right
            } else {
                gpui::TextAlign::Left
            };
            let cell = div()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(crate::theme::current().text_dim))
                        .text_align(align)
                        .child(label.to_string()),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(crate::theme::current().text))
                        .text_align(align)
                        .child(value),
                );
            if right {
                cell.w(px(96.))
            } else {
                cell
            }
        };
        let gib = |mib: u64| format!("{:.1} GiB", mib as f32 / 1024.);
        let row = |left, right| {
            div().flex().justify_between().child(left).child(right)
        };
        match self.which {
            SysPanel::Cpu => {
                let load = match sysmon.loadavg {
                    Some((loads, procs)) => format!(
                        "{:.2} {:.2} {:.2} ({} procs)",
                        loads[0], loads[1], loads[2], procs
                    ),
                    None => "n/a".into(),
                };
                let uptime = sysmon
                    .uptime
                    .map(|secs| {
                        let days = secs / 86_400;
                        let hours = (secs % 86_400) / 3600;
                        let minutes = (secs % 3600) / 60;
                        if days > 0 {
                            format!("{days}d {hours}h")
                        } else if hours > 0 {
                            format!("{hours}h {minutes}m")
                        } else {
                            format!("{minutes}m")
                        }
                    })
                    .unwrap_or_else(|| "n/a".into());
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(row(
                        stat(
                            "Temperature",
                            sysmon
                                .temp
                                .map(|temp| format!("{temp}°C"))
                                .unwrap_or_else(|| "n/a".into()),
                            false,
                        ),
                        stat("Uptime", uptime, true),
                    ))
                    .child(row(
                        stat("Load (1/5/15m)", load, false),
                        stat("Processes", sysmon.loadavg.map(|(_, procs)| procs).unwrap_or(0).to_string(), true),
                    ))
            }
            SysPanel::Ram => {
                let (used, total) = match sysmon.ram {
                    Some(ram) => (gib(ram.used_mib), gib(ram.total_mib)),
                    None => ("n/a".into(), "n/a".into()),
                };
                div().flex().flex_col().gap_2().child(row(
                    stat("Used", used, false),
                    stat("Total", total, true),
                ))
            }
            SysPanel::Temp => {
                let peak = self
                    .history(cx)
                    .iter()
                    .max()
                    .map(|peak| format!("{peak}°C"))
                    .unwrap_or_else(|| "n/a".into());
                div().flex().flex_col().gap_2().child(row(
                    stat(
                        "Now",
                        sysmon
                            .temp
                            .map(|temp| format!("{temp}°C"))
                            .unwrap_or_else(|| "n/a".into()),
                        false,
                    ),
                    stat("Recent peak", peak, true),
                ))
            }
            SysPanel::Disk => {
                let (used, total) = match sysmon.disk.clone() {
                    Some(disk) => (gib(disk.used_mib), gib(disk.total_mib)),
                    None => ("n/a".into(), "n/a".into()),
                };
                let mount = sysmon
                    .disk
                    .clone()
                    .map(|disk| disk.mount)
                    .unwrap_or_else(|| "n/a".into());
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(row(
                        stat("Used", used, false),
                        stat("Total", total, true),
                    ))
                    .child(row(stat("Mount", mount, false), div()))
            }
            SysPanel::Battery => {
                let status = match sysmon.battery {
                    Some(battery) => {
                        if battery.percent >= 100 {
                            "full"
                        } else if battery.charging {
                            "charging"
                        } else if battery.on_ac {
                            "on AC (held)"
                        } else {
                            "discharging"
                        }
                    }
                    None => "n/a",
                };
                div().flex().flex_col().gap_2().child(row(
                    stat(
                        "Charge",
                        sysmon
                            .battery
                            .map(|battery| format!("{}%", battery.percent))
                            .unwrap_or_else(|| "n/a".into()),
                        false,
                    ),
                    stat("Status", status.to_string(), true),
                ))
            }
        }
    }
}

impl Render for SysPanelView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let history: Vec<u32> = self.history(cx);
        let percent = self.percent(cx);
        let stats = self.stats(cx);

        // the history, drawn big: fixed-width bars in a fixed-height
        // strip, newest rightmost
        let spark: Vec<_> = history
            .iter()
            .map(|sample| {
                div()
                    .w(px(4.))
                    .mr(px(1.))
                    .h(px(((*sample as f32 / 100.0).max(0.04) * 48.0) as f32))
                    .rounded_sm()
                    .bg(rgb(if *sample >= 85 { crate::theme::URGENT } else { crate::theme::current().accent }))
            })
            .collect();
        let spark = div()
            .id("sys-panel-spark")
            .flex()
            .items_end()
            .h(px(48.))
            .rounded_md()
            .bg(rgb(crate::theme::current().inset))
            .px(px(4.))
            .py(px(4.))
            .overflow_hidden()
            .child(div().flex().items_end().h_full().children(spark));

        let caption = match self.which {
            SysPanel::Cpu => percent.map(|percent| format!("{percent}% total")),
            SysPanel::Ram | SysPanel::Disk => {
                percent.map(|percent| format!("{percent}% used"))
            }
            SysPanel::Temp => percent.map(|temp| format!("{temp}°C now")),
            SysPanel::Battery => percent.map(|percent| format!("{percent}% charge")),
        };

        let content = div()
            .flex()
            .flex_col()
            .pt(px(10.))
            .px(px(16.))
            .pb(px(14.))
            .gap_2()
            .child(kit::pane_header(self.which.title()))
            .child(spark)
            .children(caption.map(|caption| {
                div()
                    .text_size(px(12.))
                    .text_color(rgb(crate::theme::current().text))
                    .child(caption)
            }))
            .child(stats);

        // the height is measured, not fixed: content is intrinsic
        // height (no size_full) and its laid-out height refines the
        // geometry each frame (the calendar's convergence), so a
        // one-row stats block makes a shorter panel than a two-row
        // one, with the same bottom padding under both
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
