//! The spike's one view: a CPU graph over the last two minutes, the
//! memory line, and the table. The table speaks in apps, not
//! processes: every process an app owns files under one row (the
//! windowed process and its helpers together), clicking it unfolds
//! the member roster in place, and the session's background plumbing
//! (the daemons nothing recognizes) hides behind one summary line.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use gpui::{
    prelude::*, App, ClickEvent, ClipboardItem, Context, Div, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, Keystroke, MouseButton, Render, Rgba, ScrollStrategy, SharedString, Stateful,
    UniformListScrollHandle, WeakEntity, Window, div, px, uniform_list,
};

use crate::niri;
use crate::sample::{self, AppGroup, GroupHit, ProcRow, Sampler, Snapshot, SortKey};
use crate::stats::{Stats, Vitals};
use crate::theme;

/// One beat's sampling state, living in the tick's background task:
/// both halves compute deltas against the previous beat, so the state
/// must survive across beats. It never touches the UI thread; a beat
/// lands as a `BeatOut` the view reads like a letter.
struct BeatState {
    sampler: Sampler,
    stats: Stats,
}

/// What one beat produced: the table's snapshot and the cards'
/// vitals. Plain data, shipped across the executor boundary.
struct BeatOut {
    snap: Snapshot,
    vitals: Vitals,
    cores: Vec<f32>,
    /// whether niri has this process's own window inside the tiling
    /// layout: the titlebar's fold reads it
    niri_tiled: bool,
}

impl BeatState {
    fn new() -> Self {
        Self {
            sampler: Sampler::new(),
            stats: Stats::new(),
        }
    }

    fn run(&mut self) -> BeatOut {
        let windows = niri::windows().unwrap_or_else(|err| {
            log::debug!("niri join unavailable this beat: {err:#}");
            Vec::new()
        });
        let niri_tiled = windows.iter().any(|w| {
            w.pid == Some(std::process::id()) && w.is_floating == Some(false)
        });
        let snap = self.sampler.beat(&windows);
        let (vitals, cores) = self.stats.beat();
        BeatOut {
            snap,
            vitals,
            cores,
            niri_tiled,
        }
    }
}

/// A card sparkline's window: forty seconds of the recent past.
const CARD_HIST: usize = 40;

/// The vitals cards' sparkline height; the rows whisper at theirs.
const CARD_SPARK_H: f32 = 18.;

/// The row spark's height: a strip at the cpu cell's bottom, clear
/// of the digits above it.
const ROW_SPARK_H: f32 = 8.;

/// The cpu card spans two columns, so its sparkline carries twice
/// the window to keep the bars from going fat.
const CPU_HIST: usize = 80;

/// The pressure card's rows in display order: (label, psi slot).
const PSI_ROWS: [(&str, usize); 3] = [("cpu", 0), ("mem", 1), ("io", 2)];

/// A group's disk suffix appears at or above this throughput; below
/// it the identity line stays quiet.
const DISK_SUFFIX_AT: u64 = 100 * 1024;

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
        count: usize,
        cpu: f32,
        mem: u64,
    },
}

pub(crate) struct MonitorView {
    history: Vec<f32>,
    vitals: Vitals,
    /// The cards' own windows: disk and network throughput, recent
    /// past, for their sparklines. The temperature joins only while a
    /// sensor exists, the runnable count because its burstiness is
    /// the story the instant count can't tell.
    disk_hist: Vec<f32>,
    net_hist: Vec<f32>,
    temp_hist: Vec<f32>,
    procs_hist: Vec<f32>,
    /// Memory usage in percent, the mem card's sparkline: a fixed
    /// 0-100 scale, since memory's moves are too small for
    /// peak-scaling to tell the truth about.
    mem_hist: Vec<f32>,
    /// Per-core usage from the last beat, shown when the CPU card
    /// opens.
    core_usage: Vec<f32>,
    show_cores: bool,
    show_nets: bool,
    show_disks: bool,
    show_mem: bool,
    swap_used: u64,
    swap_total: u64,
    /// The app groups, persistent across beats: existing groups keep
    /// their position, the dead drop out, first-seen apps join at the
    /// tail, and only a header click reorders any of it.
    groups: Vec<AppGroup>,
    /// Each group's recent cpu, one sample per beat, keyed by the
    /// group key: the spark behind the row's name. Rings die with
    /// their group, so the map never gathers dead keys.
    group_hists: HashMap<String, Vec<f32>>,
    /// Which app groups are unfolded, keyed by the group key so the
    /// state survives the members churning underneath it.
    expanded_apps: HashSet<String>,
    show_background: bool,
    sort: SortKey,
    mem_used: u64,
    mem_total: u64,
    palette_mtime: Option<SystemTime>,
    scroll: UniformListScrollHandle,
    /// The table's filter: plain typing lands here, escape clears,
    /// an empty string is no filter at all.
    filter: String,
    /// The row the user last copied: its name and when, so the
    /// footer can confirm the copy for a couple of seconds.
    copied_flash: Option<(String, Instant)>,
    /// The keyboard cursor: which visible row the kill keys aim at,
    /// as an index into the same item list the render walks. None
    /// until the first arrow press or row click, so a window nobody
    /// has steered in wears no highlight.
    cursor: Option<usize>,
    /// The pid one SIGTERM already went to from the kill key: a
    /// second kill key on the same row means the ask did not land,
    /// and arms the SIGKILL card instead.
    sigterm_pid: Option<u32>,
    /// The armed SIGKILL: the row's name and pid, shown in a card
    /// above the footer; enter seals it, any other key unwinds it.
    kill_confirm: Option<(String, u32)>,
    /// The last kill action's line, and when, so the footer can
    /// confirm the signal for a couple of seconds (same shape as
    /// copied_flash; a beat re-renders every second, so the line
    /// retires itself).
    signal_flash: Option<(String, Instant)>,
    /// A sort restored from the state file waits for the first
    /// merged table to apply to: the order only moves on the user's
    /// say-so, and this is that say-so arriving from last launch.
    pending_sort: bool,
    /// Where the view's knobs persist. None (no config dir) leaves
    /// both halves dark: nothing loads, nothing saves. Tests point
    /// it at a scratch file.
    state_path: Option<PathBuf>,
    /// Whether niri has this process's own window inside the tiling
    /// layout: the titlebar folds on it. niri's xdg tiled states are
    /// a CSD hint, not a tiling signal, so the IPC is the only voice
    niri_tiled: bool,
    focus: FocusHandle,
    /// The last beat that reached the state: an equal beat applies
    /// nothing, so an idle machine stops repainting once a second.
    last_beat: Option<Snapshot>,
    /// The groups and rings as Arc snapshots for the render closure:
    /// the uniform list's items closure is 'static, and a per-render
    /// deep clone of the whole table was its price. Refreshed by
    /// `refresh_snapshots` wherever the sources mutate.
    groups_snap: Arc<Vec<AppGroup>>,
    hists_snap: Arc<HashMap<String, Vec<f32>>>,
    /// Set by every paint, cleared by every notified beat: the
    /// repaint gate. An occluded surface stops paying for repaints
    /// it cannot show; sampling never stops, so the rings stay
    /// continuous for when it returns.
    painted: bool,
}

impl MonitorView {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // the window surface must opt into alpha before anything
        // paints, and subpixel AA is wrong over a translucent
        // background (kuma-term/src/view.rs, same stack)
        window.set_background_appearance(gpui::WindowBackgroundAppearance::Transparent);
        // the titlebar trims its buttons to this: worth a line in the
        // log when a button the user expects is missing
        log::info!("window controls: {:?}", window.window_controls());
        // the filter rides the window's focus: the view takes it at
        // birth and nothing in the table steals it, so plain typing
        // always lands in the buffer
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        // the view's knobs outlive the window: the state file under
        // ~/.config (Koguma's shape) carries the sort and the filter
        // across launches. A missing or unreadable file is a first
        // launch, not an error; a malformed line dies quietly so a
        // future key cannot break an older reader.
        let state_path = Self::state_path();
        let (saved_sort, saved_filter) = state_path
            .as_deref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .as_deref()
            .map(parse_state)
            .unwrap_or((None, None));
        let mut view = Self {
            history: Vec::new(),
            vitals: Vitals::default(),
            disk_hist: Vec::new(),
            net_hist: Vec::new(),
            temp_hist: Vec::new(),
            procs_hist: Vec::new(),
            mem_hist: Vec::new(),
            core_usage: Vec::new(),
            show_cores: false,
            show_nets: false,
            show_disks: false,
            show_mem: false,
            swap_used: 0,
            swap_total: 0,
            groups: Vec::new(),
            group_hists: HashMap::new(),
            expanded_apps: HashSet::new(),
            show_background: false,
            sort: saved_sort.unwrap_or(SortKey::Cpu),
            mem_used: 0,
            mem_total: 0,
            palette_mtime: None,
            scroll: UniformListScrollHandle::new(),
            filter: saved_filter.unwrap_or_default(),
            copied_flash: None,
            cursor: None,
            sigterm_pid: None,
            kill_confirm: None,
            signal_flash: None,
            pending_sort: saved_sort.is_some(),
            state_path,
            // the fold's niri half, seeded: a window is not in niri's
            // list until it maps, so until the first beat the best
            // answer is "tiled while niri is there"; niri tiles at
            // birth. The beat's own answer replaces it
            niri_tiled: kuma_platform::niri::own_windows().is_ok(),
            focus,
            last_beat: None,
            groups_snap: Arc::new(Vec::new()),
            hists_snap: Arc::new(HashMap::new()),
            painted: true,
        };
        view.sync_palette();

