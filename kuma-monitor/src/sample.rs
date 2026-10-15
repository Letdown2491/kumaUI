//! The sampler: one beat of process rows joined with niri's windows,
//! the app grouping that folds a windowed process and its helpers
//! into one row, and the sorts. The join and the grouping are pure so
//! the tests can pin them without a live system.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;

use sysinfo::{Pid, ProcessesToUpdate, System};

use crate::niri::NiriWindow;

/// A process row's sysinfo half, before the join.
pub(crate) struct RawRow {
    pub pid: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub cpu: f32,
    pub mem: u64,
    /// Read+write bytes per second (sysinfo's disk_usage delta).
    pub disk: u64,
}

/// The window half the join attaches: the app_id the dock matches
/// against, the window title, and whether it holds focus now.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct WindowRef {
    pub app_id: Option<String>,
    pub title: String,
    pub focused: bool,
}

/// One process row: sysinfo's numbers plus whatever window niri
/// attributes to the pid.
#[derive(Clone, Debug)]
pub(crate) struct ProcRow {
    pub pid: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub cpu: f32,
    pub mem: u64,
    /// Read+write bytes per second.
    pub disk: u64,
    pub window: Option<WindowRef>,
}

/// One app, however many processes carry it: the windowed process and
/// its helpers under one row, with the sums the user actually reasons
/// about (what does this app cost, what does closing it return).
#[derive(Clone, Debug)]
pub(crate) struct AppGroup {
    /// The stable identity beats merge on: the app_id lowercased,
    /// or the binary's name lowercased for background groups.
    pub key: String,
    /// What the row leads with: the app_id's own case when windowed,
    /// else the binary's name.
    pub name: String,
    pub cpu: f32,
    pub mem: u64,
    /// Read+write bytes per second across the members.
    pub disk: u64,
    /// No member carries a window: session plumbing, summarized
    /// behind one line instead of a wall of daemon names.
    pub background: bool,
    /// True while any member's window holds focus: the row highlight.
    pub has_focused: bool,
    /// Members by pid: a stable roster, so the expand state reads the
    /// same list in the same order every beat.
    pub members: Vec<ProcRow>,
}

/// One beat of the whole table: totals for the graph and the rows
/// for the list.
pub(crate) struct Snapshot {
    pub cpu_total: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    pub rows: Vec<ProcRow>,
}

/// The keeper between beats: sysinfo computes cpu% as a delta against
/// the previous refresh, so the Sampler must live across the tick.
pub(crate) struct Sampler {
    system: System,
    /// The uid the walk scopes to: the monitor's question is which of
    /// the user's apps eat the machine, and the root half of /proc
    /// (systemd, the kernel's threads) is noise the walk then never
    /// pays for either.
    uid: u32,
}

impl Sampler {
    pub(crate) fn new() -> Self {
        // an EMPTY cache, seeded by the first beat's uid-scoped walk:
        // new_all would load every root process into the table
        let mut system = System::new();
        system.refresh_cpu_usage();
        let uid = std::fs::metadata("/proc/self")
            .map(|meta| meta.uid())
            .unwrap_or(u32::MAX);
        Self { system, uid }
    }

    pub(crate) fn beat(&mut self, windows: &[NiriWindow]) -> Snapshot {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();

        // the user's own pids: one readdir, one stat per /proc dir,
        // no file reads. Refreshing cached pids too (dead ones are
        // then refreshed-and-dead, which remove_dead_processes
        // prunes; unrefreshed cache entries would linger forever).
        let live = live_user_pids(self.uid);
        let mut update: Vec<Pid> = self.system.processes().keys().copied().collect();
        let seen: std::collections::HashSet<u32> =
            update.iter().map(|pid| pid.as_u32()).collect();
        update.extend(
            live.into_iter()
                .filter(|pid| !seen.contains(&pid.as_u32())),
        );
        self.system
            .refresh_processes(ProcessesToUpdate::Some(&update), true);

        let raw = self
            .system
            .processes()
            .values()
            .map(|p| RawRow {
                pid: p.pid().as_u32(),
                parent: p.parent().map(|p| p.as_u32()),
                name: p.name().to_string_lossy().into_owned(),
                cpu: p.cpu_usage(),
                // what a death returns, not what the process
                // displays: RSS minus the pages other processes
                // also hold. sysinfo only exposes the RSS half,
                // which bills zen's shared pages to every one of
                // its forked content processes.
                mem: private_mem(p.pid().as_u32(), p.memory()),
                // read+write deltas, already computed by the refresh
                // we were doing anyway
                disk: {
                    let usage = p.disk_usage();
                    usage.read_bytes.saturating_add(usage.written_bytes)
                },
            })
            .collect();

        Snapshot {
            cpu_total: self.system.global_cpu_usage(),
            mem_used: self.system.used_memory(),
            mem_total: self.system.total_memory(),
            swap_used: self.system.used_swap(),
            swap_total: self.system.total_swap(),
            rows: join(raw, windows),
        }
    }
}

