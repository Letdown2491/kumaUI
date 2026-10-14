//! The spike's one view: a CPU graph over the last two minutes, the
//! memory line, and the table. The table speaks in apps, not
//! processes: every process an app owns files under one row (the
//! windowed process and its helpers together), clicking it unfolds
//! the member roster in place, and the session's background plumbing
//! (the daemons nothing recognizes) hides behind one summary line.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use gpui::{
    prelude::*, ClickEvent, Context, Div, IntoElement, Render, SharedString, Stateful,
    UniformListScrollHandle, WeakEntity, Window, div, px, uniform_list,
};

use crate::niri;
use crate::sample::{self, AppGroup, ProcRow, Sampler, Snapshot, SortKey};
use crate::theme;

/// Two minutes of history at the 1s tick.
const HISTORY: usize = 120;

/// The table's columns, one spec for the header and the rows: the
/// same builder shapes both, so a value cannot drift off its title.
/// NAME is the flex column; the rest are fixed and right-set.
#[derive(Clone, Copy, PartialEq, Debug)]
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

/// One visible line: an app's row (with its fold state), one of its
/// members when unfolded, or the background summary.
enum Item {
    Group {
        index: usize,
        key: String,
        open: bool,
        dim: bool,
    },
    Member {
        group: usize,
        member: usize,
    },
    BgSummary {
        open: bool,
    },
}

pub(crate) struct MonitorView {
    sampler: Sampler,
    history: Vec<f32>,
    /// The app groups, persistent across beats: existing groups keep
    /// their position, the dead drop out, first-seen apps join at the
    /// tail, and only a header click reorders any of it.
    groups: Vec<AppGroup>,
    /// Which app groups are unfolded, keyed by the group key so the
    /// state survives the members churning underneath it.
    expanded_apps: HashSet<String>,
    show_background: bool,
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
            groups: Vec::new(),
            expanded_apps: HashSet::new(),
            show_background: false,
            sort: SortKey::Cpu,
            mem_used: 0,
            mem_total: 0,
            palette_mtime: None,
            scroll: UniformListScrollHandle::new(),
        };
        view.sync_palette();
        view.beat(cx);
        sample::sort_groups(&mut view.groups, view.sort);

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
        if !snap.rows.is_empty() {
            sample::merge_groups(&mut self.groups, snap.rows);
        }

        self.history.push(snap.cpu_total);
        if self.history.len() > HISTORY {
            self.history.remove(0);
        }
        self.mem_used = snap.mem_used;
        self.mem_total = snap.mem_total;
    }

    fn set_sort(&mut self, key: SortKey, cx: &mut Context<Self>) {
        self.sort = key;
        sample::sort_groups(&mut self.groups, key);
        cx.notify();
    }
}

impl Render for MonitorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cpu_now = self.history.last().copied().unwrap_or(0.);
        let this = cx.weak_entity();
        // apps first, then the background summary, then (unfolded)
        // the background groups: each keeps the groups' sorted order
        let mut items: Vec<Item> = Vec::new();
        let mut background: Vec<usize> = Vec::new();
        for (index, group) in self.groups.iter().enumerate() {
            if group.background {
                background.push(index);
                continue;
            }
            let open = self.expanded_apps.contains(&group.key);
            items.push(Item::Group {
                index,
                key: group.key.clone(),
                open,
                dim: false,
            });
            if open {
                for member in 0..group.members.len() {
                    items.push(Item::Member {
                        group: index,
                        member,
                    });
                }
            }
        }
        let bg_count: usize = background
            .iter()
            .map(|index| self.groups[*index].members.len())
            .sum();
        let bg_cpu: f32 = background
            .iter()
            .map(|index| self.groups[*index].members.iter().map(|row| row.cpu).sum::<f32>())
            .sum();
        let bg_mem: u64 = background
            .iter()
            .map(|index| self.groups[*index].members.iter().map(|row| row.mem).sum::<u64>())
            .sum();
        if !background.is_empty() {
            items.push(Item::BgSummary {
                open: self.show_background,
            });
        }
        if self.show_background {
            for index in &background {
                let open = self.expanded_apps.contains(&self.groups[*index].key);
                items.push(Item::Group {
                    index: *index,
                    key: self.groups[*index].key.clone(),
                    open,
                    dim: true,
                });
                if open {
                    for member in 0..self.groups[*index].members.len() {
                        items.push(Item::Member {
                            group: *index,
                            member,
                        });
                    }
                }
            }
        }
        let count = items.len();
        let groups = self.groups.clone();
        let this2 = this.clone();

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
                            items[range.clone()]
                                .iter()
                                .map(|item| list_item(item, &groups, &this2))
                                .collect()
                        })
                        .flex_1()
                        .track_scroll(&self.scroll),
                    ),
            )
            .child(self.footer(bg_count, bg_cpu, bg_mem))
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
            .debug_selector(move || format!("hdr-{}", col.label()))
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

    /// The line under the table: the app and process counts on the
    /// left, the build stamp on the right (which build this window
    /// is: a monitor that cannot say its own build invites judging it
    /// against the previous one).
    fn footer(&self, processes: usize, _bg_cpu: f32, _bg_mem: u64) -> Div {
        let apps = self.groups.iter().filter(|g| !g.background).count();
        div()
            .flex()
            .border_t_1()
            .border_color(theme::border())
            .py_1()
            .px_3()
            .text_size(px(11.))
            .text_color(theme::text_dim())
            .child(format!(
                "{} apps \u{b7} {} processes",
                apps, processes
            ))
            .child(
                div()
                    .flex_1()
                    .text_size(px(10.))
                    .text_color(theme::text_dim())
                    .child(crate::version_line()),
            )
    }
}