        // The tick: sample in the background, land on the UI thread,
        // wait a second, again. The bar's loop shape (kuma-shell/src/
        // bar.rs): update-or-die, then the executor's timer. The
        // sampling itself lives OFF the UI thread: a beat's reads
        // (niri's socket under its timeout, the /proc walks, the
        // statm round) must never stall a paint, whatever the
        // machine is doing to the disk. A beat's failures degrade
        // that beat, not the loop: the sampler keeps its deltas, the
        // niri join drops to None for a beat and comes back.
        let mut beat = BeatState::new();
        cx.spawn(async move |this, cx| {
            loop {
                // ownership ping-pongs through the pool: the state
                // comes back with its deltas intact
                let (next, out) =
                    cx.background_executor()
                        .spawn(async move {
                            let out = beat.run();
                            (beat, out)
                        })
                        .await;
                beat = next;
                let alive = this
                    .update(cx, |this, cx| {
                        this.apply_beat(out, cx);
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
        let mtime = kuma_platform::palette::mtime();
        if mtime == self.palette_mtime {
            return;
        }
        self.palette_mtime = mtime;
        theme::apply_palette(&read_palette());
    }

    fn apply_beat(&mut self, out: BeatOut, cx: &mut Context<Self>) {
        self.sync_palette();
        self.niri_tiled = out.niri_tiled;
        // an equal beat applies nothing: the table, the rings, and
        // the repaint are all skipped, so an idle machine stops
        // repainting once a second
        if self.last_beat.as_ref() == Some(&out.snap) {
            return;
        }
        self.last_beat = Some(out.snap.clone());
        self.apply_snapshot(out.snap, out.vitals, out.cores);
        // the repaint gate: an occluded surface drops the beat
        // instead of queueing a paint nobody can see (sampling never
        // stops, so the rings stay continuous for when it returns)
        if self.painted {
            self.painted = false;
            cx.notify();
        }
    }

    /// Land a beat. Values move in place; the order only moves when
    /// the user clicks a header. A table that reshuffles every second
    /// cannot be read, and a hog announces itself by its number, not
    /// by jumping rows.
    fn apply_snapshot(&mut self, snap: Snapshot, vitals: Vitals, cores: Vec<f32>) {
        if !snap.rows.is_empty() {
            sample::merge_groups(&mut self.groups, snap.rows);
            // the rows' rings: one cpu sample per beat, behind each
            // name. Rings of groups the merge dropped die here too.
            for group in &self.groups {
                push_hist(
                    self.group_hists.entry(group.key.clone()).or_default(),
                    group.cpu,
                    CARD_HIST,
                );
            }
            let live: HashSet<&str> = self.groups.iter().map(|g| g.key.as_str()).collect();
            self.group_hists.retain(|key, _| live.contains(key.as_str()));
            // unfolded state dies with its group too, like the rings:
            // without this the set gathers the keys of every app ever
            // expanded, forever
            self.expanded_apps.retain(|key| live.contains(key.as_str()));
            // a restored sort applies once, to the first table there
            // is to sort: from here the order only moves on the
            // user's say-so, like any other launch
            if self.pending_sort {
                self.pending_sort = false;
                sample::sort_groups(&mut self.groups, self.sort);
            }
        }

        push_hist(&mut self.history, snap.cpu_total, CPU_HIST);
        self.mem_used = snap.mem_used;
        self.mem_total = snap.mem_total;
        self.swap_used = snap.swap_used;
        self.swap_total = snap.swap_total;
        push_hist(
            &mut self.disk_hist,
            (vitals.io_read + vitals.io_write) as f32,
            CARD_HIST,
        );
        push_hist(
            &mut self.net_hist,
            (vitals.net_rx + vitals.net_tx) as f32,
            CARD_HIST,
        );
        if let Some(temp) = vitals.cpu_temp {
            push_hist(&mut self.temp_hist, temp, CARD_HIST);
        }
        push_hist(&mut self.procs_hist, vitals.procs_running as f32, CARD_HIST);
        let mem_pct_now = if snap.mem_total > 0 {
            snap.mem_used as f32 / snap.mem_total as f32 * 100.
        } else {
            0.
        };
        push_hist(&mut self.mem_hist, mem_pct_now, CARD_HIST);
        self.vitals = vitals;
        if !cores.is_empty() {
            self.core_usage = cores;
        }
        self.refresh_snapshots();
    }

    /// Re-stamp the render's Arc snapshots after any mutation of the
    /// table or its rings: the render closure is 'static and used to
    /// pay a full deep clone of both per frame instead.
    fn refresh_snapshots(&mut self) {
        self.groups_snap = Arc::new(self.groups.clone());
        self.hists_snap = Arc::new(self.group_hists.clone());
    }

    fn set_sort(&mut self, key: SortKey, cx: &mut Context<Self>) {
        self.sort = key;
        sample::sort_groups(&mut self.groups, key);
        self.save_state();
        self.refresh_snapshots();
        cx.notify();
    }

    // ---------- the state file ----------

    /// Where the view's knobs persist: one tiny key=value file under
    /// ~/.config, Koguma's shape (browser.rs `state_path`), spoken in
    /// the same dialect so a reader meets one convention. Not a
    /// settings system, just the knobs you set once and expect to
    /// keep.
    fn state_path() -> Option<PathBuf> {
        let mut path = dirs::config_dir()?;
        path.push("kuma-monitor/state");
        Some(path)
    }

    /// The knobs' text, one line each: the sort key's own name and
    /// the filter buffer whole (it can hold an equals sign; the
    /// parse's split_once keeps the value's tail). A trailing newline
    /// because a config file ends with one.
    fn state_text(&self) -> String {
        format!("sort={}\nfilter={}\n", self.sort.name(), self.filter)
    }

    /// The knobs land through the platform's atomic write: one
    /// rename, the bytes fsynced first, the record 0600. A crash
    /// mid-write tears the temp file, never the state the next
    /// launch restores from.
    fn save_state(&self) {
        let Some(path) = &self.state_path else {
            return;
        };
        if let Err(err) = kuma_platform::files::write_atomic(path, self.state_text().as_bytes()) {
            log::error!("save state: {err}");
        }
    }

    // ---------- the kill path ----------

    /// The cursor row's process, if the cursor sits on a member row
    /// (the pid the row carries): group headers and the background
    /// summary are folds and sums, and the kill key reads through
    /// them as nothing to aim at.
    fn cursor_row(&self) -> Option<&ProcRow> {
        let item = self.visible_items().into_iter().nth(self.cursor?)?;
        match item {
            Item::Member { group, member } => {
                self.groups.get(group).and_then(|g| g.members.get(member))
            }
            _ => None,
        }
    }

    /// The arrows walk the visible rows, wrapping at the ends; the
    /// list scrolls the cursor back into view. Nothing armed, no
    /// kill: just the aiming.
    fn move_cursor(&mut self, down: bool, cx: &mut Context<Self>) {
        let count = self.visible_items().len();
        if count == 0 {
            self.cursor = None;
            cx.notify();
            return;
        }
        let step = if down { 1 } else { count - 1 };
        self.cursor = Some(match self.cursor {
            None if down => 0,
            None => count - 1,
            Some(ix) => (ix + step) % count,
        });
        self.scroll
            .scroll_to_item(self.cursor.unwrap(), ScrollStrategy::Center);
        cx.notify();
    }

    /// The kill key on the cursor row. First press: one SIGTERM, the
    /// polite ask, which most apps answer by exiting and saving.
    /// The same row a second time means the ask did not land, so
    /// that press arms the SIGKILL card instead: nothing dies on a
    /// second keypress without a yes.
    fn kill_key(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.cursor_row() else {
            return;
        };
        let (pid, name) = (row.pid, member_lead(row));
        if self.sigterm_pid == Some(pid) {
            self.kill_confirm = Some((name, pid));
            cx.notify();
            return;
        }
        self.signal_flash = Some((send_signal(pid, libc::SIGTERM, name), Instant::now()));
        self.sigterm_pid = Some(pid);
        cx.notify();
    }

    /// The card's yes: SIGKILL to the armed pid, the flash line to
    /// say so. The arm drops whatever the answer was.
    fn confirm_sigkill(&mut self, cx: &mut Context<Self>) {
        if let Some((name, pid)) = self.kill_confirm.take() {
            self.signal_flash = Some((send_signal(pid, libc::SIGKILL, name), Instant::now()));
        }
        self.sigterm_pid = None;
        cx.notify();
    }

    /// The table's rows for the current beat: apps first, then the
    /// background summary, then the unfolded background groups, each
    /// keeping the groups' sorted order. With a filter active, a
    /// group matched by name shows whole (its expand state still
    /// decides the members), a group matched through a member or pid
    /// pulls its header along and shows just the matches, and
    /// matching background groups surface past the fold. The
    /// summary itself obeys the same hide rule: it shows only when
    /// the filter found something in the background (it is then the
    /// surfaced rows' fold header), and its sums stay machine-wide
    /// whenever it shows.
    fn visible_items(&self) -> Vec<Item> {
        let q = self.filter.to_lowercase();
        let filtering = !q.is_empty();
        let mut items: Vec<Item> = Vec::new();
        let mut background: Vec<(usize, Option<GroupHit>)> = Vec::new();
        for (index, group) in self.groups.iter().enumerate() {
            let hit = if filtering {
                sample::match_group(group, &q)
            } else {
                Some(GroupHit::All)
            };
            if group.background {
                background.push((index, hit));
                continue;
            }
            if let Some(hit) = hit {
                push_group_rows(
                    &mut items,
                    &self.groups,
                    index,
                    &hit,
                    false,
                    &self.expanded_apps,
                );
            }
        }
        let bg_count: usize = background
            .iter()
            .map(|(index, _)| self.groups[*index].members.len())
            .sum();
        let bg_cpu: f32 = background
            .iter()
            .map(|(index, _)| self.groups[*index].members.iter().map(|row| row.cpu).sum::<f32>())
            .sum();
        let bg_mem: u64 = background
            .iter()
            .map(|(index, _)| self.groups[*index].members.iter().map(|row| row.mem).sum::<u64>())
            .sum();
        // the summary is a table row like any other: with a filter
        // active it shows only when the filter found something in
        // the background, and then the miss line in render can
        // speak; unfiltered, it always shows
        let bg_matches = background.iter().any(|(_, hit)| hit.is_some());
        if !background.is_empty() && (!filtering || bg_matches) {
            items.push(Item::BgSummary {
                open: self.show_background,
                count: bg_count,
                cpu: bg_cpu,
                mem: bg_mem,
            });
        }
        for (index, hit) in &background {
            let surfaced = if filtering { hit.is_some() } else { self.show_background };
            if surfaced {
                push_group_rows(
                    &mut items,
                    &self.groups,
                    *index,
                    hit.as_ref().unwrap_or(&GroupHit::All),
                    true,
                    &self.expanded_apps,
                );
            }
        }
        items
    }

    /// The filter buffer takes the window's plain typing: printable
    /// keys ride key_char, backspace pops, escape clears. Anything
    /// with a modifier held is not typing. Returns whether the
    /// buffer changed, so a stray keystroke spends no repaint.
    fn filter_key(&mut self, keystroke: &Keystroke) -> bool {
        let modifiers = &keystroke.modifiers;
        match keystroke.key.as_str() {
            "escape" => {
                if self.filter.is_empty() {
                    false
                } else {
                    self.filter.clear();
                    true
                }
            }
            "backspace" => self.filter.pop().is_some(),
            _ => {
                if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
                    return false;
                }
                match keystroke.key_char.as_ref() {
                    Some(ch) => {
                        self.filter.push_str(ch);
                        true
                    }
                    None => false,
                }
            }
        }
    }
}

impl Render for MonitorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // background alpha: the focused window shows the most
        // wallpaper, inactive windows dim toward solid for contrast
        // (kuma-term/src/view.rs). Picked here because render is the
        // one place that sees the window; a focus flip repaints on
        // the next beat at the latest.
        theme::set_window_opacity(if window.is_window_active() {
            theme::BG_OPACITY_FOCUSED
        } else {
            theme::BG_OPACITY_UNFOCUSED
        });
        let cpu_now = self.history.last().copied().unwrap_or(0.);
        let this = cx.weak_entity();
        // the table's rows for this beat: the filter decides what hides
        let items = self.visible_items();
        let filtering = !self.filter.is_empty();
        let count = items.len();
        let total_procs: usize = self.groups.iter().map(|group| group.members.len()).sum();
        // the 'static list closure moves these in: Arc snapshots, not
        // a per-render deep clone of the whole table
        let groups = self.groups_snap.clone();
        let group_hists = self.hists_snap.clone();
        let cursor = self.cursor;
        self.painted = true;
        let this2 = this.clone();

        div()
            .size_full()
            .flex()
            .flex_col()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                // the armed card eats the keyboard first: enter seals
                // the SIGKILL, any other key unwinds the arm so it
                // cannot outlive its moment
                if this.kill_confirm.is_some() {
                    match event.keystroke.key.as_str() {
                        "enter" => this.confirm_sigkill(cx),
                        _ => {
                            this.kill_confirm = None;
                            cx.notify();
                        }
                    }
                    return;
                }
                match event.keystroke.key.as_str() {
                    "up" | "down" => {
                        this.move_cursor(event.keystroke.key == "down", cx);
                        return;
                    }
                    // ctrl+k, not plain k: plain typing is the
                    // filter's, and a process monitor that killed
                    // what you meant to search for would be a trap
                    "k" if event.keystroke.modifiers.control => {
                        this.kill_key(cx);
                        return;
                    }
                    _ => {}
                }
                if this.filter_key(&event.keystroke) {
                    this.save_state();
                    cx.notify();
                }
            }))
            .bg(theme::bg())
            .text_color(theme::text())
            .text_size(px(13.))
            .children(
                // the CSD bar: the platform's behavior, the monitor's
                // colors. It folds away while the window is tiled: the
                // tile frames the window, and the vitals want the row.
                // niri tells it through the IPC (its xdg tiled states
                // are a CSD hint only); other compositors answer
                // through the states where they send them
                (!(kuma_platform::titlebar::window_is_tiled(window) || self.niri_tiled)).then(
                    || {
                        kuma_platform::titlebar::bar(
                            "Kamuy".into(),
                            window.window_controls(),
                            kuma_platform::titlebar::TitlebarIcons {
                                minimize: "icons/titlebar_minimize.svg".into(),
                                maximize: "icons/titlebar_maximize.svg".into(),
                                close: "icons/titlebar_close.svg".into(),
                            },
                            kuma_platform::titlebar::TitlebarColors {
                                background: theme::surface(),
                                border: theme::border(),
                                title: theme::text_dim(),
                                ink: theme::text_dim(),
                                hover: theme::row_hover(),
                            },
                        )
                    },
                ),
            )
            .child(self.vitals_area(cpu_now, &this))
            .child(
                // the header floats: no rule above or below, its dim
                // text and the row padding do the separating
                div()
                    .flex()
                    .items_center()
                    .px_3()
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
                    .child(if filtering && items.is_empty() {
                        // a silent table is never ambiguous
                        div()
                            .py_2()
                            .text_color(theme::text_dim())
                            .child(format!("no processes match \"{}\"", self.filter))
                    } else {
                        div().flex_1().flex().flex_col().child(
                            uniform_list("proc-rows", count, move |range, _, _| {
                                items[range.clone()]
                                    .iter()
                                    .enumerate()
                                    .map(|(offset, item)| {
                                        let ix = range.start + offset;
                                        list_item(
                                            item,
                                            ix,
                                            cursor,
                                            &groups,
                                            &group_hists,
                                            &this2,
                                        )
                                    })
                                    .collect()
                            })
                            .flex_1()
                            .track_scroll(&self.scroll),
                        )
                    }),
            )
            // the armed SIGKILL, a card above the footer: the row it
            // would kill named whole, the two keys that answer. Any
            // other key unwound the arm already (the listener), so
            // the card never outlives its moment.
            .children(self.kill_confirm.as_ref().map(|(name, pid)| {
                div()
                    .flex()
                    .items_center()
                    .mx_3()
                    .my_1()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .bg(theme::surface())
                    .border_1()
                    .border_color(theme::accent())
                    .rounded_sm()
                    .text_size(px(11.))
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .child(format!("SIGKILL {name} (pid {pid})? the ask did not land")),
                    )
                    .child(
                        div()
                            .text_color(theme::text_dim())
                            .child("enter confirms \u{b7} any other key steps back"),
                    )
            }))
            .child(self.footer(total_procs))
    }
}

