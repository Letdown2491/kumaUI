//! The spike's one view: a CPU graph over the last two minutes, the
//! memory line, a sortable header, and the joined process list. The
//! look is the house's: the shell's published palette applied live,
//! the wallpaper deciding the colors.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use gpui::{ClickEvent, Context, Div, IntoElement, Render, Stateful, UniformListScrollHandle, Window, div, px, uniform_list, prelude::*};

use crate::niri;
use crate::sample::{self, ProcRow, Sampler, SortKey};
use crate::theme;

/// Two minutes of history at the 1s tick.
const HISTORY: usize = 120;

/// The window column's width, matching its header cell.
const WINDOW_COL: f32 = 280.;

pub(crate) struct MonitorView {
    sampler: Sampler,
    history: Vec<f32>,
    rows: Vec<ProcRow>,
    sort: SortKey,
    mem_used: u64,
    mem_total: u64,
    palette_mtime: Option<SystemTime>,
    scroll: UniformListScrollHandle,
}

impl MonitorView {
    pub(crate) fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            sampler: Sampler::new(),
            history: Vec::new(),
            rows: Vec::new(),
            sort: SortKey::Cpu,
            mem_used: 0,
            mem_total: 0,
            palette_mtime: None,
            scroll: UniformListScrollHandle::new(),
        };
        view.sync_palette();
        view.beat(cx);

        // The tick: sample, land, wait a second, again. The bar's
        // loop shape (kuma-shell/src/bar.rs): update-or-die, then the
        // executor's timer. A beat's failures degrade that beat, not
        // the loop: the sampler keeps its deltas, niri's join drops
        // to None for a beat and comes back.
        cx.spawn(async move |this, cx| {
            loop {
                let alive = this
                    .update(cx, |this, cx| {
                        this.beat(cx);
                    })
                    .is_ok();
                if !alive {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(1)).await;
            }
        })
        .detach();
        view
    }

    /// The shell's palette handshake: apply when the file's mtime
    /// moved (wallpaper changed, accent picked); a vanished file
    /// reverts to the built-ins once.
    fn sync_palette(&mut self) {
        let mtime = palette_mtime();
        if mtime == self.palette_mtime {
            return;
        }
        self.palette_mtime = mtime;
        theme::apply_palette(&read_palette());
    }

    fn beat(&mut self, cx: &mut Context<Self>) {
        self.sync_palette();
        let windows = niri::windows().unwrap_or_else(|err| {
            log::debug!("niri join unavailable this beat: {err:#}");
            Vec::new()
        });
        let snap = self.sampler.beat(&windows);
        self.history.push(snap.cpu_total);
        if self.history.len() > HISTORY {
            self.history.remove(0);
        }
        self.mem_used = snap.mem_used;
        self.mem_total = snap.mem_total;
        self.rows = snap.rows;
        sample::sort_rows(&mut self.rows, self.sort);
        cx.notify();
    }

    fn set_sort(&mut self, key: SortKey, cx: &mut Context<Self>) {
        self.sort = key;
        sample::sort_rows(&mut self.rows, key);
        cx.notify();
    }
}

impl Render for MonitorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cpu_now = self.history.last().copied().unwrap_or(0.);
        let rows = self.rows.clone();
        let count = rows.len();

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::bg())
            .text_color(theme::text())
            .text_size(px(13.))
            .child(self.graph_panel(cpu_now))
            .child(self.header(cx))
            .child(
                // the wrapper is an explicit flex column and the list
                // grows into it: an Auto-sized deferred element is a
                // zero-content div to taffy and paints nothing without
                // a height (the quicklook list's own note)
                div()
                    .flex_1()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .child(
                        uniform_list("proc-rows", count, move |range, _, _| {
                            rows[range.clone()].iter().map(row_view).collect()
                        })
                        .flex_1()
                        .track_scroll(&self.scroll),
                    ),
            )
    }
}

impl MonitorView {
    /// The top strip: totals on one line (dim labels, bright values),
    /// the two-minute bar history under them.
    fn graph_panel(&self, cpu_now: f32) -> Div {
        let bars = self
            .history
            .iter()
            .map(|pct| {
                let height = (pct / 100. * 34.).clamp(1.5, 34.);
                div()
                    .flex_1()
                    .h(px(height))
                    .min_w(px(1.))
                    .bg(theme::accent())
            })
            .collect::<Vec<_>>();

        div()
            .px_3()
            .pt_3()
            .pb_2()
            .flex()
            .flex_col()
            .gap_2()
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .gap_2()
                    .text_color(theme::text_dim())
                    .child(div().child("CPU").text_color(theme::text_dim()))
                    .child(format!("{cpu_now:.1}%"))
                    .child(div().flex_1())
                    .child("MEM")
                    .child(format!(
                        "{} / {}",
                        sample::human_mem(self.mem_used),
                        sample::human_mem(self.mem_total)
                    )),
            )
            .child(div().h(px(36.)).flex().items_end().gap(px(1.)).children(bars))
    }

    /// The column titles. The sortable ones are clicks (the active
    /// one wears the accent); WINDOW has no sort of its own yet and
    /// stays an honest label.
    fn header(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .items_center()
            .px_1()
            .border_b_1()
            .border_color(theme::border())
            .child(self.sort_cell("col-pid", "PID", SortKey::Pid, Some(60.), cx))
            .child(self.sort_cell("col-name", "NAME", SortKey::Name, None, cx))
            .child(
                div()
                    .w(px(WINDOW_COL))
                    .py_1()
                    .px_2()
                    .text_size(px(11.))
                    .text_color(theme::text_dim())
                    .child("WINDOW"),
            )
            .child(self.sort_cell("col-cpu", "CPU%", SortKey::Cpu, Some(72.), cx))
            .child(self.sort_cell("col-mem", "MEM", SortKey::Mem, Some(100.), cx))
    }

    fn sort_cell(
        &self,
        id: &'static str,
        label: &'static str,
        key: SortKey,
        width: Option<f32>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let active = self.sort == key;
        let cell = div()
            .id(id)
            .py_1()
            .px_2()
            .text_size(px(11.))
            .text_color(if active {
                theme::accent()
            } else {
                theme::text_dim()
            })
            .on_click(cx.listener(move |this, _click: &ClickEvent, _, cx| {
                this.set_sort(key, cx);
            }))
            .child(label);
        match width {
            Some(w) => cell.w(px(w)),
            None => cell.flex_1(),
        }
    }
}