/// The user's own pids out of /proc: a numeric entry, one stat, a uid
/// compare. Cheap enough to run every beat.
fn live_user_pids(uid: u32) -> Vec<Pid> {
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(entry.path()) else {
            continue;
        };
        if meta.uid() == uid {
            pids.push(Pid::from_u32(pid));
        }
    }
    pids
}

/// The private half of memory from statm (resident minus shared, in
/// pages). One cheap read per process, unlike smaps_rollup whose
/// page-table walk measured ~30ms per big process on this machine:
/// proportional set size would cost more than the whole sysinfo walk.
/// Falls back to RSS when the read fails.
fn private_mem(pid: u32, rss_fallback: u64) -> u64 {
    let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/statm")) else {
        return rss_fallback;
    };
    parse_statm(&text)
        .map(|(resident, shared)| (resident.saturating_sub(shared)) * PAGE_SIZE)
        .unwrap_or(rss_fallback)
}

const PAGE_SIZE: u64 = 4096;

/// statm: "size resident shared text lib data dt", all in pages.
fn parse_statm(text: &str) -> Option<(u64, u64)> {
    let mut fields = text.split_whitespace();
    let _size = fields.next()?.parse::<u64>().ok()?;
    let resident = fields.next()?.parse::<u64>().ok()?;
    let shared = fields.next()?.parse::<u64>().ok()?;
    Some((resident, shared))
}

/// The join: attach each pid's window, the focused one winning when
/// a process owns several (electron's renderer split). Pids niri
/// does not know keep None; windows whose pid died between beats
/// drop out until niri catches up.
pub(crate) fn join(raw: Vec<RawRow>, windows: &[NiriWindow]) -> Vec<ProcRow> {
    let mut by_pid: HashMap<u32, &NiriWindow> = HashMap::new();
    for window in windows {
        let Some(pid) = window.pid else { continue };
        let better = match by_pid.get(&pid) {
            // the focused window wins the pid, the first seen breaks
            // the tie when neither holds focus
            Some(existing) => window.is_focused && !existing.is_focused,
            None => true,
        };
        if better {
            by_pid.insert(pid, window);
        }
    }
    raw.into_iter()
        .map(|row| {
            let window = by_pid.get(&row.pid).map(|window| WindowRef {
                app_id: window.app_id.clone(),
                title: window.title.clone().unwrap_or_default(),
                focused: window.is_focused,
            });
            ProcRow {
                pid: row.pid,
                parent: row.parent,
                name: row.name,
                cpu: row.cpu,
                mem: row.mem,
                disk: row.disk,
                window,
            }
        })
        .collect()
}

/// Fold a walk's rows into app groups. A row files under the nearest
/// WINDOWED ANCESTOR: zen's forked content processes under zen,
/// WebKit's helpers under epiphany, whatever each binary is named.
/// A row with no windowed ancestor is background plumbing and groups
/// under its own name. Existing groups keep their position (the
/// order only moves on a header click), dead groups drop out,
/// first-seen groups join at the tail.
pub(crate) fn merge_groups(groups: &mut Vec<AppGroup>, rows: Vec<ProcRow>) {
    // key first (the ancestor walk reads the whole table), consume after
    let keys: Vec<String> = rows.iter().map(|row| group_key(&rows, row)).collect();
    let mut by_key: HashMap<String, Vec<ProcRow>> = HashMap::new();
    for (key, row) in keys.into_iter().zip(rows.into_iter()) {
        by_key.entry(key).or_default().push(row);
    }
    groups.retain_mut(|group| match by_key.remove(&group.key) {
        Some(members) => {
            *group = AppGroup::build(group.key.clone(), members);
            true
        }
        // the whole app died between walks
        None => false,
    });
    let mut new_keys: Vec<String> = by_key.keys().cloned().collect();
    new_keys.sort();
    for key in new_keys {
        let members = by_key.remove(&key).unwrap();
        groups.push(AppGroup::build(key, members));
    }
}

