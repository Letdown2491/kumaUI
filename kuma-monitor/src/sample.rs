//! The sampler: one beat of process rows joined with niri's windows,
//! and the sort that orders the table. The join is pure so the tests
//! can pin it without a live system.

use std::collections::HashMap;

use sysinfo::{ProcessesToUpdate, System};

use crate::niri::NiriWindow;

/// A process row's sysinfo half, before the join.
pub(crate) struct RawRow {
    pub pid: u32,
    pub name: String,
    pub cpu: f32,
    pub mem: u64,
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
    pub name: String,
    pub cpu: f32,
    pub mem: u64,
    pub window: Option<WindowRef>,
}

/// One beat of the whole table: totals for the graph and the rows
/// for the list.
pub(crate) struct Snapshot {
    pub cpu_total: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    pub rows: Vec<ProcRow>,
}

/// The keeper between beats: sysinfo computes cpu% as a delta against
/// the previous refresh, so the Sampler must live across the tick.
pub(crate) struct Sampler {
    system: System,
}

impl Sampler {
    pub(crate) fn new() -> Self {
        // new_all, not new: the first beat lands on a filled system
        // so the window opens with numbers instead of a zero frame
        let mut system = System::new_all();
        system.refresh_cpu_usage();
        Self { system }
    }

    pub(crate) fn beat(&mut self, windows: &[NiriWindow]) -> Snapshot {
        self.system.refresh_processes(ProcessesToUpdate::All, true);
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();

        let raw = self
            .system
            .processes()
            .values()
            .map(|p| RawRow {
                pid: p.pid().as_u32(),
                name: p.name().to_string_lossy().into_owned(),
                cpu: p.cpu_usage(),
                mem: p.memory(),
            })
            .collect();

        Snapshot {
            cpu_total: self.system.global_cpu_usage(),
            mem_used: self.system.used_memory(),
            mem_total: self.system.total_memory(),
            rows: join(raw, windows),
        }
    }
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
                name: row.name,
                cpu: row.cpu,
                mem: row.mem,
                window,
            }
        })
        .collect()
}

/// The row's lead identity: the window's app when joined, the
/// process name otherwise. The name sort runs on this, so a windowed
/// process files under its app, not its binary.
pub(crate) fn lead(row: &ProcRow) -> &str {
    match &row.window {
        Some(window) => window
            .app_id
            .as_deref()
            .or_else(|| {
                if window.title.is_empty() {
                    None
                } else {
                    Some(window.title.as_str())
                }
            })
            .unwrap_or(&row.name),
        None => &row.name,
    }
}

/// The table's order. Cpu and Mem sort down (the hog first), Name
/// sorts up; a spike needs no direction toggle.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum SortKey {
    Cpu,
    Mem,
    Name,
}

pub(crate) fn sort_rows(rows: &mut [ProcRow], key: SortKey) {
    match key {
        SortKey::Cpu => rows.sort_by(|a, b| b.cpu.partial_cmp(&a.cpu).unwrap_or(std::cmp::Ordering::Equal)),
        SortKey::Mem => rows.sort_by(|a, b| b.mem.cmp(&a.mem)),
        SortKey::Name => rows.sort_by(|a, b| lead(a).to_lowercase().cmp(&lead(b).to_lowercase())),
    }
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
            name: name.to_string(),
            cpu: 0.,
            mem: 0,
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
    fn sorts_cpu_down_and_name_up() {
        let mut rows = vec![
            ProcRow { pid: 1, name: "b".into(), cpu: 1., mem: 10, window: None },
            ProcRow { pid: 2, name: "A".into(), cpu: 9., mem: 50, window: None },
            ProcRow { pid: 3, name: "c".into(), cpu: 4., mem: 30, window: None },
        ];
        sort_rows(&mut rows, SortKey::Cpu);
        assert_eq!(rows.iter().map(|r| r.pid).collect::<Vec<_>>(), vec![2, 3, 1]);
        sort_rows(&mut rows, SortKey::Name);
        assert_eq!(rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), vec!["A", "b", "c"]);
        sort_rows(&mut rows, SortKey::Mem);
        assert_eq!(rows.iter().map(|r| r.pid).collect::<Vec<_>>(), vec![2, 3, 1]);
    }

    #[test]
    fn lead_prefers_the_app_id_then_the_title() {
        let mut row = ProcRow {
            pid: 1,
            name: "electron".into(),
            cpu: 0.,
            mem: 0,
            window: Some(WindowRef {
                app_id: Some("kuma-term".into()),
                title: "kuma-term".into(),
                focused: false,
            }),
        };
        assert_eq!(lead(&row), "kuma-term");
        row.window = Some(WindowRef {
            app_id: None,
            title: "Some Window".into(),
            focused: false,
        });
        assert_eq!(lead(&row), "Some Window");
        row.window = None;
        assert_eq!(lead(&row), "electron");
    }

    /// A live beat in the build container: the sampler's own half of
    /// the table yields rows (the container's /proc holds cargo and
    /// friends during the run), cpu and memory read as finite totals.
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
