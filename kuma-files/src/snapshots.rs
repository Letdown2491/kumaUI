//! The snapshot store's face for the file manager: which snapshots a
//! folder has, and where one snapshot's copy of that folder lives on
//! disk. kumaOS's snapshot engine does the taking and the pruning; the
//! store it keeps (`<target>/.snapshots/<id>`, ids shaped
//! `2026-08-08T134347`, newest first) is browsable on purpose, and
//! this module is the file manager's way of browsing it without
//! guessing at the shapes.
//!
//! Read-only by nature: a snapshot is a `btrfs subvolume snapshot -r`,
//! which a plain directory listing reads happily and nothing should
//! ever write to. Restoring is a copy back to the live tree, and it
//! rides the browser's ordinary op queue, where the replace-copy
//! steps the old live file aside and Ctrl+Z can bring it back.
//!
//! No snapshots (no store, the folder outside the target) means no
//! timeline: every query returns empty and the browser hides the
//! surface. Defaults, not dependencies.

use std::path::{Path, PathBuf};

/// One snapshot of the current folder: the store id and the human
/// form of it. The id is the road (it names the store directory); the
/// label is what the picker and the banner show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub id: String,
    pub when: String,
}

/// The snapshot store of one declared target. kumaOS composes
/// `/var/home` as the snapshot subvolume and its declaration's default
/// target, so that constant is the whole discovery; the struct exists
/// so a test can point the store somewhere else.
#[derive(Clone, Debug)]
pub struct KumaSnapshots {
    pub target: PathBuf,
}

impl KumaSnapshots {
    /// The store as the machine declares it. `/var/home` is kumaOS's
    /// home subvolume; on a machine without the layer the store
    /// simply will not be there, and every query degrades to empty.
    pub fn discover() -> Self {
        Self {
            target: PathBuf::from("/var/home"),
        }
    }

    fn store(&self) -> PathBuf {
        self.target.join(".snapshots")
    }

    /// The snapshots that carry a copy of `dir`, newest first. Empty
    /// unless `dir` lives under the target and outside the store
    /// itself: the store is a nested subvolume inside the target, so
    /// snapshots never contain each other, and offering "snapshots of
    /// a snapshot" would be offering the same store to itself.
    pub fn snapshots_for(&self, dir: &Path) -> Vec<Snapshot> {
        if self.relative(dir).is_none() {
            return Vec::new();
        }
        let store = self.store();
        let Ok(read) = std::fs::read_dir(&store) else {
            return Vec::new();
        };
        let mut ids: Vec<String> = read
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| is_snapshot_id(name))
            .collect();
        // the id shape sorts lexically as time: newest first is one
        // descending sort, no stat needed
        ids.sort();
        ids.reverse();
        ids.into_iter()
            .map(|id| Snapshot {
                when: humanize(&id),
                id,
            })
            .collect()
    }

    /// Where one snapshot's copy of `dir` sits: a plain directory any
    /// listing can read. `None` when the id is not a snapshot's name,
    /// the folder is outside the target, or the copy is gone (pruned
    /// mid-view; the browser renders an empty listing, which is what
    /// the folder is now).
    pub fn resolve(&self, dir: &Path, id: &str) -> Option<PathBuf> {
        if !is_snapshot_id(id) {
            return None;
        }
        let relative = dir.strip_prefix(&self.target).ok()?;
        let resolved = self.store().join(id).join(relative);
        resolved.is_dir().then_some(resolved)
    }

    /// The folder's path inside the target, or `None` when the folder
    /// is not a target resident (or is the store, or lives in it).
    fn relative(&self, dir: &Path) -> Option<PathBuf> {
        let relative = dir.strip_prefix(&self.target).ok()?;
        if relative.starts_with(Path::new(".snapshots")) {
            return None;
        }
        Some(relative.to_path_buf())
    }
}

/// The names the snapshot engine writes, and nothing else: a
/// directory someone dropped in the store is not a snapshot and is
/// never offered as one. The shape check mirrors kumaOS's own
/// validator digit for digit: digits everywhere but the two dashes
/// and the T. The engine only writes real timestamps (`date
/// +%Y-%m-%dT%H%M%S`), so anything shape-valid it did not write is a
/// problem for the store, not for this check.
fn is_snapshot_id(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() != 17 || bytes[10] != b'T' {
        return false;
    }
    bytes.iter().enumerate().all(|(ix, byte)| match ix {
        4 | 7 => *byte == b'-',
        10 => *byte == b'T',
        _ => byte.is_ascii_digit(),
    })
}

