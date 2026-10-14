//! The spike's one view: a CPU graph over the last two minutes, the
//! memory line, and the table. The table leads with identity (the
//! window when there is one, the process name when not), keeps its
//! order stable while values tick (the order moves on a click, never
//! on a beat), and folds only the kernel's threads behind one summary
//! line: the app's question is which window is eating the machine,
//! and the session's own processes stay in sight.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use gpui::{
    ClickEvent, Context, Div, IntoElement, Render, Stateful, UniformListScrollHandle, Window,
    div, px, uniform_list, prelude::*,
};

use crate::niri;
use crate::sample::{self, ProcRow, Sampler, Snapshot, SortKey};
use crate::theme;

/// Two minutes of history at the 1s tick.
const HISTORY: usize = 120;

/// The table's columns, one spec for the header and the rows: the
/// same builder shapes both, so a value cannot drift off its title.
/// NAME is the flex column; the rest are fixed and right-set.
#[derive(Clone, Copy, PartialEq)]
enum Col {
    Name,
    Pid,
    Cpu,
    Mem,
}

const COLS: [Col; 4] = [Col::Name, Col::Pid, Col::Cpu, Col::Mem];

impl Col {
    fn label(self) -> &'static str {
        match self {
            Col::Name => "NAME",
            Col::Pid => "PID",
            Col::Cpu => "CPU%",
            Col::Mem => "MEM",
        }
    }

    /// None: the column flexes (NAME); Some: fixed pixels.
    fn width(self) -> Option<f32> {
        match self {
            Col::Name => None,
            Col::Pid => Some(88.),
            Col::Cpu => Some(72.),
            Col::Mem => Some(104.),
        }
    }

    fn right(self) -> bool {
        !matches!(self, Col::Name)
    }

    fn key(self) -> SortKey {
        match self {
            Col::Name => SortKey::Name,
            Col::Pid => SortKey::Pid,
            Col::Cpu => SortKey::Cpu,
            Col::Mem => SortKey::Mem,
        }
    }
}

pub(crate) struct MonitorView {
    sampler: Sampler,
    history: Vec<f32>,
    rows: Vec<ProcRow>,
    sort: SortKey,
    expanded: bool,
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
            expanded: false,
            mem_used: 0,
            mem_total: 0,
            palette_mtime: None,
            scroll: UniformListScrollHandle::new(),
        };
        view.sync_palette();
        view.beat(cx);
        sample::sort_rows(&mut view.rows, view.sort);

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
        self.apply_snapshot(snap);
        cx.notify();
    }

    /// Land a beat. Values move in place; the order only moves when
    /// the user clicks a header. A table that reshuffles every second
    /// cannot be read, and a hog announces itself by its number, not
    /// by jumping rows.
    fn apply_snapshot(&mut self, snap: Snapshot) {
        let mut fresh: HashMap<u32, ProcRow> =
            snap.rows.into_iter().map(|row| (row.pid, row)).collect();
        self.rows.retain_mut(|row| match fresh.remove(&row.pid) {
            Some(new) => {
                *row = new;
                true
            }
            // the process died between beats
            None => false,
        });
        // spawned since the last beat: they join at the tail, the
        // next header click files them
        self.rows.extend(fresh.into_values());

        self.history.push(snap.cpu_total);
        if self.history.len() > HISTORY {
            self.history.remove(0);
        }
        self.mem_used = snap.mem_used;
        self.mem_total = snap.mem_total;
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
        let (kernel, shown): (Vec<ProcRow>, Vec<ProcRow>) = rows
            .into_iter()
            .partition(|row| row.window.is_none() && sample::is_kernel(row));
        let list = if self.expanded {
            self.rows.clone()
        } else {
            shown
        };
        let count = list.len();

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::bg())
            .text_color(theme::text())
            .text_size(px(13.))
            .child(self.graph_panel(cpu_now))
            .child(
                div()
                    .flex()
                    .items_center()
                    .px_3()
                    .border_b_1()
                    .border_color(theme::border())
                    .children(COLS.map(|col| self.header_cell(col, cx))),
            )
            .child(
                // the wrapper is an explicit flex column and the list
                // grows into it: an Auto-sized deferred element is a
                // zero-content div to taffy and paints nothing without
                // a height (the quicklook list's own note)
                div()
                    .flex_1()
                    .overflow_hidden()
                    .px_3()
                    .flex()
                    .flex_col()
                    .child(
                        uniform_list("proc-rows", count, move |range, _, _| {
                            list[range.clone()].iter().map(row_view).collect()
                        })
                        .flex_1()
                        .track_scroll(&self.scroll),
                    ),
            )
            .child(self.footer(&kernel, cx))
    }
}