/// A row's grouping key: its own app_id when windowed, else the
/// app_id of the nearest windowed ancestor (walking ppid up to a
/// bounded depth), else the process's own name.
fn group_key(all: &[ProcRow], row: &ProcRow) -> String {
    if let Some(app) = row
        .window
        .as_ref()
        .and_then(|window| window.app_id.as_ref())
    {
        return app.to_lowercase();
    }
    let mut pid = row.parent;
    let mut hops = 0;
    while let Some(ancestor_pid) = pid {
        if hops >= 32 {
            break;
        }
        hops += 1;
        let Some(ancestor) = all.iter().find(|row| row.pid == ancestor_pid) else {
            break;
        };
        if let Some(app) = ancestor
            .window
            .as_ref()
            .and_then(|window| window.app_id.as_ref())
        {
            return app.to_lowercase();
        }
        pid = ancestor.parent;
    }
    row.name.to_lowercase()
}

impl AppGroup {
    fn build(key: String, mut members: Vec<ProcRow>) -> Self {
        members.sort_by_key(|row| row.pid);
        let name = members
            .iter()
            .find_map(|row| row.window.as_ref().and_then(|w| w.app_id.clone()))
            .unwrap_or_else(|| members[0].name.clone());
        AppGroup {
            cpu: members.iter().map(|row| row.cpu).sum(),
            mem: members.iter().map(|row| row.mem).sum(),
            disk: members.iter().map(|row| row.disk).sum(),
            background: !members.iter().any(|row| row.window.is_some()),
            has_focused: members
                .iter()
                .any(|row| row.window.as_ref().is_some_and(|w| w.focused)),
            key,
            name,
            members,
        }
    }
}

/// The table's order over app groups. Cpu and Mem sort down (the hog
/// first), Name and Pid sort up; a spike needs no direction toggle.
/// Pid compares a group's first member (its lowest pid).
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum SortKey {
    Cpu,
    Mem,
    Name,
    Pid,
}

impl SortKey {
    /// The direction the key sorts in: the arrow in the active
    /// header announces it, since the click that changes columns
    /// never flips direction.
    pub(crate) fn sorts_down(self) -> bool {
        matches!(self, SortKey::Cpu | SortKey::Mem)
    }
}

pub(crate) fn sort_groups(groups: &mut [AppGroup], key: SortKey) {
    match key {
        SortKey::Cpu => groups
            .sort_by(|a, b| b.cpu.partial_cmp(&a.cpu).unwrap_or(std::cmp::Ordering::Equal)),
        SortKey::Mem => groups.sort_by(|a, b| b.mem.cmp(&a.mem)),
        SortKey::Name => groups.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
        SortKey::Pid => groups.sort_by(|a, b| a.members[0].pid.cmp(&b.members[0].pid)),
    }
}

/// What a filter query finds in one group: the whole group (the name
/// matched) or a subset of members (a member name or pid matched).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GroupHit {
    All,
    Members(Vec<usize>),
}

/// The filter's one predicate: a case-insensitive substring on the
/// group's name, else on member names, else a pid prefix when the
/// query is all digits. None hides the group outright.
pub(crate) fn match_group(group: &AppGroup, q: &str) -> Option<GroupHit> {
    if q.is_empty() {
        return Some(GroupHit::All);
    }
    if group.name.to_lowercase().contains(q) {
        return Some(GroupHit::All);
    }
    let digits = q.bytes().all(|b| b.is_ascii_digit());
    let hits: Vec<usize> = group
        .members
        .iter()
        .enumerate()
        .filter(|(_, row)| {
            row.name.to_lowercase().contains(q) || (digits && row.pid.to_string().starts_with(q))
        })
        .map(|(ix, _)| ix)
        .collect();
    (!hits.is_empty()).then_some(GroupHit::Members(hits))
}