impl MonitorView {
    /// The vitals row: five quiet cards, label over value over
    /// detail. The CPU, MEM, NET and DISK cards are the click
    /// surfaces (each opens a card: the cores detail between rows
    /// one and two, memory's anatomy and the interfaces and disks
    /// below the lattice); the rest read and stay still.
    fn vitals_area(&self, cpu_now: f32, this: &WeakEntity<MonitorView>) -> Div {
        let v = &self.vitals;
        let mem_pct = if self.mem_total > 0 {
            self.mem_used as f64 / self.mem_total as f64
        } else {
            0.
        };

        // -- row one: the processor's story, the cpu card the hero
        // at two columns wide (its sparkline earns the longer window
        // that keeps bars from going fat), temp beside at one. The
        // rows are grids sharing one three-column lattice, so a
        // single-width card is the same width in every row by
        // construction; flex distribution kept drifting by pixels.
        let cpu_card = self.card_base("CPU").col_span(2)
            .child(
                div()
                    .text_size(px(14.))
                    .truncate()
                    .debug_selector(|| "val-cpu".into())
                    .child(format!("{cpu_now:.1}%")),
            )
            .child(spark(&self.history, None, CARD_SPARK_H))
            .child(
                div()
                    .text_size(px(9.))
                    .text_color(theme::text_dim())
                    .truncate()
                    .child({
                        let cores = v.core_count;
                        if cores > 0 {
                            format!("{cores} cores \u{b7} click for each")
                        } else {
                            " ".to_string()
                        }
                    }),
            )
            .id("card-cpu")
            .on_click({
                let this = this.clone();
                move |_, _, app| {
                    let _ = this.update(app, |this, cx| {
                        this.show_cores = !this.show_cores;
                        cx.notify();
                    });
                }
            })
            .debug_selector(|| "card-cpu".into());

        let temp_card = self
            .card_base("TEMP")
            .child(
                div()
                    .text_size(px(14.))
                    .truncate()
                    .child(match v.cpu_temp {
                        Some(t) => format!("{t:.1}\u{b0}C"),
                        None => "\u{2014}".to_string(),
                    }),
            )
            .children((!self.temp_hist.is_empty()).then(|| spark(&self.temp_hist, Some(100.), CARD_SPARK_H)))
            .child(
                div()
                    .text_size(px(9.))
                    .text_color(theme::text_dim())
                    .truncate()
                    .child({
                        match (&v.cpu_temp, self.temp_hist.len() < CARD_HIST) {
                            (Some(_), true) => "warming the sparkline".to_string(),
                            (Some(_), false) => "package sensor".to_string(),
                            (None, _) => "no sensor found".to_string(),
                        }
                    }),
            )
            .debug_selector(|| "card-temp".into());

        // -- row two: the scheduler's story (suffering, queueing,
        // who is stuck), the diagnosis row, in the same clothes as
        // the state rows
        let mut psi_card = self.card_base("PRESSURE");
        if v.psi.iter().all(Option::is_none) {
            psi_card = psi_card.child(
                div()
                    .text_size(px(9.))
                    .text_color(theme::text_dim())
                    .child("not exposed by this kernel"),
            );
        } else {
            psi_card = psi_card.child(
                div().flex().flex_col().gap_1().justify_center().h_full().children(
                    PSI_ROWS.map(|(name, slot)| psi_row(name, v.psi[slot])),
                ),
            );
        }
        let psi_card = psi_card.debug_selector(|| "card-psi".into());

        // load: the familiar decayed count. Each window gets a meter
        // scaled against the core count, so a full bar means one
        // runnable task per core and the figure rides along.
        let cores = v.core_count.max(1) as f32;
        let load_card = {
            let mut card = self.card_base("LOAD");
            if v.core_count > 0 {
                card = card.child(
                    div().flex().flex_col().gap_1().justify_center().h_full().children(
                        ["1m", "5m", "15m"]
                            .iter()
                            .zip(v.load)
                            .map(|(window, value)| {
                                meter_row(
                                    window,
                                    format!("{value:.2}"),
                                    (value / cores).clamp(0., 1.),
                                    value > cores,
                                )
                            }),
                    ),
                );
            }
            card.child(
                div()
                    .text_size(px(9.))
                    .text_color(theme::text_dim())
                    .truncate()
                    .child(format!("{} cores", v.core_count)),
            )
            .debug_selector(|| "card-load".into())
        };

        // procs: the same counts the load average decays, but right
        // now. Blocked is the abnormal one: uninterruptible waits are
        // io or a driver not answering, so it wears the light.
        let procs_card = self
            .card_base("PROCS")
            .child(
                div()
                    .text_size(px(14.))
                    .truncate()
                    .child(format!("{} running", v.procs_running)),
            )
            .child(spark(&self.procs_hist, None, CARD_SPARK_H))
            .child(
                div()
                    .text_size(px(9.))
                    .text_color(if v.procs_blocked > 0 {
                        theme::accent()
                    } else {
                        theme::text_dim()
                    })
                    .truncate()
                    .child(format!("{} blocked", v.procs_blocked)),
            )
            .debug_selector(|| "card-procs".into());

        // -- row two: the memory and the wires. The memory card
        // reads the same heights as its neighbors (value, spark,
        // detail) with the bar folded into the value line, where it
        // spans the leftover width like a headline's underline.
        let mem_card = self
            .card_base("MEM")
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(14.))
                            .truncate()
                            .debug_selector(|| "val-mem".into())
                            .child(format!("{:.0}%", mem_pct * 100.)),
                    )
                    .child(
                        meter(mem_pct as f32, theme::accent(), Some("mem-fill"))
                            .flex_1()
                            .min_w(px(0.))
                            .debug_selector(|| "mem-track".into()),
                    ),
            )
            .child(spark(&self.mem_hist, Some(100.), CARD_SPARK_H))
            .child(
                div()
                    .text_size(px(9.))
                    .text_color(theme::text_dim())
                    .truncate()
                    .debug_selector(|| "sub-mem".into())
                    .child({
                        let mut text = format!(
                            "{} / {}",
                            sample::human_mem(self.mem_used),
                            sample::human_mem(self.mem_total)
                        );
                        if self.swap_total > 0 {
                            text.push_str(&format!(
                                " \u{b7} swap {}",
                                sample::human_mem(self.swap_used)
                            ));
                        }
                        text.push_str(" \u{b7} click for detail");
                        text
                    }),
            )
            .id("card-mem")
            .on_click({
                let this = this.clone();
                move |_, _, app| {
                    let _ = this.update(app, |this, cx| {
                        this.show_mem = !this.show_mem;
                        cx.notify();
                    });
                }
            })
            .debug_selector(|| "card-mem".into());

        let disk_card = self
            .card_base("DISK")
            .child(
                div()
                    .text_size(px(14.))
                    .truncate()
                    .debug_selector(|| "val-disk".into())
                    .child(rate(v.io_read + v.io_write)),
            )
            .child(spark(&self.disk_hist, None, CARD_SPARK_H))
            .child(
                div()
                    .text_size(px(9.))
                    .text_color(theme::text_dim())
                    .truncate()
                    .child(format!(
                        "R {} \u{b7} W {} \u{b7} click for devices",
                        rate(v.io_read),
                        rate(v.io_write)
                    )),
            )
            .id("card-disk")
            .on_click({
                let this = this.clone();
                move |_, _, app| {
                    let _ = this.update(app, |this, cx| {
                        this.show_disks = !this.show_disks;
                        cx.notify();
                    });
                }
            })
            .debug_selector(|| "card-disk".into());

        let net_card = self
            .card_base("NET")
            .child(
                div()
                    .text_size(px(14.))
                    .truncate()
                    .debug_selector(|| "val-net".into())
                    .child(rate(v.net_rx + v.net_tx)),
            )
            .child(spark(&self.net_hist, None, CARD_SPARK_H))
            .child(
                div()
                    .text_size(px(9.))
                    .text_color(theme::text_dim())
                    .truncate()
                    .child(format!(
                        "\u{2193} {} \u{b7} \u{2191} {} \u{b7} click for devices",
                        rate(v.net_rx),
                        rate(v.net_tx)
                    )),
            )
            .id("card-net")
            .on_click({
                let this = this.clone();
                move |_, _, app| {
                    let _ = this.update(app, |this, cx| {
                        this.show_nets = !this.show_nets;
                        cx.notify();
                    });
                }
            })
            .debug_selector(|| "card-net".into());

        div()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .pt_2()
            .pb_2()
            .debug_selector(|| "vitals-area".into())
            .child(
                div()
                    .grid()
                    .grid_cols(3)
                    .gap_2()
                    .debug_selector(|| "vitals-strip".into())
                    .child(cpu_card)
                    .child(temp_card),
            )
            .children((self.show_cores && !self.core_usage.is_empty()).then(|| self.cores_row()))
            .child(
                div()
                    .grid()
                    .grid_cols(3)
                    .gap_2()
                    .debug_selector(|| "vitals-strip-2".into())
                    .child(psi_card)
                    .child(load_card)
                    .child(procs_card),
            )
            .child(
                div()
                    .grid()
                    .grid_cols(3)
                    .gap_2()
                    .debug_selector(|| "vitals-strip-3".into())
                    .child(mem_card)
                    .child(disk_card)
                    .child(net_card),
            )
            .children((self.show_mem && self.vitals.mem.total > 0).then(|| self.mem_detail()))
            .children((self.show_disks && !self.vitals.disks.is_empty()).then(|| {
                self.devices_card(
                    "DISKS",
                    &self.vitals.disks,
                    self.vitals.io_read + self.vitals.io_write,
                )
            }))
            .children((self.show_nets && !self.vitals.nets.is_empty()).then(|| {
                self.devices_card(
                    "INTERFACES",
                    &self.vitals.nets,
                    self.vitals.net_rx + self.vitals.net_tx,
                )
            }))
    }

    /// The per-core drop-down: the cards' chrome at full width, one
    /// column per core, the bar lit accent past sixty percent (the
    /// LOAD card's lit language: busy is worth finding), the core's
    /// index under it. Opened by clicking the CPU card and hung
    /// directly beneath it, so it reads as that card's own detail.
    fn cores_row(&self) -> Div {
        self.dropdown_base("CORES")
            .h(px(96.))
            .child(
                div().flex_1().flex().items_end().children(
                    self.core_usage.iter().enumerate().map(|(ix, pct)| {
                        div()
                            .flex_1()
                            .h_full()
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_end()
                            .gap_1()
                            .child(
                                div()
                                    .w(px(6.))
                                    .h(px((pct / 100. * 48.).clamp(1., 48.)))
                                    .bg(if *pct >= 60. {
                                        theme::accent()
                                    } else {
                                        theme::ghost()
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(8.))
                                    .text_color(theme::text_dim())
                                    .child(ix.to_string()),
                            )
                    }),
                ),
            )
    }

    /// A drop-down's frame: the cards' chrome at full width, the
    /// height content-sized (it hangs below the lattice, so the
    /// strips' ninety-six rhythm does not bind it; the cores card
    /// pins its own).
    fn dropdown_base(&self, label: &str) -> Div {
        div()
            .w_full()
            .min_w(px(0.))
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .py_2()
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
            .rounded_sm()
            .child(
                div()
                    .text_size(px(9.))
                    .text_color(theme::text_dim())
                    .child(label.to_string()),
            )
    }

    /// The memory drop-down's rows: the two sections that answer to
    /// the card above (in use is the card's own used figure,
    /// available is total minus it, so the pair always closes to the
    /// total), then the anatomy under each. Other takes what the
    /// in-use side's named buckets miss (gpu buffers, boot
    /// reservations, unreclaimed fragments); cache is the
    /// headroom's mass, mostly page cache. Rows are
    /// (label, bytes, share of total, is a section).
    fn mem_rows(&self) -> Vec<(&'static str, u64, f32, bool)> {
        let mem = &self.vitals.mem;
        let total = self.mem_total as f32;
        let frac = |bytes: u64| {
            if total > 0. {
                bytes as f32 / total
            } else {
                0.
            }
        };
        let available = self.mem_total.saturating_sub(self.mem_used);
        let other = self
            .mem_used
            .saturating_sub(mem.apps)
            .saturating_sub(mem.shared)
            .saturating_sub(mem.kernel);
        let cache = available.saturating_sub(mem.free);
        vec![
            ("IN USE", self.mem_used, frac(self.mem_used), true),
            ("APPS", mem.apps, frac(mem.apps), false),
            ("SHARED", mem.shared, frac(mem.shared), false),
            ("KERNEL", mem.kernel, frac(mem.kernel), false),
            ("OTHER", other, frac(other), false),
            ("AVAILABLE", available, frac(available), true),
            ("FREE", mem.free, frac(mem.free), false),
            ("CACHE", cache, frac(cache), false),
        ]
    }

    /// The memory drop-down: two sections closing against the card
    /// above (the card's own used figure, then the headroom), each
    /// with its anatomy underneath. Section meters carry the card's
    /// accent (in use) and a ghost fill (available); the buckets are
    /// ghost throughout, nothing here is distress.
    fn mem_detail(&self) -> Div {
        self.dropdown_base("MEMORY")
            .children(self.mem_rows().into_iter().map(
                |(label, bytes, share, section)| {
                    let ink = if section {
                        theme::text()
                    } else {
                        theme::text_dim()
                    };
                    let fill = if label == "IN USE" {
                        theme::accent()
                    } else {
                        theme::ghost()
                    };
                    div()
                        .h(px(20.))
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .w(px(48.))
                                .text_size(px(9.))
                                .text_color(ink)
                                .child(label.to_string()),
                        )
                        .child(meter(share, fill, None).flex_1().min_w(px(0.)))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .text_size(px(9.))
                                .text_color(ink)
                                .truncate()
                                .child(sample::human_mem(bytes)),
                        )
                },
            ))
    }

    /// The per-device drop-down: the cards' chrome full width, one
    /// meter row per interface or disk in the meter rows' language:
    /// dim nine-pixel label, a ghost meter of the device's share of
    /// the beat's total, the down and up figures closing the line.
    fn devices_card(&self, label: &str, devs: &[(String, f64, f64)], total: f64) -> Div {
        self.dropdown_base(label).children(devs.iter().map(|(name, down, up)| {
            div()
                .h(px(20.))
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .w(px(72.))
                        .truncate()
                        .text_size(px(9.))
                        .text_color(theme::text_dim())
                        .child(name.clone()),
                )
                .child(
                    meter(
                        if total > 0. { ((down + up) / total) as f32 } else { 0. },
                        theme::ghost(),
                        None,
                    )
                    .flex_1()
                    .min_w(px(0.)),
                )
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .text_size(px(9.))
                        .text_color(theme::text_dim())
                        .truncate()
                        .child(format!("\u{2193} {} \u{b7} \u{2191} {}", rate(*down), rate(*up))),
                )
        }))
    }

    /// One vitals card's frame: rounded, hairline border, dim label,
    /// overflow that can never escape (flex's automatic minimum would
    /// otherwise let the content push the card past the strip).
    fn card_base(&self, label: &str) -> Div {
        div()
            .flex_1()
            .min_w(px(0.))
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .py_2()
            .h(px(96.))
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
            .rounded_sm()
            .child(
                div()
                    .text_size(px(9.))
                    .text_color(theme::text_dim())
                    .child(label.to_string()),
            )
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
            .child(if active {
                // the direction is fixed per key; the arrow just
                // announces it
                format!(
                    "{} {}",
                    col.label(),
                    if col.key().sorts_down() { "\u{25be}" } else { "\u{25b4}" }
                )
            } else {
                col.label().to_string()
            });
        // while a filter is active it lives in the name header:
        // state that must never be invisible, at zero layout cost
        let cell = if col == Col::Name && !self.filter.is_empty() {
            cell.flex()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .truncate()
                        .text_color(theme::text_dim())
                        .child(format!(" \u{b7} \"{}\" (esc clears)", self.filter)),
                )
        } else {
            cell
        };
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

    /// The line under the table: the list's own counts on the left,
    /// the machine's age on the right. A fresh right-click copy
    /// speaks there for two seconds, and so does a fresh kill: the
    /// signal's line, delivery or excuse alike (a beat re-renders
    /// every second, so the confirmation retires itself). Load has a
    /// card; the build stamp answers to --version, not to every
    /// frame.
    fn footer(&self, processes: usize) -> Div {
        let apps = self.groups.iter().filter(|g| !g.background).count();
        let fresh = |flash: &Option<(String, Instant)>| {
            flash
                .as_ref()
                .filter(|(_, at)| at.elapsed() < Duration::from_secs(2))
                .map(|(line, _)| line.clone())
        };
        let lead = fresh(&self.signal_flash)
            .or_else(|| fresh(&self.copied_flash))
            .unwrap_or_else(|| format!("{apps} apps \u{b7} {processes} processes"));
        div()
            .flex()
            .items_center()
            .border_t_1()
            .border_color(theme::border())
            .py_1()
            .px_3()
            .text_size(px(11.))
            .text_color(theme::text_dim())
            .child(lead)
            .child(div().flex_1())
            .child(format!("up {}", human_uptime(self.vitals.uptime_secs)))
    }
}