impl MonitorView {
    /// The top strip: totals on one line (dim labels, bright values),
    /// the two-minute history under them as one quiet silhouette.
    fn graph_panel(&self, cpu_now: f32) -> Div {
        let bars = self
            .history
            .iter()
            .map(|pct| {
                let height = (pct / 100. * 36.).clamp(2., 36.);
                div()
                    .flex_1()
                    .h(px(height))
                    .min_w(px(1.))
                    .bg(theme::ghost())
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
                    .child(div().child("CPU").text_color(theme::text_dim()))
                    .child(format!("{cpu_now:.1}%"))
                    .child(div().flex_1())
                    .child("MEM")
                    .text_color(theme::text_dim())
                    .child(format!(
                        "{} / {}",
                        sample::human_mem(self.mem_used),
                        sample::human_mem(self.mem_total)
                    )),
            )
            .child(div().h(px(36.)).flex().items_end().children(bars))
    }

    /// The column title, from the same Col spec the rows use: the
    /// same width, the same px_2, the same right-setting. Every
    /// column sorts; the active one wears the accent.
    fn header_cell(&self, col: Col, cx: &mut Context<Self>) -> Stateful<Div> {
        let active = self.sort == col.key();
        let cell = div()
            .id(col.label())
            .py_1()
            .px_2()
            .text_size(px(11.))
            .text_color(if active {
                theme::accent()
            } else {
                theme::text_dim()
            })
            .on_click(cx.listener(move |this, _click: &ClickEvent, _, cx| {
                this.set_sort(col.key(), cx);
            }))
            .child(col.label());
        let cell = match col.width() {
            Some(w) => cell.w(px(w)),
            None => cell.flex_1(),
        };
        if col.right() {
            cell.flex().justify_end()
        } else {
            cell
        }
    }

    /// The summary line under the table: the kernel's threads, folded.
    /// Collapsed it counts (n threads, their cpu and memory, both of
    /// which are nearly nothing); expanded it offers the way back.
    fn footer(&self, kernel: &[ProcRow], cx: &mut Context<Self>) -> Stateful<Div> {
        let cpu: f32 = kernel.iter().map(|row| row.cpu).sum();
        let mem: u64 = kernel.iter().map(|row| row.mem).sum();
        let label = if self.expanded {
            "- hide the kernel's threads".to_string()
        } else {
            format!(
                "+ {} kernel threads \u{b7} {cpu:.1}% \u{b7} {}",
                kernel.len(),
                sample::human_mem(mem)
            )
        };
        div()
            .id("system-toggle")
            .border_t_1()
            .border_color(theme::border())
            .py_1()
            .px_3()
            .text_size(px(11.))
            .text_color(theme::text_dim())
            .on_click(cx.listener(|this, _click: &ClickEvent, _, cx| {
                this.expanded = !this.expanded;
                cx.notify();
            }))
            .child(label)
    }
}

/// One table row, from the same Col spec the header uses: same
/// widths, same px_2, same right-setting, so a value sits exactly
/// under its title. Identity leads; unwindowed rows wear dim (they
/// are the session's residents, the expanded view's extra residents
/// are the kernel's); the focused window's row is the single
/// highlight.
fn row_view(row: &ProcRow) -> Div {
    let focused = row.window.as_ref().is_some_and(|w| w.focused);
    let mut base = div()
        .h(px(26.))
        .flex()
        .items_center()
        .bg(if focused {
            theme::row_selected()
        } else {
            theme::clear()
        });
    for col in COLS {
        base = base.child(child_cell(row, col));
    }
    base
}

/// The row's cell for one column: content first, then the shared
/// spec (width, padding, alignment) applied identically to the
/// header's cells.
fn child_cell(row: &ProcRow, col: Col) -> Div {
    let mut cell = match col {
        Col::Name => identity_cell(row),
        Col::Pid => div().child(row.pid.to_string()).text_color(theme::text_dim()),
        Col::Cpu => div().child(format!("{:.1}", row.cpu)),
        Col::Mem => div()
            .child(sample::human_mem(row.mem))
            .text_color(theme::text_dim()),
    };
    cell = match col.width() {
        Some(w) => cell.w(px(w)),
        None => cell.flex_1(),
    };
    cell = cell.px_2();
    if col.right() {
        cell.flex().justify_end()
    } else {
        cell
    }
}

/// The identity column's content: the window's app and title when
/// joined (a title that repeats the app says nothing twice), the
/// process name when not.
fn identity_cell(row: &ProcRow) -> Div {
    let (lead, suffix) = identity(row);
    let unwindowed = row.window.is_none();
    let mut cell = div().flex().gap_2().truncate().child(
        div()
            .truncate()
            .text_color(if unwindowed {
                theme::text_dim()
            } else {
                theme::text()
            })
            .child(lead),
    );
    if !suffix.is_empty() {
        cell = cell.child(
            div()
                .truncate()
                .text_color(theme::text_dim())
                .child(suffix),
        );
    }
    cell
}

/// The identity pair: lead (bright) and suffix (dim).
fn identity(row: &ProcRow) -> (String, String) {
    match &row.window {
        Some(window) => {
            let app = window.app_id.clone().unwrap_or_default();
            let title = window.title.trim().to_string();
            if title.is_empty() || title == app {
                (if app.is_empty() { row.name.clone() } else { app }, String::new())
            } else if app.is_empty() {
                (title, String::new())
            } else {
                (app, format!("\u{b7} {title}"))
            }
        }
        None => (row.name.clone(), String::new()),
    }
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

    #[test]
    fn identity_prefers_the_app_then_the_title_then_the_name() {
        let mut row = ProcRow {
            pid: 1,
            parent: None,
            name: "electron".into(),
            cpu: 0.,
            mem: 0,
            window: Some(sample::WindowRef {
                app_id: Some("kuma-term".into()),
                title: "kuma-term".into(),
                focused: false,
            }),
        };
        // a title that repeats the app says nothing twice
        let (lead, suffix) = identity(&row);
        assert_eq!(lead, "kuma-term");
        assert!(suffix.is_empty());
        // a real title rides along, dim, after the app
        row.window.as_mut().unwrap().title = "main".into();
        let (lead, suffix) = identity(&row);
        assert_eq!(lead, "kuma-term");
        assert_eq!(suffix, "\u{b7} main");
        // no app_id: the title speaks
        row.window.as_mut().unwrap().app_id = None;
        let (lead, suffix) = identity(&row);
        assert_eq!(lead, "main");
        assert!(suffix.is_empty());
        // no window: the process name
        row.window = None;
        let (lead, suffix) = identity(&row);
        assert_eq!(lead, "electron");
        assert!(suffix.is_empty());
    }
}