/// The shared cell spec: width, padding, right-setting. The header
/// builds its own cells (stateful, clickable) but every row kind
/// passes through this one function, so the extents cannot drift.
fn apply_spec(cell: Div, col: Col) -> Div {
    let cell = match col.width() {
        Some(w) => cell.w(px(w)),
        None => cell.flex_1(),
    };
    let cell = cell.px_2();
    let cell = cell.debug_selector(move || format!("row-{}", col.label()));
    if col.right() {
        cell.flex().justify_end()
    } else {
        cell
    }
}

fn list_item(
    item: &Item,
    groups: &[AppGroup],
    this: &WeakEntity<MonitorView>,
) -> Stateful<Div> {
    match item {
        Item::Group {
            index,
            key,
            open,
            dim,
        } => group_row(&groups[*index], key, *open, *dim, this),
        Item::Member { group, member } => member_row(&groups[*group].members[*member]),
        Item::BgSummary { open } => background_row(*open, this),
    }
}

/// The app's row: its sums, its process count, and the fold for the
/// roster. Clicking anywhere in it unfolds the members in place.
fn group_row(
    group: &AppGroup,
    key: &str,
    open: bool,
    dim: bool,
    this: &WeakEntity<MonitorView>,
) -> Stateful<Div> {
    let click_key = key.to_string();
    let this = this.clone();
    let text = if dim { theme::text_dim() } else { theme::text() };
    let mut name = div().flex().gap_2().truncate()
        .child(
            div()
                .text_color(theme::text_dim())
                .child(if open { "\u{25be}" } else { "\u{25b8}" }),
        )
        .child(div().truncate().text_color(text).child(group.name.clone()));
    if group.members.len() > 1 {
        name = name.child(
            div()
                .truncate()
                .text_color(theme::text_dim())
                .child(format!("\u{b7} {} procs", group.members.len())),
        );
    }
    div()
        .id(SharedString::from(key.to_string()))
        .h(px(26.))
        .w_full()
        .flex()
        .items_center()
        .debug_selector(|| "row-line".into())
        .bg(if group.has_focused {
            theme::row_selected()
        } else {
            theme::clear()
        })
        .on_click(move |_, _, app| {
            let _ = this.update(app, |this, cx| {
                if !this.expanded_apps.remove(&click_key) {
                    this.expanded_apps.insert(click_key.clone());
                }
                cx.notify();
            });
        })
        .child(apply_spec(name.flex_1(), Col::Name))
        .child(apply_spec(div(), Col::Pid))
        .child(
            apply_spec(
                div()
                    .child(format!("{:.1}", group.cpu))
                    .text_color(text),
                Col::Cpu,
            ),
        )
        .child(
            apply_spec(
                div()
                    .child(sample::human_mem(group.mem))
                    .text_color(theme::text_dim()),
                Col::Mem,
            ),
        )
}

/// The background summary: the session's plumbing, folded. Collapsed
/// it counts (processes, their cpu and memory); expanded it offers
/// the way back and the daemon rows beneath it.
fn background_row(open: bool, this: &WeakEntity<MonitorView>) -> Stateful<Div> {
    let this = this.clone();
    div()
        .id("bg-summary")
        .h(px(26.))
        .w_full()
        .flex()
        .items_center()
        .debug_selector(|| "row-line".into())
        .on_click(move |_, _, app| {
            let _ = this.update(app, |this, cx| {
                this.show_background = !this.show_background;
                cx.notify();
            });
        })
        .child(
            apply_spec(
                div()
                    .flex_1()
                    .text_color(theme::text_dim())
                    .child(if open {
                        "\u{25be} hide background processes".to_string()
                    } else {
                        "\u{25b8} background processes".to_string()
                    }),
                Col::Name,
            )
            .pl(px(2.)),
        )
        .child(apply_spec(div(), Col::Pid))
        .child(apply_spec(div(), Col::Cpu))
        .child(apply_spec(div(), Col::Mem))
}