impl Focusable for MonitorView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// A micro history: the buffer's samples as one quiet line of
/// bars (each history vec is already capped to its card's
/// window). Scaled to the window's own peak (never the global
/// 100%, or an idle line would be invisible) unless a fixed
/// ceiling is given, as the temperature needs: its drama lives in
/// single degrees, and peak-scaling would lie about the slope.
/// The height is the caller's: the cards announce at eighteen,
/// the rows whisper.
fn spark(history: &[f32], ceiling: Option<f32>, height: f32) -> Div {
    let peak = ceiling.unwrap_or_else(|| history.iter().copied().fold(1.0_f32, f32::max));
    let bars = history
        .iter()
        .map(|v| {
            // a non-finite sample has no length: collapse it to the
            // floor rather than let NaN ride px() into layout
            let v = if v.is_finite() { *v } else { 0. };
            div()
                .flex_1()
                .min_w(px(1.))
                .h(px((v / peak * height).clamp(1., height)))
                .bg(theme::ghost())
        })
        .collect::<Vec<_>>();
    div().h(px(height)).w_full().flex().items_end().children(bars)
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

/// One signal to one pid: the monitor's single act on the machine
/// it watches. The answer is a footer line covering both outcomes,
/// the delivery or why nothing was sent ("already gone", the pid's
/// ESRCH read charitably; the row dies at the next beat anyway).
fn send_signal(pid: u32, sig: i32, name: String) -> String {
    // SAFETY: kill takes a pid and a signal number and nothing else
    let done = unsafe { libc::kill(pid as libc::pid_t, sig) };
    if done == 0 {
        let kind = if sig == libc::SIGKILL { "SIGKILL" } else { "SIGTERM" };
        format!("sent {kind} to {name} (pid {pid})")
    } else {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            format!("{name} (pid {pid}): already gone")
        } else {
            format!("{name} (pid {pid}): {err}")
        }
    }
}

