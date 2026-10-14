//! The spike's one view: a CPU graph over the last two minutes, the
//! memory line, a sortable header, and the joined process list. The
//! theming is deliberately neutral: the spike earns wallpaper-derived
//! colors at graduation, not before.

use std::time::Duration;

use gpui::{
    ClickEvent, Context, IntoElement, Render, UniformListScrollHandle, Window, div, px, rgb,
    uniform_list, prelude::*,
};

use crate::niri;
use crate::sample::{self, ProcRow, Sampler, SortKey};

/// Two minutes of history at the 1s tick.
const HISTORY: usize = 120;

const BG: u32 = 0x101014;
const ROW_BG: u32 = 0x16161c;
const FOCUS_BG: u32 = 0x1d222a;
const TEXT: u32 = 0xd8d8e0;
const DIM: u32 = 0x77778a;
const HAIRLINE: u32 = 0x26262e;
const ACCENT: u32 = 0x6ab0a2;

pub(crate) struct MonitorView {
    sampler: Sampler,
    history: Vec<f32>,
    rows: Vec<ProcRow>,
    sort: SortKey,
    mem_used: u64,
    mem_total: u64,
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
            scroll: UniformListScrollHandle::new(),
        };
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

    fn beat(&mut self, cx: &mut Context<Self>) {
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
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .font_family("monospace")
            .text_size(px(13.))
            .child(self.graph_panel(cpu_now))
            .child(self.header(cx))
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
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
    /// The top strip: totals on one line, the two-minute bar history
    /// under them.
    fn graph_panel(&self, cpu_now: f32) -> gpui::Div {
        let bars = self
            .history
            .iter()
            .map(|pct| {
                let height = (pct / 100. * 34.).clamp(1.5, 34.);
                div()
                    .flex_1()
                    .h(px(height))
                    .min_w(px(1.))
                    .bg(rgb(ACCENT))
            })
            .collect::<Vec<_>>();

        div()
            .px_3()
            .pt_2()
            .pb_2()
            .flex()
            .flex_col()
            .gap_1()
            .border_b_1()
            .border_color(rgb(HAIRLINE))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .text_color(rgb(TEXT))
                    .child(format!("CPU {cpu_now:.1}%"))
                    .child(format!(
                        "MEM {} / {}",
                        sample::human_mem(self.mem_used),
                        sample::human_mem(self.mem_total)
                    )),
            )
            .child(div().h(px(36.)).flex().items_end().gap(px(1.)).children(bars))
    }

    /// The column titles, each a click: the active one wears the
    /// accent. Cpu and Mem sort down, Name and Pid up.
    fn header(&self, cx: &mut Context<Self>) -> gpui::Div {
        let cells = [
            ("col-pid", "PID", SortKey::Pid, px(64.), false),
            ("col-name", "NAME", SortKey::Name, px(0.), true),
            ("col-window", "WINDOW", SortKey::Name, px(260.), false),
            ("col-cpu", "CPU%", SortKey::Cpu, px(72.), false),
            ("col-mem", "MEM", SortKey::Mem, px(96.), false),
        ];
        let children = cells
            .map(|(id, label, key, width, grow)| {
                let active = self.sort == key;
                let mut cell = div()
                    .id(id)
                    .py_1()
                    .px_2()
                    .text_size(px(11.))
                    .text_color(rgb(if active { ACCENT } else { DIM }))
                    .on_click(cx.listener(move |this, _click: &ClickEvent, _, cx| {
                        this.set_sort(key, cx);
                    }))
                    .child(label);
                cell = if grow { cell.flex_1() } else { cell.w(width) };
                cell
            })
            .into_iter()
            .collect::<Vec<_>>();

        div()
            .flex()
            .items_center()
            .px_1()
            .border_b_1()
            .border_color(rgb(HAIRLINE))
            .children(children)
    }
}

/// One table row: the numbers right-set in the mono font, the window
/// column carrying app_id and title, the focused window's row wearing
/// the focus background.
fn row_view(row: &ProcRow) -> gpui::Div {
    let window_cell = match &row.window {
        Some(window) => div().flex().gap_1().truncate().child(
            div()
                .truncate()
                .text_color(rgb(if window.focused { TEXT } else { DIM }))
                .child(format!(
                    "{} · {}",
                    window.app_id.as_deref().unwrap_or("?"),
                    window.title
                )),
        ),
        None => div().text_color(rgb(DIM)).child("-"),
    };

    div()
        .h(px(22.))
        .flex()
        .items_center()
        .px_2()
        .bg(rgb(if row.window.as_ref().is_some_and(|w| w.focused) {
            FOCUS_BG
        } else if row.pid % 2 == 0 {
            ROW_BG
        } else {
            BG
        }))
        .child(div().w(px(60.)).text_color(rgb(DIM)).child(row.pid.to_string()))
        .child(div().flex_1().truncate().child(row.name.clone()))
        .child(div().w(px(256.)).truncate().child(window_cell))
        .child(div().w(px(72.)).child(format!("{:>6.1}", row.cpu)))
        .child(div().w(px(96.)).child(format!("{:>10}", sample::human_mem(row.mem))))
}