/// A member row: the window's title when it has one (the app's name
/// is the group header now: saying it twice is noise), the process
/// name when not; indented a step under its app.
fn member_row(row: &ProcRow) -> Stateful<Div> {
    let windowed = row.window.is_some();
    let focused = row.window.as_ref().is_some_and(|w| w.focused);
    let lead = member_lead(row);
    div()
        .id(SharedString::from(format!("m-{}", row.pid)))
        .h(px(26.))
        .w_full()
        .flex()
        .items_center()
        .debug_selector(|| "row-line".into())
        .bg(if focused {
            theme::row_selected()
        } else {
            theme::clear()
        })
        .child(
            apply_spec(
                div()
                    .flex()
                    .gap_2()
                    .truncate()
                    .child(
                        div()
                            .truncate()
                            .text_color(if windowed {
                                theme::text()
                            } else {
                                theme::text_dim()
                            })
                            .child(lead),
                    )
                    .flex_1(),
                Col::Name,
            )
            .pl(px(24.)),
        )
        .child(
            apply_spec(
                div().child(row.pid.to_string()).text_color(theme::text_dim()),
                Col::Pid,
            ),
        )
        .child(apply_spec(div().child(format!("{:.1}", row.cpu)), Col::Cpu))
        .child(
            apply_spec(
                div()
                    .child(sample::human_mem(row.mem))
                    .text_color(theme::text_dim()),
                Col::Mem,
            ),
        )
}

/// A member's lead: the title when the window has one, the process
/// name otherwise.
fn member_lead(row: &ProcRow) -> String {
    match &row.window {
        Some(window) if !window.title.is_empty() => window.title.clone(),
        _ => row.name.clone(),
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
    fn member_leads_with_the_title_then_the_name() {
        let mut row = ProcRow {
            pid: 1,
            parent: None,
            name: "electron".into(),
            cpu: 0.,
            mem: 0,
            window: Some(sample::WindowRef {
                app_id: Some("kuma-term".into()),
                title: "main".into(),
                focused: false,
            }),
        };
        // the app's name is the group header: the member says the title
        assert_eq!(member_lead(&row), "main");
        row.window.as_mut().unwrap().title = String::new();
        assert_eq!(member_lead(&row), "electron");
        row.window = None;
        assert_eq!(member_lead(&row), "electron");
    }

    /// The proof the last rounds lacked: header and rows get their
    /// cells from one Col spec, and this asserts they land on the
    /// same x-extents in a real painted window, not in my reasoning.
    #[test]
    fn row_cells_share_the_header_s_column_extents() {
        use std::sync::Arc;

        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(()),
        );
        let mut window = app.open_window(|window, cx| MonitorView::new(window, cx));
        app.run_until_parked();
        // in the test container there are no windows and no windowed
        // ancestors: unfold the background roster so the table surely
        // paints member rows
        window.update(|view, _, cx| {
            view.show_background = true;
            view.expanded_apps = view.groups.iter().map(|g| g.key.clone()).collect();
            cx.notify();
        });
        app.run_until_parked();
        let extents = window.update(|_, window, _| {
            let probe = |sel: &str| {
                window
                    .debug_element_bounds(sel)
                    .map(|b| (sel.to_string(), b))
            };
            let mut all: Vec<(String, gpui::Bounds<gpui::Pixels>)> = Vec::new();
            all.extend(probe("hdr-line"));
            all.extend(probe("row-line"));
            for col in COLS {
                all.extend(probe(&format!("hdr-{}", col.label())));
                all.extend(probe(&format!("row-{}", col.label())));
            }
            all
        });
        let bad: Vec<String> = COLS
            .iter()
            .filter_map(|col| {
                let hdr = extents
                    .iter()
                    .find(|(sel, _)| *sel == format!("hdr-{}", col.label()))
                    .map(|(_, b)| *b)?;
                let row = extents
                    .iter()
                    .find(|(sel, _)| *sel == format!("row-{}", col.label()))
                    .map(|(_, b)| *b)?;
                if (hdr.origin.x - row.origin.x).abs() < px(1.)
                    && (hdr.size.width - row.size.width).abs() < px(1.)
                {
                    None
                } else {
                    Some(format!(
                        "{col:?}: hdr x={} w={} | row x={} w={}",
                        hdr.origin.x, hdr.size.width, row.origin.x, row.size.width
                    ))
                }
            })
            .collect();
        let dump = extents
            .iter()
            .map(|(sel, b)| {
                format!(
                    "{sel}: x={} y={} w={} h={}",
                    b.origin.x, b.origin.y, b.size.width, b.size.height
                )
            })
            .collect::<Vec<_>>()
            .join("\n  ");
        assert!(
            bad.is_empty(),
            "column extents diverge:\n{}\nall bounds:\n  {dump}",
            bad.join("\n")
        );
    }
}