/// The state file's parse: sort and filter, nothing else. Unknown
/// keys and malformed values die quietly, so a future key or a
/// hand-edited file cannot break an older reader; a duplicated line
/// lets the last one win.
fn parse_state(text: &str) -> (Option<SortKey>, Option<String>) {
    let mut sort = None;
    let mut filter = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "sort" => sort = SortKey::from_name(value),
            "filter" => filter = Some(value.to_string()),
            _ => {}
        }
    }
    (sort, filter)
}

fn list_item(
    item: &Item,
    ix: usize,
    cursor: Option<usize>,
    groups: &[AppGroup],
    hists: &HashMap<String, Vec<f32>>,
    this: &WeakEntity<MonitorView>,
) -> Stateful<Div> {
    let aimed = cursor == Some(ix);
    match item {
        Item::Group {
            index,
            key,
            open,
            dim,
        } => group_row(
            &groups[*index],
            ix,
            key,
            *open,
            *dim,
            aimed,
            hists.get(key).map(|ring| ring.as_slice()),
            this,
        ),
        Item::Member { group, member } => {
            member_row(&groups[*group].members[*member], ix, aimed, this)
        }
        Item::BgSummary {
            open,
            count,
            cpu,
            mem,
        } => background_row(*open, *count, *cpu, *mem, this),
    }
}

/// One group row plus its visible members: the whole roster when the
/// hit is All (the expand state still decides), or just the matching
/// members when the query found them (the header pulled along,
/// effectively open so the triangle never lies).
fn push_group_rows(
    items: &mut Vec<Item>,
    groups: &[AppGroup],
    index: usize,
    hit: &GroupHit,
    dim: bool,
    expanded: &HashSet<String>,
) {
    let group = &groups[index];
    let (open, members): (bool, Vec<usize>) = match hit {
        GroupHit::All => (
            expanded.contains(&group.key),
            (0..group.members.len()).collect(),
        ),
        GroupHit::Members(hits) => (true, hits.clone()),
    };
    items.push(Item::Group {
        index,
        key: group.key.clone(),
        open,
        dim,
    });
    if open {
        for member in members {
            items.push(Item::Member { group: index, member });
        }
    }
}

/// The app's row: its sums, its process count, and the fold for the
/// roster. The cpu figure carries the group's recent history as a
/// spark strip at its cell's bottom, the figure floating clear
/// above: the row-shaped echo of the cards' value-over-spark
/// pairing. Full bar is one core's worth, fixed rather than
/// peak-scaled, so sparks are comparable row to row and a hog's row
/// reads busy before its number does. Clicking anywhere in it
/// unfolds the members in place.
fn group_row(
    group: &AppGroup,
    ix: usize,
    key: &str,
    open: bool,
    dim: bool,
    aimed: bool,
    hist: Option<&[f32]>,
    this: &WeakEntity<MonitorView>,
) -> Stateful<Div> {
    let click_key = key.to_string();
    let this = this.clone();
    let copy_this = this.clone();
    let copy_key = click_key.clone();
    let aim_ix = ix;
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
    if let Some(active) = disk_suffix(group.disk) {
        name = name.child(
            div()
                .truncate()
                .text_color(theme::text_dim())
                .child(active),
        );
    }
    div()
        // namespaced ids: the group key is user-influenced content (the
        // app's name), and a bare `m-{pid}` on the member rows could
        // collide with a group literally named that
        .id(SharedString::from(format!("group/{key}")))
        .h(px(26.))
        .w_full()
        .flex()
        .items_center()
        .debug_selector(|| "row-line".into())
        .bg(if group.has_focused {
            theme::row_selected()
        } else if aimed {
            theme::row_hover()
        } else {
            theme::clear()
        })
        // the focused row is already lit, and so is the cursor's;
        // the others lift on hover
        .when(!group.has_focused && !aimed, |row| {
            row.hover(|style| style.bg(theme::row_hover()))
        })
        .on_mouse_down(MouseButton::Right, move |_, _, app| {
            let _ = copy_this.update(app, |this, cx| {
                // the copy line is built here, on the click, not per
                // render per row: it was the only consumer of a
                // string built for every visible row every beat
                let Some(group) = this.groups.iter().find(|group| group.key == copy_key) else {
                    return;
                };
                cx.write_to_clipboard(ClipboardItem::new_string(group_line(group)));
                this.copied_flash = Some((group.name.clone(), Instant::now()));
                cx.notify();
            });
        })
        .on_click(move |_, _, app| {
            let _ = this.update(app, |this, cx| {
                if !this.expanded_apps.remove(&click_key) {
                    this.expanded_apps.insert(click_key.clone());
                }
                // a click aims too: the cursor lands on the row it
                // landed on, so the kill keys answer to the mouse
                // and the arrows alike
                this.cursor = Some(aim_ix);
                cx.notify();
            });
        })
        .child(apply_spec(name, Col::Name))
        .child(apply_spec(div(), Col::Pid))
        .child(apply_spec(
            // the ring lives in the column it explains, as a strip
            // at the cell's bottom: the figure floats clear above
            // it, the row-shaped echo of the cards' value-over-spark
            div()
                .relative()
                .h_full()
                .flex()
                .items_center()
                .children(hist.map(|ring| {
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .h(px(ROW_SPARK_H))
                        .flex()
                        .items_end()
                        .overflow_hidden()
                        .debug_selector(|| "row-spark".into())
                        .child(spark(ring, Some(100.), ROW_SPARK_H))
                }))
                .child(div().child(format!("{:.1}", group.cpu)).text_color(text)),
            Col::Cpu,
        ))
        .child(
            apply_spec(
                div()
                    .child(sample::human_mem(group.mem))
                    .text_color(theme::text_dim()),
                Col::Mem,
            ),
        )
}

/// The background summary: the session's plumbing, folded. Built on
/// the group row's anatomy (triangle at the same indent, the sums in
/// the same columns as the headers they answer to), all of it dim:
/// it is a footnote, not an app.
fn background_row(
    open: bool,
    count: usize,
    cpu: f32,
    mem: u64,
    this: &WeakEntity<MonitorView>,
) -> Stateful<Div> {
    let this = this.clone();
    let copy_this = this.clone();
    let line = format!(
        "{count} background processes  {cpu:.1}%  {}",
        sample::human_mem(mem)
    );
    let name = div()
        .flex()
        .gap_2()
        .truncate()
        .child(div().child(if open { "\u{25be}" } else { "\u{25b8}" }))
        .child(
            div()
                .truncate()
                .child(format!("{count} background processes")),
        );
    div()
        .id("bg-summary")
        .h(px(26.))
        .w_full()
        .flex()
        .items_center()
        .text_color(theme::text_dim())
        .debug_selector(|| "row-line".into())
        .hover(|style| style.bg(theme::row_hover()))
        .on_mouse_down(MouseButton::Right, move |_, _, app| {
            let _ = copy_this.update(app, |this, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(line.clone()));
                this.copied_flash = Some(("background".to_string(), Instant::now()));
                cx.notify();
            });
        })
        .on_click(move |_, _, app| {
            let _ = this.update(app, |this, cx| {
                this.show_background = !this.show_background;
                cx.notify();
            });
        })
        .child(apply_spec(name.flex_1(), Col::Name))
        .child(apply_spec(div(), Col::Pid))
        .child(apply_spec(div().child(format!("{cpu:.1}")), Col::Cpu))
        .child(apply_spec(div().child(sample::human_mem(mem)), Col::Mem))
}