/// `2026-08-08T134347` reads as a filename; `2026-08-08 13:43:47`
/// reads as a moment. The id shape is guaranteed above, so the splice
/// cannot miss.
pub(crate) fn humanize(id: &str) -> String {
    format!(
        "{} {}:{}:{}",
        &id[..10],
        &id[11..13],
        &id[13..15],
        &id[15..17]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthesized store: `base` plays the target, `.snapshots`
    /// holds two real-shaped ids, a wrongly-shaped impostor, and a
    /// snapshot of `home/martin/Documents` with one file in it.
    struct Store {
        base: PathBuf,
    }

    impl Store {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir().join(format!("koguma-snap-{name}-{}", std::process::id()));
            let store = base.join(".snapshots");
            for id in ["2026-08-08T134347", "2026-10-20T091500"] {
                let doc = store.join(id).join("home/martin/Documents");
                std::fs::create_dir_all(&doc).unwrap();
                std::fs::write(doc.join("notes.txt"), "as it was").unwrap();
            }
            std::fs::create_dir_all(store.join("not-a-snapshot")).unwrap();
            std::fs::create_dir_all(store.join("2026-08-08T1343")).unwrap();
            Self { base }
        }

        fn snapshots(&self) -> KumaSnapshots {
            KumaSnapshots {
                target: self.base.clone(),
            }
        }

        fn documents(&self) -> PathBuf {
            self.base.join("home/martin/Documents")
        }
    }

    impl Drop for Store {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn only_engine_shaped_names_are_snapshots() {
        assert!(is_snapshot_id("2026-08-08T134347"));
        assert!(!is_snapshot_id("2026-08-08 134347"));
        assert!(!is_snapshot_id("not-a-snapshot"));
        // a dropped-in directory with a date-looking name is still not
        // a snapshot unless the whole shape holds
        assert!(!is_snapshot_id("2026-08-08T1343"));
        assert!(!is_snapshot_id("2026-08-08T1343470"));
        // the shape check mirrors kumaOS's validator, which reads the
        // digits, not the calendar: shape-valid names the engine never
        // wrote are the store's business, not this check's
        assert!(is_snapshot_id("2026-13-99T999999"));
    }

    #[test]
    fn the_picker_lists_real_ids_newest_first() {
        let store = Store::new("picker");
        let snaps = store.snapshots().snapshots_for(&store.documents());
        assert_eq!(snaps.len(), 2, "the impostors must not ride");
        assert_eq!(snaps[0].id, "2026-10-20T091500");
        assert_eq!(snaps[0].when, "2026-10-20 09:15:00");
        assert_eq!(snaps[1].id, "2026-08-08T134347");
    }

    #[test]
    fn resolve_splices_the_folder_into_the_snapshot() {
        let store = Store::new("resolve");
        let resolved = store
            .snapshots()
            .resolve(&store.documents(), "2026-08-08T134347")
            .unwrap();
        assert_eq!(
            resolved,
            store
                .base
                .join(".snapshots/2026-08-08T134347/home/martin/Documents")
        );
        // and the copy really reads: the file the engine "took" is there
        assert_eq!(
            std::fs::read_to_string(resolved.join("notes.txt")).unwrap(),
            "as it was"
        );
    }

    #[test]
    fn resolve_refuses_bad_ids_and_pruned_snapshots() {
        let store = Store::new("refuse");
        let snapshots = store.snapshots();
        assert!(snapshots.resolve(&store.documents(), "not-a-snapshot").is_none());
        assert!(
            snapshots
                .resolve(&store.documents(), "2026-13-99T999999")
                .is_none(),
            "a shape-valid id the store does not hold is a pruned snapshot"
        );
        // the folder has no copy in the one real snapshot? it does;
        // but an id that exists without the folder does not resolve
        std::fs::create_dir_all(store.base.join(".snapshots/2026-08-08T134347/home/other")).unwrap();
        assert!(
            snapshots
                .resolve(&store.base.join("home/other"), "2026-08-08T134347")
                .is_some()
        );
    }

    #[test]
    fn folders_outside_the_target_have_no_timeline() {
        let store = Store::new("outside");
        let snapshots = store.snapshots();
        assert!(snapshots.snapshots_for(&std::env::temp_dir()).is_empty());
        assert!(snapshots.resolve(&std::env::temp_dir(), "2026-08-08T134347").is_none());
    }

    #[test]
    fn the_store_itself_is_not_offered_snapshots() {
        // inside the store, every directory is a snapshot's interior;
        // browsing there must not offer snapshots of snapshots
        let store = Store::new("nested");
        let inside = store
            .base
            .join(".snapshots/2026-08-08T134347/home/martin/Documents");
        assert!(store.snapshots().snapshots_for(&inside).is_empty());
        assert!(store.snapshots().relative(&store.base.join(".snapshots")).is_none());
    }
}