/// Bytes at the table's width: one decimal, the unit carrying the
/// magnitude (KiB, MiB, GiB), TiB the honest cap.
pub(crate) fn human_mem(bytes: u64) -> String {
    const KIB: f64 = 1024.;
    let mut value = bytes as f64 / KIB;
    for unit in ["KiB", "MiB", "GiB"] {
        if value < KIB {
            return format!("{value:.1} {unit}");
        }
        value /= KIB;
    }
    format!("{value:.1} TiB")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(pid: u32, app_id: &str, focused: bool) -> NiriWindow {
        NiriWindow {
            pid: Some(pid),
            app_id: Some(app_id.to_string()),
            title: Some(format!("{app_id} title")),
            is_focused: focused,
        }
    }

    fn raw(pid: u32, name: &str) -> RawRow {
        RawRow {
            pid,
            parent: None,
            name: name.to_string(),
            cpu: 0.,
            mem: 0,
            disk: 0,
        }
    }

    fn row(pid: u32, name: &str, cpu: f32, mem: u64) -> ProcRow {
        ProcRow {
            pid,
            parent: None,
            name: name.to_string(),
            cpu,
            mem,
            disk: 0,
            window: None,
        }
    }

    fn windowed(pid: u32, name: &str, app_id: &str, focused: bool) -> ProcRow {
        ProcRow {
            pid,
            parent: None,
            name: name.to_string(),
            cpu: 0.,
            mem: 0,
            disk: 0,
            window: Some(WindowRef {
                app_id: Some(app_id.to_string()),
                title: format!("{app_id} title"),
                focused,
            }),
        }
    }

    #[test]
    fn join_attaches_the_window_to_its_pid() {
        let windows = vec![window(2298, "kuma-term", true)];
        let rows = join(vec![raw(2298, "kuma-term"), raw(999, "bgd")], &windows);
        assert_eq!(rows.len(), 2);
        let joined = rows.iter().find(|r| r.pid == 2298).unwrap();
        assert_eq!(joined.window.as_ref().unwrap().app_id.as_deref(), Some("kuma-term"));
        assert!(joined.window.as_ref().unwrap().focused);
        assert!(rows.iter().find(|r| r.pid == 999).unwrap().window.is_none());
    }

    #[test]
    fn join_prefers_the_focused_window_of_a_shared_pid() {
        // both windows share the pid: one app, several windows, the
        // focused one speaks for the row
        let windows = vec![window(7, "the-idle", false), window(7, "the-focus", true)];
        let rows = join(vec![raw(7, "electron")], &windows);
        assert_eq!(
            rows[0].window.as_ref().unwrap().app_id.as_deref(),
            Some("the-focus")
        );
    }

    #[test]
    fn join_drops_windows_without_a_pid() {
        let windows = vec![NiriWindow {
            pid: None,
            app_id: Some("mystery".into()),
            title: None,
            is_focused: false,
        }];
        let rows = join(vec![raw(1, "init")], &windows);
        assert!(rows[0].window.is_none());
    }

    #[test]
    fn groups_merge_app_with_helpers_and_sum() {
        // zen's window plus two forked content processes: one row,
        // the sums, the members rostered by pid. The helpers file
        // under the windowed ancestor whatever they are named.
        let mut zen = windowed(100, "zen", "zen", true);
        zen.cpu = 2.;
        zen.mem = 100;
        zen.disk = 50;
        let mut helper = row(102, "zen-worker", 4., 300);
        helper.parent = Some(100);
        helper.disk = 150;
        let mut groups = Vec::new();
        merge_groups(
            &mut groups,
            vec![zen.clone(), helper, row(101, "zen", 3., 200)],
        );
        assert_eq!(groups.len(), 1, "the app and its helpers must share a group");
        let group = &groups[0];
        assert_eq!(group.name, "zen");
        assert_eq!(group.members.iter().map(|r| r.pid).collect::<Vec<_>>(), vec![100, 101, 102]);
        assert_eq!(group.cpu, 9.);
        assert_eq!(group.mem, 600);
        assert_eq!(group.disk, 200);
        assert!(group.has_focused);
        assert!(!group.background);
    }

    #[test]
    fn helpers_file_under_the_windowed_ancestors_app_id() {
        // a helper whose binary name shares nothing with the app:
        // WebKit's processes under epiphany
        let browser = windowed(10, "epiphany", "org.gnome.Epiphany", false);
        let mut helper = row(11, "WebKitWebProces", 1., 90);
        helper.parent = Some(10);
        let mut groups = Vec::new();
        merge_groups(&mut groups, vec![browser, helper]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "org.gnome.Epiphany");
    }

    #[test]
    fn orphan_processes_are_background() {
        // no window anywhere in the ancestor chain: session plumbing,
        // grouped by name and marked background
        let mut daemon = row(20, "pipewire", 0.5, 30);
        daemon.parent = Some(1);
        let mut groups = Vec::new();
        merge_groups(&mut groups, vec![daemon, row(21, "pipewire", 0.2, 10)]);
        assert_eq!(groups.len(), 1);
        assert!(groups[0].background);
        assert!(!groups[0].has_focused);
    }

    #[test]
    fn groups_keep_their_order_and_drop_the_dead() {
        let mut groups = Vec::new();
        merge_groups(&mut groups, vec![row(1, "a", 0., 0), row(2, "b", 0., 0)]);
        assert_eq!(groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
        merge_groups(
            &mut groups,
            vec![row(1, "a", 0., 0), row(2, "b", 0., 0), row(3, "c", 0., 0)],
        );
        assert_eq!(
            groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"],
            "existing groups keep their position, new ones join at the tail"
        );
        merge_groups(&mut groups, vec![row(2, "b", 0., 0), row(3, "c", 0., 0)]);
        assert_eq!(
            groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(),
            vec!["b", "c"],
            "a group whose last member died drops out"
        );
    }

    #[test]
    fn sort_groups_by_each_key() {
        let mut groups = Vec::new();
        merge_groups(
            &mut groups,
            vec![
                row(1, "b", 1., 10),
                row(2, "A", 9., 50),
                row(3, "c", 4., 30),
            ],
        );
        sort_groups(&mut groups, SortKey::Cpu);
        assert_eq!(groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), vec!["A", "c", "b"]);
        sort_groups(&mut groups, SortKey::Name);
        assert_eq!(groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), vec!["A", "b", "c"]);
        sort_groups(&mut groups, SortKey::Mem);
        assert_eq!(groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), vec!["A", "c", "b"]);
        sort_groups(&mut groups, SortKey::Pid);
        assert_eq!(groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), vec!["b", "A", "c"]);
        // the arrow in the header announces each key's fixed direction
        assert!(SortKey::Cpu.sorts_down());
        assert!(SortKey::Mem.sorts_down());
        assert!(!SortKey::Name.sorts_down());
        assert!(!SortKey::Pid.sorts_down());
    }

    #[test]
    fn the_filter_matches_by_name_member_and_pid() {
        // zen's window plus two helpers: the group name answers the
        // query whole, a member name or pid narrows to the matches,
        // and a miss hides the group outright
        let mut zen = windowed(100, "zen", "zen", true);
        zen.cpu = 2.;
        let mut helper = row(102, "zen-worker", 4., 300);
        helper.parent = Some(100);
        let mut webkit = row(101, "WebKitWebProces", 1., 90);
        webkit.parent = Some(100);
        let mut groups = Vec::new();
        merge_groups(&mut groups, vec![zen, helper, webkit]);
        let group = &groups[0];

        // the group's own name, case-insensitive (the query arrives
        // lowercased; the view does it once per beat)
        assert_eq!(match_group(group, "ze"), Some(GroupHit::All));
        // a member's name
        assert_eq!(match_group(group, "webkit"), Some(GroupHit::Members(vec![1])));
        // a pid prefix, digits only
        assert_eq!(match_group(group, "10"), Some(GroupHit::Members(vec![0, 1, 2])));
        // nothing matches: hide
        assert_eq!(match_group(group, "firefox"), None);
        // an empty query is no filter at all
        assert_eq!(match_group(group, ""), Some(GroupHit::All));
    }

    #[test]
    fn parse_statm_reads_pages() {
        assert_eq!(parse_statm("123 100 60 10 0 40 0"), Some((100, 60)));
        assert_eq!(parse_statm("junk"), None);
        assert_eq!(parse_statm("1 2"), None);
    }

    /// A live beat in the build container: the uid-scoped walk yields
    /// rows (the container's /proc holds cargo and friends during the
    /// run), cpu and memory read as finite totals.
    #[test]
    fn a_live_beat_yields_rows() {
        let mut sampler = Sampler::new();
        let snap = sampler.beat(&[]);
        assert!(snap.rows.len() >= 1, "no rows from a live beat");
        assert!(snap.cpu_total >= 0.);
        assert!(snap.mem_total > 0);
    }

    #[test]
    fn human_mem_names_the_unit() {
        assert_eq!(human_mem(0), "0.0 KiB");
        assert_eq!(human_mem(1024 * 1024 * 5), "5.0 MiB");
        assert_eq!(human_mem(1024 * 1024 * 1024 * 31), "31.0 GiB");
    }
}