/// A member row: the window's title when it has one (the app's name
/// is the group header now: saying it twice is noise), the process
/// name when not; indented a step under its app. A left click aims
/// the keyboard cursor at it (the kill keys read the cursor); the
/// cursor wears the hover color steady where it sits, under the
/// focused window's own stronger highlight.
fn member_row(row: &ProcRow, ix: usize, aimed: bool, this: &WeakEntity<MonitorView>) -> Stateful<Div> {
    let aim_this = this.clone();
    let copy_this = this.clone();
    let copy_pid = row.pid;
    let click_ix = ix;
    let windowed = row.window.is_some();
    let focused = row.window.as_ref().is_some_and(|w| w.focused);
    let lead = member_lead(row);
    div()
        .id(SharedString::from(format!("member/{}", row.pid)))
        .h(px(26.))
        .w_full()
        .flex()
        .items_center()
        .debug_selector(|| "row-line".into())
        .bg(if focused {
            theme::row_selected()
        } else if aimed {
            theme::row_hover()
        } else {
            theme::clear()
        })
        .when(!focused && !aimed, |member| {
            member.hover(|style| style.bg(theme::row_hover()))
        })
        .on_mouse_down(MouseButton::Left, move |_, _, app| {
            let _ = aim_this.update(app, |this, cx| {
                this.cursor = Some(click_ix);
                cx.notify();
            });
        })
        .on_mouse_down(MouseButton::Right, move |_, _, app| {
            let _ = copy_this.update(app, |this, cx| {
                // same shape as the group row: the copy line is the
                // click's work, not every beat's
                let Some(row) = this.groups.iter().find_map(|group| {
                    group.members.iter().find(|member| member.pid == copy_pid)
                }) else {
                    return;
                };
                cx.write_to_clipboard(ClipboardItem::new_string(member_line(row)));
                this.copied_flash = Some((member_lead(row), Instant::now()));
                cx.notify();
            });
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

/// The row as one clipboard line: what the row says, in the order
/// the columns say it.
fn group_line(group: &AppGroup) -> String {
    let pids: Vec<String> = group.members.iter().map(|m| m.pid.to_string()).collect();
    format!(
        "{}  {:.1}%  {}  pids {}",
        group.name,
        group.cpu,
        sample::human_mem(group.mem),
        pids.join(", ")
    )
}

fn member_line(row: &ProcRow) -> String {
    format!(
        "{}  {:.1}%  {}  pid {}",
        member_lead(row),
        row.cpu,
        sample::human_mem(row.mem),
        row.pid
    )
}

/// The identity's activity whisper: what the app is doing to the
/// disk right now, shown only while it does enough to matter.
fn disk_suffix(disk: u64) -> Option<String> {
    (disk >= DISK_SUFFIX_AT)
        .then(|| format!("\u{b7} {}/s", sample::human_mem(disk)))
}

/// Bytes per second at the table's width.
fn rate(bps: f64) -> String {
    format!("{}/s", sample::human_mem(bps.max(0.) as u64))
}

/// A slim horizontal meter: the fill and the remainder split the
/// track by flex-grow, so the fraction is layout-guaranteed and no
/// width arithmetic can disagree with the parent. The fill can carry
/// a debug selector for the bounds tests.
fn meter(frac: f32, fill: Rgba, fill_sel: Option<&str>) -> Div {
    let frac = frac.clamp(0., 1.);
    let mut fill_div = div().h_full().flex_grow(frac).min_w(px(0.)).bg(fill);
    if let Some(sel) = fill_sel {
        fill_div = fill_div.debug_selector(move || sel.into());
    }
    div()
        .h(px(5.))
        .w_full()
        .flex()
        .bg(theme::inset())
        .rounded_full()
        .overflow_hidden()
        .child(fill_div)
        .child(div().h_full().flex_grow(1. - frac).min_w(px(0.)))
}

/// The per-device drop-down: one slim line per interface or disk
/// under the strip, opened by clicking the NET or DISK card. Built
/// in the meter rows' language: dim nine-pixel label, a ghost meter
/// of the device's share of the beat's total, the down and up
/// figures closing the line. The meter answers "where is the
/// traffic" at a glance; the figures carry the split.


/// The pressure card's one row: a three-letter label, a meter whose
/// fill lights when this resource stalls tasks, the avg10 figure.
/// One labeled meter row: a short label, a track whose fill lights
/// when the resource is in distress, the figure at the right. The
/// pressure and load cards share it, so their meters read the same.
fn meter_row(label: &str, value: String, frac: f32, lit: bool) -> Div {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(
            div()
                .w(px(24.))
                .text_size(px(9.))
                .text_color(theme::text_dim())
                .child(label.to_string()),
        )
        .child(
            meter(
                frac,
                if lit {
                    theme::accent()
                } else {
                    theme::ghost()
                },
                None,
            )
            .flex_1()
            .min_w(px(0.)),
        )
        .child(
            div()
                .w(px(28.))
                .text_size(px(9.))
                .text_color(if lit {
                    theme::text()
                } else {
                    theme::text_dim()
                })
                .truncate()
                .child(value),
        )
}

fn psi_row(name: &str, pressure: Option<f32>) -> Div {
    let p = pressure.unwrap_or(0.);
    let strained = p > 10.;
    meter_row(
        name,
        match pressure {
            Some(p) => format!("{p:.1}"),
            None => "\u{2014}".to_string(),
        },
        p / 100.,
        strained,
    )
}

/// Slide a sample into a card's history window.
fn push_hist(buf: &mut Vec<f32>, value: f32, cap: usize) {
    buf.push(value);
    if buf.len() > cap {
        buf.remove(0);
    }
}

/// "3d 4h 12m", "4h 12m", "12m": the machine's age at a glance.
fn human_uptime(secs: f64) -> String {
    let secs = secs as u64;
    let mins = (secs / 60) % 60;
    let hours = (secs / 3600) % 24;
    let days = secs / 86400;
    if days > 0 {
        format!("{days}d {hours}h {mins}m")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

// ---------- the shell's palette handshake ----------

/// Read the shell's published palette, or the built-ins when absent.
fn read_palette() -> kuma_platform::palette::Palette {
    kuma_platform::palette::read().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_leads_with_the_title_then_the_name() {
        let mut row = ProcRow {
            pid: 1,
            parent: None,
            name: "electron".into(),
            cpu: 0.,
            mem: 0,
            disk: 0,
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

    #[test]
    fn rows_copy_as_one_greppable_line() {
        let zen = ProcRow {
            pid: 100,
            parent: None,
            name: "zen".into(),
            cpu: 12.34,
            mem: 1_234_567_890,
            disk: 0,
            window: Some(sample::WindowRef {
                app_id: Some("zen".into()),
                title: "blog post".into(),
                focused: false,
            }),
        };
        let helper = ProcRow {
            pid: 101,
            parent: Some(100),
            name: "zen-worker".into(),
            cpu: 3.,
            mem: 1000,
            disk: 0,
            window: None,
        };
        // the group row: the app's name and sums, every member pid
        let group = AppGroup {
            key: "zen".into(),
            name: "Zen".into(),
            cpu: 15.34,
            mem: 1_234_568_890,
            disk: 0,
            background: false,
            has_focused: false,
            members: vec![zen.clone(), helper],
        };
        assert_eq!(group_line(&group), "Zen  15.3%  1.1 GiB  pids 100, 101");
        // the member row: the lead it displays (the title here)
        assert_eq!(member_line(&zen), "blog post  12.3%  1.1 GiB  pid 100");
    }

    #[test]
    fn the_disk_suffix_whispers_only_when_busy() {
        assert_eq!(disk_suffix(0), None);
        assert_eq!(disk_suffix(200 * 1024), Some("\u{b7} 200.0 KiB/s".to_string()));
        assert_eq!(rate(1.5 * 1024. * 1024.), "1.5 MiB/s".to_string());
        assert_eq!(human_uptime(3. * 86400. + 4. * 3600. + 12. * 60.), "3d 4h 12m");
        assert_eq!(human_uptime(4. * 3600. + 12. * 60.), "4h 12m");
        assert_eq!(human_uptime(12. * 60.), "12m");
    }

    #[test]
    fn the_card_histories_cap_at_their_window() {
        let mut hist = Vec::new();
        for sample in 0..60 {
            push_hist(&mut hist, sample as f32, CARD_HIST);
        }
        assert_eq!(hist.len(), CARD_HIST);
        assert_eq!(hist[0], (60 - CARD_HIST) as f32);
        assert_eq!(hist[CARD_HIST - 1], 59.);
        // the cpu card's window runs longer, to match its width
        let mut cpu = Vec::new();
        for sample in 0..(CPU_HIST as u32 + 5) {
            push_hist(&mut cpu, sample as f32, CPU_HIST);
        }
        assert_eq!(cpu.len(), CPU_HIST);
        assert_eq!(cpu[0], 5.);
        assert_eq!(cpu[CPU_HIST - 1], (CPU_HIST + 4) as f32);
    }

    /// The memory drop-down closes against the card: the in-use row
    /// is the card's own used figure, available is what remains, and
    /// every bucket sums into its section, so the rows land on the
    /// total with nothing double counted.
    #[test]
    fn the_memory_rows_close_to_the_card() {
        use std::sync::Arc;

        use crate::stats::MemInfo;

        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(()),
        );
        let mut window = app.open_window(|window, cx| MonitorView::new(window, cx));
        app.run_until_parked();
        window.update(|view, _, _| {
            // this machine's shape, at the beat the user audited
            view.mem_total = 14_868_776 * 1024;
            view.mem_used = 4_471_152 * 1024;
            view.vitals.mem = MemInfo {
                apps: 2_742_076 * 1024,
                shared: 48_716 * 1024,
                kernel: 336_992 * 1024,
                free: 758_992 * 1024,
                ..MemInfo::default()
            };
            let rows = view.mem_rows();
            let row = |name: &str| {
                rows.iter()
                    .find(|r| r.0 == name)
                    .map(|&(_, bytes, _, _)| bytes)
                    .unwrap_or_else(|| panic!("{name} missing"))
            };
            // the sections answer to the card's own arithmetic
            assert_eq!(row("IN USE"), view.mem_used);
            assert_eq!(row("AVAILABLE"), view.mem_total - view.mem_used);
            // the buckets nest: each side sums to its section
            assert_eq!(
                row("APPS") + row("SHARED") + row("KERNEL") + row("OTHER"),
                view.mem_used
            );
            assert_eq!(row("FREE") + row("CACHE"), view.mem_total - view.mem_used);
            // and every byte of ram is spoken for exactly once
            let all: u64 = rows.iter().filter(|r| !r.3).map(|r| r.1).sum();
            assert_eq!(all, view.mem_total);
        });
    }

    /// The row sparks' rings: one sample per beat capped at the
    /// cards' window, and a ring dies with its group, so the map
    /// never gathers dead keys.
    #[test]
    fn the_row_spark_rings_cap_and_die_with_their_group() {
        use std::sync::Arc;

        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(()),
        );
        let mut window = app.open_window(|window, cx| MonitorView::new(window, cx));
        app.run_until_parked();
        let snap_row = |pid: u32, name: &str, cpu: f32| ProcRow {
            pid,
            parent: None,
            name: name.into(),
            cpu,
            mem: 10,
            disk: 0,
            window: None,
        };
        let snap = |rows: Vec<ProcRow>| Snapshot {
            cpu_total: 0.,
            mem_used: 0,
            mem_total: 0,
            swap_used: 0,
            swap_total: 0,
            rows,
        };
        window.update(|view, _, _| {
            for beat in 0..(CARD_HIST + 5) {
                view.apply_snapshot(
                    snap(vec![snap_row(7, "sparky", beat as f32)]),
                    Vitals::default(),
                    Vec::new(),
                );
            }
            let ring = &view.group_hists["sparky"];
            assert_eq!(ring.len(), CARD_HIST);
            // the window slides: the first five beats are gone
            assert_eq!(ring[0], 5.);
            assert_eq!(ring[CARD_HIST - 1], (CARD_HIST + 4) as f32);
            // the group dies, its ring dies with it
            view.apply_snapshot(snap(vec![snap_row(8, "other", 1.)]), Vitals::default(), Vec::new());
            assert!(!view.group_hists.contains_key("sparky"));
            assert!(view.group_hists.contains_key("other"));
        });
    }

    /// The row spark paints under the cpu figure: a short strip at
    /// the cell's bottom, inside the cpu column, never tall enough
    /// to reach the digits.
    #[test]
    fn the_row_spark_paints_under_the_cpu_figure() {
        use std::sync::Arc;

        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(()),
        );
        let mut window = app.open_window(|window, cx| MonitorView::new(window, cx));
        app.run_until_parked();
        window.update(|view, _, cx| {
            view.apply_snapshot(Snapshot {
                cpu_total: 0.,
                mem_used: 0,
                mem_total: 0,
                swap_used: 0,
                swap_total: 0,
                rows: vec![ProcRow {
                    pid: 7,
                    parent: None,
                    name: "sparky".into(),
                    cpu: 40.,
                    mem: 10,
                    disk: 0,
                    window: Some(sample::WindowRef {
                        app_id: Some("Sparky".into()),
                        title: "main".into(),
                        focused: false,
                    }),
                }],
            }, Vitals::default(), Vec::new());
            cx.notify();
        });
        app.run_until_parked();
        let bounds = window.update(|_, window, _| {
            let row = window
                .debug_element_bounds("row-line")
                .expect("the group row never painted");
            let spark = window
                .debug_element_bounds("row-spark")
                .expect("the row spark never painted");
            (row, spark)
        });
        let (row, spark) = bounds;
        // a whisper strip: short, pinned to the row's bottom, and
        // living in the cpu column (72px, left of the mem column's
        // 104)
        assert!(
            spark.size.height <= px(ROW_SPARK_H + 1.),
            "spark is {:?} tall",
            spark.size
        );
        assert!(
            spark.size.width <= px(73.),
            "spark is {:?} wide, the cell is {:?}",
            spark.size,
            row.size
        );
        let row_bottom = row.origin.y + row.size.height;
        assert!(
            (spark.origin.y + spark.size.height - row_bottom).abs() < px(1.5),
            "spark {spark:?} is not anchored at the row's bottom {row:?}"
        );
        let row_right = row.origin.x + row.size.width;
        assert!(
            spark.origin.x >= row_right - px(177.5),
            "spark at {:?} strays out of the cpu column (row {row:?})",
            spark.origin
        );
        assert!(
            spark.origin.x >= row.origin.x - px(0.5)
                && spark.origin.y >= row.origin.y - px(0.5)
                && spark.origin.x + spark.size.width <= row.origin.x + row.size.width + px(0.5)
                && spark.origin.y + spark.size.height <= row.origin.y + row.size.height + px(0.5),
            "spark {spark:?} escaped its row {row:?}"
        );
    }

    /// The overflow the first strip died of: cards that refused to
    /// shrink below their text and a memory bar whose width arithmetic
    /// disagreed with its track. Asserts the layout consequences in a
    /// real painted window with absurd injected values: no card may
    /// escape the strip, and the fill must be exactly the fraction of
    /// its track at 100% and at a quarter.
    #[test]
    fn the_vitals_cards_hold_their_numbers() {
        use std::sync::Arc;

        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(()),
        );
        let mut window = app.open_window(|window, cx| MonitorView::new(window, cx));
        app.run_until_parked();
        window.update(|view, _, cx| {
            view.vitals.net_rx = 3.7e9;
            view.vitals.net_tx = 2.9e9;
            view.vitals.io_read = 1.2e9;
            view.vitals.io_write = 4096.;
            view.vitals.psi = [Some(99.9), Some(50.), None];
            view.vitals.cpu_temp = Some(99.9);
            view.mem_used = view.mem_total;
            view.swap_used = 8 * 1024 * 1024 * 1024;
            view.swap_total = view.swap_used;
            view.history = vec![87.; CPU_HIST];
            view.temp_hist = vec![95.; CARD_HIST];
            view.disk_hist = vec![1.2e9; CARD_HIST];
            view.net_hist = vec![6.6e9; CARD_HIST];
            view.procs_hist = vec![3.; CARD_HIST];
            view.mem_hist = vec![87.; CARD_HIST];
            cx.notify();
        });
        app.run_until_parked();
        let bounds = window.update(|_, window, _| {
            let probe = |sel: &str| {
                window
                    .debug_element_bounds(sel)
                    .unwrap_or_else(|| panic!("{sel} never painted"))
            };
            let rows: Vec<(String, _)> = ["vitals-strip", "vitals-strip-2", "vitals-strip-3"]
                .iter()
                .map(|sel| (sel.to_string(), probe(sel)))
                .collect();
            // (card selector, its row selector, bounds)
            let cards: Vec<(String, String, _)> = [
                ("card-cpu", "vitals-strip"),
                ("card-temp", "vitals-strip"),
                ("card-psi", "vitals-strip-2"),
                ("card-load", "vitals-strip-2"),
                ("card-procs", "vitals-strip-2"),
                ("card-mem", "vitals-strip-3"),
                ("card-disk", "vitals-strip-3"),
                ("card-net", "vitals-strip-3"),
            ]
            .iter()
            .map(|(card, row)| (card.to_string(), row.to_string(), probe(card)))
            .collect();
            let track = probe("mem-track");
            let fill = probe("mem-fill");
            (rows, cards, track, fill)
        });
        let (rows, cards, track, fill) = bounds;
        for (sel, row_sel, card) in &cards {
            let row = &rows
                .iter()
                .find(|(probe_sel, _)| probe_sel == row_sel)
                .unwrap()
                .1;
            assert!(
                card.origin.x >= row.origin.x - px(0.5)
                    && card.size.width + card.origin.x
                        <= row.size.width + row.origin.x + px(0.5)
                    && card.size.height <= row.size.height + px(0.5),
                "{sel} escaped {row_sel}: card {card:?} in row {row:?}"
            );
        }
        // full: the fill is the whole track
        assert!(
            (fill.size.width - track.size.width).abs() < px(1.),
            "fill at 100% is {:?} of track {:?}",
            fill.size,
            track.size
        );
        // the grid: every single-width card is exactly as wide as
        // its neighbors in the other rows, and the cpu hero is two
        // units plus one gap. Taffy rounds per cell, so the slack
        // forgives half a pixel per cell.
        let width_of = |sel: &str| {
            cards
                .iter()
                .find(|(probe_sel, _, _)| probe_sel == sel)
                .unwrap()
                .2
                .size
                .width
        };
        let unit = width_of("card-mem");
        for sel in ["card-temp", "card-psi", "card-load", "card-procs", "card-disk", "card-net"] {
            let w = width_of(sel);
            assert!(
                (w - unit).abs() < px(1.),
                "{sel} is {w:?} wide but the grid unit is {unit:?}"
            );
        }
        let cpu = width_of("card-cpu");
        assert!(
            (cpu - (unit * 2. + px(8.))).abs() < px(2.),
            "cpu hero is {cpu:?} wide but two units plus a gap is {:?}",
            unit * 2. + px(8.)
        );
        // a quarter: the fill is a quarter of the track
        app.run_until_parked();
        window.update(|view, _, cx| {
            view.mem_used = view.mem_total / 4;
            cx.notify();
        });
        app.run_until_parked();
        let quarter = window.update(|_, window, _| {
            let track = window.debug_element_bounds("mem-track").unwrap();
            let fill = window.debug_element_bounds("mem-fill").unwrap();
            (track, fill)
        });
        let (track, fill) = quarter;
        assert!(
            (fill.size.width - track.size.width / 4.).abs() < px(1.),
            "fill at 25% is {:?} of track {:?}",
            fill.size,
            track.size
        );
        assert!(fill.size.width + fill.origin.x <= track.size.width + track.origin.x + px(0.5));
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

    // ---------- the filter ----------

    fn prow(pid: u32, name: &str) -> ProcRow {
        ProcRow {
            pid,
            parent: None,
            name: name.to_string(),
            cpu: 1.,
            mem: 10,
            disk: 0,
            window: None,
        }
    }

    fn fgroup(name: &str, background: bool, members: Vec<ProcRow>) -> AppGroup {
        AppGroup {
            key: name.to_lowercase(),
            name: name.to_string(),
            cpu: members.iter().map(|m| m.cpu).sum(),
            mem: members.iter().map(|m| m.mem).sum(),
            disk: 0,
            background,
            has_focused: false,
            members,
        }
    }

    fn k(key: &str, ch: Option<&str>) -> Keystroke {
        Keystroke {
            key: key.to_string(),
            modifiers: gpui::Modifiers::default(),
            key_char: ch.map(str::to_string),
        }
    }

    #[test]
    fn the_filter_takes_plain_typing_only() {
        use std::sync::Arc;

        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(()),
        );
        let mut window = app.open_window(|window, cx| MonitorView::new(window, cx));
        app.run_until_parked();
        window.update(|view, _, _| {
            // the knobs must not leak into the owner's real state
            // file: a test run that typed here would persist "ku"
            view.state_path = None;
            assert!(view.filter_key(&k("k", Some("k"))));
            assert!(view.filter_key(&k("u", Some("u"))));
            assert_eq!(view.filter, "ku");
            // a plain key without key_char (enter, the arrows) is not typing
            assert!(!view.filter_key(&k("down", None)));
            // a modifier disqualifies even with a char
            let mut ctrl = k("k", Some("k"));
            ctrl.modifiers.control = true;
            assert!(!view.filter_key(&ctrl));
            // backspace pops, escape clears, escape on empty is a no-op
            assert!(view.filter_key(&k("backspace", None)));
            assert_eq!(view.filter, "k");
            assert!(view.filter_key(&k("escape", None)));
            assert_eq!(view.filter, "");
            assert!(!view.filter_key(&k("escape", None)));
        });
    }

    #[test]
    fn the_filter_hides_what_it_misses_and_surfaces_what_it_finds() {
        use std::sync::Arc;

        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(()),
        );
        let mut window = app.open_window(|window, cx| MonitorView::new(window, cx));
        app.run_until_parked();
        window.update(|view, _, _| {
            view.groups = vec![
                fgroup("Firefox", false, vec![prow(1, "firefox"), prow(2, "Web Content")]),
                fgroup("zen", false, vec![prow(3, "zen")]),
                fgroup("pipewire", true, vec![prow(4, "pipewire")]),
            ];
            // no filter: both app groups plus the background summary
            // (the background group itself sits behind the closed fold)
            assert_eq!(view.visible_items().len(), 3);

            // a member match pulls its header along, narrowed to the
            // matches; the summary hides, nothing in the background
            // matched "content"
            view.filter = "content".into();
            let items = view.visible_items();
            assert_eq!(items.len(), 2, "firefox header and the match");
            match &items[0] {
                Item::Group { index, open, .. } => {
                    assert_eq!(*index, 0);
                    assert!(*open, "a member-matched group shows its matches");
                }
                _ => panic!("expected a group row"),
            }
            match &items[1] {
                Item::Member { group, member } => {
                    assert_eq!(*group, 0);
                    assert_eq!(*member, 1);
                }
                _ => panic!("expected the matched member"),
            }

            // a background match surfaces past the fold: "pipe" hits
            // the group's name, so the expand state governs (closed
            // until clicked), the row itself dim
            view.filter = "pipe".into();
            let items = view.visible_items();
            assert_eq!(items.len(), 2);
            match &items[1] {
                Item::Group { index, dim, open, .. } => {
                    assert_eq!(*index, 2);
                    assert!(*dim);
                    assert!(!*open);
                }
                _ => panic!("expected the surfaced background group"),
            }

            // a miss hides everything, summary included, so the
            // table goes quiet and the miss line speaks
            view.filter = "qqq".into();
            assert!(view.visible_items().is_empty());
        });
    }

    /// The CSD bar renders above the vitals: the drag zone starts at
    /// the window's top, the button cluster stays inside the bar's
    /// height, and the glyphs resolve from the app's asset source.
    #[test]
    fn the_titlebar_renders_over_the_vitals() {
        use std::sync::Arc;

        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(crate::icons::Assets),
        );
        let mut window = app.open_window(|window, cx| MonitorView::new(window, cx));
        app.run_until_parked();
        // one fresh frame so the bar's paint records its bounds
        window.update(|_, _, cx| cx.notify());
        app.run_until_parked();
        let (drag, close) = window.update(|_, window, _| {
            (
                window.debug_element_bounds("kuma-titlebar-drag").unwrap(),
                window.debug_element_bounds("kuma-titlebar-close").unwrap(),
            )
        });
        assert_eq!(drag.origin.y, px(0.), "the bar is the window's first child");
        // the bar's hairline is inset of its 34px, so the children
        // stop one pixel short
        assert_eq!(close.bottom(), px(kuma_platform::titlebar::BAR_HEIGHT - 1.));
        assert!(drag.right() < close.origin.x, "drag zone left of the buttons");
    }

    /// The titlebar's glyphs must rasterize to actual ink: a path the
    /// SVG parser drops paints nothing and the button goes blind (the
    /// missing minus, caught here).
    #[test]
    fn the_titlebar_glyphs_rasterize() {
        use gpui::AssetSource;
        use std::sync::Arc;

        let assets = Arc::new(crate::icons::Assets);
        let renderer = gpui::SvgRenderer::new(assets.clone());
        for (name, path) in [
            ("minimize", "icons/titlebar_minimize.svg"),
            ("maximize", "icons/titlebar_maximize.svg"),
            ("close", "icons/titlebar_close.svg"),
        ] {
            let bytes = assets
                .load(path)
                .unwrap()
                .unwrap_or_else(|| panic!("{name}: nothing at {path}"));
            let image = renderer
                .render_single_frame(bytes.as_ref(), 1.0)
                .unwrap_or_else(|err| panic!("{name}: did not parse: {err}"));
            let ink = image
                .as_bytes(0)
                .unwrap()
                .iter()
                .skip(3)
                .step_by(4)
                .filter(|&alpha| *alpha > 0)
                .count();
            assert!(ink > 20, "{name}: only {ink} inked pixels rasterized");
        }
    }

    #[test]
    fn the_state_parse_round_trips_the_knobs_and_forgives_the_rest() {
        let text = "sort=mem\nfilter=zen\n";
        assert_eq!(
            parse_state(text),
            (Some(SortKey::Mem), Some("zen".to_string()))
        );
        // a filter can carry an equals sign: split_once keeps the tail
        let text = "filter=a=b\nsort=name\n";
        assert_eq!(
            parse_state(text),
            (Some(SortKey::Name), Some("a=b".to_string()))
        );
        // an empty filter value is a cleared buffer, not a missing line
        assert_eq!(parse_state("filter=\n"), (None, Some(String::new())));
        // unknown keys, malformed lines, a bad sort word: all die
        // quietly, a future key or a hand edit must not break a read
        assert_eq!(parse_state("future=1\nnope\nsort=zzz\n"), (None, None));
        // every key speaks its own name and only its own
        for key in [SortKey::Cpu, SortKey::Mem, SortKey::Name, SortKey::Pid] {
            assert_eq!(SortKey::from_name(key.name()), Some(key));
        }
    }

    #[test]
    fn the_knobs_land_in_the_state_file_and_read_back() {
        use std::sync::Arc;

        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(()),
        );
        let mut window = app.open_window(|window, cx| MonitorView::new(window, cx));
        app.run_until_parked();
        let state = std::env::temp_dir().join(format!("kuma-monitor-state-{}", std::process::id()));
        let _ = std::fs::remove_file(&state);
        window.update(|view, _, cx| {
            view.state_path = Some(state.clone());
            view.filter.push_str("zen");
            view.save_state();
            view.set_sort(SortKey::Mem, cx);
        });
        let text = std::fs::read_to_string(&state).unwrap();
        assert_eq!(
            parse_state(&text),
            (Some(SortKey::Mem), Some("zen".to_string()))
        );
        std::fs::remove_file(&state).ok();
    }

    #[test]
    fn a_restored_sort_applies_to_the_first_table_then_never_again() {
        use std::sync::Arc;

        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(()),
        );
        let mut window = app.open_window(|window, cx| MonitorView::new(window, cx));
        app.run_until_parked();
        let snap_row = |pid: u32, name: &str, cpu: f32| ProcRow {
            pid,
            parent: None,
            name: name.into(),
            cpu,
            mem: 10,
            disk: 0,
            window: None,
        };
        let snap = |rows: Vec<ProcRow>| Snapshot {
            cpu_total: 0.,
            mem_used: 0,
            mem_total: 0,
            swap_used: 0,
            swap_total: 0,
            rows,
        };
        window.update(|view, _, _| {
            view.pending_sort = true;
            view.sort = SortKey::Name;
            view.apply_snapshot(
                snap(vec![snap_row(2, "zeta", 90.), snap_row(1, "alpha", 1.)]),
                Vitals::default(),
                Vec::new(),
            );
            let names: Vec<String> = view.groups.iter().map(|g| g.name.clone()).collect();
            assert_eq!(names, vec!["alpha", "zeta"], "the restored sort applied");
            // a later beat must not re-sort: from here the order only
            // moves on the user's say-so, new groups at the tail
            view.apply_snapshot(
                snap(vec![
                    snap_row(3, "mid", 50.),
                    snap_row(1, "alpha", 1.),
                    snap_row(2, "zeta", 90.),
                ]),
                Vitals::default(),
                Vec::new(),
            );
            let names: Vec<String> = view.groups.iter().map(|g| g.name.clone()).collect();
            assert_eq!(names, vec!["alpha", "zeta", "mid"], "no re-sort, tail join");
        });
    }

    #[test]
    fn the_cursor_walks_the_rows_and_wraps() {
        use std::sync::Arc;

        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(()),
        );
        let mut window = app.open_window(|window, cx| MonitorView::new(window, cx));
        app.run_until_parked();
        window.update(|view, _, cx| {
            view.groups = vec![
                fgroup("a", false, vec![prow(1, "a")]),
                fgroup("b", false, vec![prow(2, "b")]),
            ];
            // closed folds: two group rows, the cursor wraps both ways
            view.move_cursor(true, cx);
            assert_eq!(view.cursor, Some(0));
            view.move_cursor(true, cx);
            assert_eq!(view.cursor, Some(1));
            view.move_cursor(true, cx);
            assert_eq!(view.cursor, Some(0), "down wraps at the tail");
            view.move_cursor(false, cx);
            assert_eq!(view.cursor, Some(1), "up wraps at the head");
            // the kill key on a fold row is a no-op: a fold is a sum,
            // and the kill keys read the pid a member row carries
            view.kill_key(cx);
            assert!(view.kill_confirm.is_none());
            assert!(view.sigterm_pid.is_none());
        });
    }

    #[test]
    fn the_kill_key_asks_with_sigterm_then_arms_the_sigkill_card() {
        use std::sync::Arc;

        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(()),
        );
        let mut window = app.open_window(|window, cx| MonitorView::new(window, cx));
        app.run_until_parked();
        // a real child to aim at: sleep answers SIGTERM by exiting,
        // which is the whole point of the polite ask
        let mut sleeper = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleeper child");
        let pid = sleeper.id();
        window.update(|view, _, cx| {
            view.groups = vec![fgroup("sleeper", false, vec![prow(pid, "sleeper")])];
            view.expanded_apps.insert("sleeper".into());
            // down twice: the header first, the member row second
            view.move_cursor(true, cx);
            view.move_cursor(true, cx);
            assert!(matches!(
                view.visible_items()[view.cursor.unwrap()],
                Item::Member { .. }
            ));
            // first press: one SIGTERM, no card
            view.kill_key(cx);
            assert!(view.kill_confirm.is_none());
            assert_eq!(view.sigterm_pid, Some(pid));
        });
        let mut exited = false;
        for _ in 0..100 {
            if sleeper.try_wait().unwrap().is_some() {
                exited = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(exited, "the sleeper never took the SIGTERM");
        window.update(|view, _, cx| {
            // second press on the same row: the ask did not land, so
            // the card arms instead of signaling again
            view.kill_key(cx);
            let (name, armed) = view.kill_confirm.clone().expect("card armed");
            assert_eq!(armed, pid);
            assert_eq!(name, "sleeper");
        });
        window.update(|view, _, cx| {
            // the yes: SIGKILL, whose footer line reads "already
            // gone" here because the SIGTERM above already ended it
            view.confirm_sigkill(cx);
            assert!(view.kill_confirm.is_none());
            let (line, _) = view.signal_flash.clone().unwrap();
            assert!(
                line.contains(&format!("(pid {pid})")) && line.contains("already gone"),
                "flash: {line}"
            );
        });
        let _ = sleeper.wait();
    }
}