/// One table row: numbers right-set in their columns, the window
/// column carrying app_id (bright) and title (dim), the focused
/// window's row wearing the palette's selection color.
fn row_view(row: &ProcRow) -> Div {
    let focused = row.window.as_ref().is_some_and(|w| w.focused);
    let base = div()
        .h(px(24.))
        .flex()
        .items_center()
        .px_1()
        .bg(if focused {
            theme::row_selected()
        } else if row.pid % 2 == 0 {
            theme::row()
        } else {
            theme::clear()
        });

    let name_cell = div().flex_1().truncate().px_2().child(row.name.clone());

    let window_cell = match &row.window {
        Some(window) => div()
            .w(px(WINDOW_COL))
            .flex()
            .gap_1()
            .px_2()
            .truncate()
            .child(
                div()
                    .truncate()
                    .text_color(if focused {
                        theme::text()
                    } else {
                        theme::text_dim()
                    })
                    .child(window.app_id.clone().unwrap_or_else(|| "?".into())),
            )
            .child(
                div()
                    .truncate()
                    .text_color(theme::text_dim())
                    .child(format!("· {}", window.title)),
            ),
        None => div().w(px(WINDOW_COL)).px_2().text_color(theme::text_dim()).child("-"),
    };

    base.child(div().w(px(60.)).flex().justify_end().text_color(theme::text_dim()).child(row.pid.to_string()))
        .child(name_cell)
        .child(window_cell)
        .child(
            div()
                .w(px(72.))
                .flex()
                .justify_end()
                .child(format!("{:>6.1}", row.cpu)),
        )
        .child(
            div()
                .w(px(100.))
                .flex()
                .justify_end()
                .text_color(theme::text_dim())
                .child(sample::human_mem(row.mem)),
        )
}

// ---------- the shell's palette handshake (from kuma-files) ----------

fn palette_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("kuma-shell/palette"))
}

fn palette_mtime() -> Option<SystemTime> {
    std::fs::metadata(palette_path()?).ok()?.modified().ok()
}

/// Read the shell's published palette, or the built-ins when absent.
fn read_palette() -> theme::Palette {
    let Some(path) = palette_path() else {
        return theme::Palette::default();
    };
    match std::fs::read_to_string(path) {
        Ok(text) => parse_palette(&text),
        Err(_) => theme::Palette::default(),
    }
}

fn parse_palette(text: &str) -> theme::Palette {
    let mut palette = theme::Palette::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Some(hex) = parse_hex_color(value) else {
            continue;
        };
        // the shell's panel colors arrive as RRGGBBAA (alpha in the
        // low byte); our surfaces are opaque, so keep the top 24 bits.
        // Pure RGB values pass through untouched.
        let hex = if hex > 0xFFFFFF { hex >> 8 } else { hex };
        match key.trim() {
            "panel_bg" => palette.panel_bg = hex,
            "surface" => palette.surface = hex,
            "surface_hover" => palette.surface_hover = hex,
            "inset" => palette.inset = hex,
            "divider" => palette.divider = hex,
            "divider_soft" => palette.divider_soft = hex,
            "text" => palette.text = hex,
            "text_dim" => palette.text_dim = hex,
            "accent" => palette.accent = hex,
            "accent_text" => palette.accent_text = hex,
            _ => {}
        }
    }
    palette
}

/// A color out of "#rrggbb", "rrggbb", "0xrrggbb", or the 8-digit
/// alpha-carrying form the shell publishes for panel colors.
fn parse_hex_color(value: &str) -> Option<u32> {
    let text = value.trim().trim_start_matches('#').trim_start_matches("0x");
    if text.len() != 6 && text.len() != 8 {
        return None;
    }
    if !text.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(text, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_palette_handshake() {
        let text = "panel_bg=0x181b21\naccent=#89b4fa\ntext=d8dce3ff\njunk=no\ntext_dim=zzz";
        let palette = parse_palette(text);
        assert_eq!(palette.panel_bg, 0x181b21);
        assert_eq!(palette.accent, 0x89b4fa);
        // the alpha-carrying form folds to the top 24 bits (alpha is
        // the low byte the shell publishes)
        assert_eq!(palette.text, 0xd8dce3);
        // a bad value keeps the default
        assert_eq!(palette.text_dim, theme::Palette::default().text_dim);
    }
}
