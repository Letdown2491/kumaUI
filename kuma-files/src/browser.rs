use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{collections::HashMap, fs, io};

use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Div, ExternalDragPayload, ExternalPaths,
    FileDragPaths, FocusHandle, Focusable, ImageSource, KeyDownEvent, MouseButton, ObjectFit,
    Pixels, Point, Render, RenderImage, Stateful, Window, div, img, prelude::*, px, rgba, rgb, svg,
};
use trash::{os_limited, TrashItem};

use crate::{icons, theme};

/// One drag origin. The external payload resolver needs the path plus
/// whether it is a directory, so the platform drag advertises both.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct DragEntry {
    pub(crate) path: PathBuf,
    pub(crate) is_dir: bool,
}

#[derive(Clone)]
struct Entry {
    /// Directory view: the file's path. Trash view: the trash item's
    /// `.trashinfo` path, unique even when two trashed files share an
    /// original name. Selection and cursor key off this.
    key: PathBuf,
    path: PathBuf,
    name: String,
    is_dir: bool,
    size: Option<u64>,
    item: Option<TrashItem>,
}

#[derive(Clone)]
enum Source {
    Dir(PathBuf),
    Trash,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    List,
    Icons,
}

impl PartialEq for Source {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Source::Dir(a), Source::Dir(b)) => a == b,
            (Source::Trash, Source::Trash) => true,
            _ => false,
        }
    }
}
impl Source {
    fn label(&self) -> String {
        match self {
            Source::Dir(dir) => dir
                .file_name()
                .map_or_else(
                    || dir.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                ),
            Source::Trash => "Trash".into(),
        }
    }
}

struct Tab {
    source: Source,
    entries: Vec<Entry>,
    selection: HashSet<PathBuf>,
    cursor: Option<usize>,
    history: Vec<Source>,
    forward: Vec<Source>,
    renaming: Option<PathBuf>,
    rename_buffer: String,
    /// Byte offset into rename_buffer; stays on char boundaries.
    rename_cursor: usize,
    view_mode: ViewMode,
}

/// A queued file operation. `run` executes it and returns its inverse,
/// so the undo stack is the same type and Ctrl+Z walks it back.
#[derive(Clone, Debug)]
enum Op {
    Copy { from: PathBuf, to: PathBuf, replace: bool },
    Move { from: PathBuf, to: PathBuf, replace: bool },
    Remove { path: PathBuf, is_dir: bool },
    Trash { paths: Vec<PathBuf> },
    Restore { items: Vec<TrashItem> },
    Purge { items: Vec<TrashItem> },
    /// A conflict resolved as Skip: filtered out before the queue runs.
    Nop,
}

impl Op {
    fn verb(&self) -> &'static str {
        match self {
            Op::Copy { .. } => "copying",
            Op::Move { .. } => "moving",
            Op::Remove { .. } => "removing",
            Op::Trash { .. } => "trashing",
            Op::Restore { .. } => "restoring",
            Op::Purge { .. } => "purging",
            Op::Nop => "skipping",
        }
    }

    fn run(&self) -> Result<Option<Op>, String> {
        match self {
            Op::Nop => Ok(None),
            Op::Copy { from, to, replace } => {
                if to.exists() && !replace {
                    return Err(format!("{} already exists", to.display()));
                }
                if from.is_dir() {
                    copy_dir_all(from, to).map_err(|err| format!("copy: {err}"))?;
                } else {
                    fs::copy(from, to).map_err(|err| format!("copy: {err}"))?;
                }
                Ok(Some(Op::Remove {
                    path: to.clone(),
                    is_dir: from.is_dir(),
                }))
            }
            Op::Move { from, to, replace } => {
                if to.exists() {
                    if !replace {
                        return Err(format!("{} already exists", to.display()));
                    }
                    // rename will not replace, so clear the way first
                    let removed = if to.is_dir() {
                        fs::remove_dir_all(to)
                    } else {
                        fs::remove_file(to)
                    };
                    removed.map_err(|err| format!("move: {err}"))?;
                }
                fs::rename(from, to).map_err(|err| format!("move: {err}"))?;
                Ok(Some(Op::Move {
                    from: to.clone(),
                    to: from.clone(),
                    replace: false,
                }))
            }
            Op::Remove { path, is_dir } => {
                let result = if *is_dir {
                    fs::remove_dir_all(path)
                } else {
                    fs::remove_file(path)
                };
                result.map_err(|err| format!("remove: {err}"))?;
                Ok(None)
            }
            Op::Trash { paths } => {
                trash::delete_all(paths).map_err(|err| format!("trash: {err}"))?;
                let all = os_limited::list().map_err(|err| format!("trash list: {err}"))?;
                let items: Vec<TrashItem> = all
                    .into_iter()
                    .filter(|item| paths.contains(&item.original_path()))
                    .collect();
                Ok(Some(Op::Restore { items }))
            }
            Op::Restore { items } => {
                os_limited::restore_all(items.clone()).map_err(|err| format!("restore: {err}"))?;
                Ok(Some(Op::Trash {
                    paths: items.iter().map(|item| item.original_path()).collect(),
                }))
            }
            Op::Purge { items } => {
                os_limited::purge_all(items.clone()).map_err(|err| format!("purge: {err}"))?;
                Ok(None)
            }
        }
    }
}

fn copy_dir_all(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

struct Place {
    name: String,
    path: PathBuf,
}

/// A paste that may collide with an existing file, resolved by the
/// conflict dialog before the queue runs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OpKind {
    Copy,
    Move,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlanState {
    Ready,
    Conflict,
    Skipped,
}

struct PlannedOp {
    kind: OpKind,
    from: PathBuf,
    to: PathBuf,
    replace: bool,
    state: PlanState,
}

impl PlannedOp {
    fn into_op(self) -> Op {
        match self.state {
            PlanState::Skipped | PlanState::Conflict => Op::Nop,
            PlanState::Ready => match self.kind {
                OpKind::Copy => Op::Copy {
                    from: self.from,
                    to: self.to,
                    replace: self.replace,
                },
                OpKind::Move => Op::Move {
                    from: self.from,
                    to: self.to,
                    replace: self.replace,
                },
            },
        }
    }
}

#[derive(Clone, Copy)]
enum ConflictDecision {
    Replace,
    KeepBoth,
    Skip,
}

struct ConflictDialog {
    ops: Vec<PlannedOp>,
    /// Indices into `ops` of the entries that collided.
    conflicts: Vec<usize>,
    /// Replace is only offered when both sides are plain files.
    can_replace_all: bool,
    ix: usize,
    apply_all: bool,
}

/// "name (copy).ext", then "name (copy 2).ext", and so on. After 999
/// colliding copies this gives up and returns `to` itself; the queue
/// reports the "already exists" error, which is honest enough.
fn unique_dest(to: &Path) -> PathBuf {
    let stem = to
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = to
        .extension()
        .map(|e| e.to_string_lossy().into_owned());
    for n in 1..1000 {
        let suffix = if n == 1 {
            " (copy)".to_string()
        } else {
            format!(" (copy {n})")
        };
        let name = match &ext {
            Some(ext) => format!("{stem}{suffix}.{ext}"),
            None => format!("{stem}{suffix}"),
        };
        let candidate = to.with_file_name(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    to.to_path_buf()
}

pub(crate) struct Browser {
    tabs: Vec<Tab>,
    active: usize,
    clipboard: Option<(bool, Vec<PathBuf>)>,
    undo: Vec<Op>,
    conflict_dialog: Option<ConflictDialog>,
    status: String,
    progress: String,
    busy: bool,
    focus: FocusHandle,
    places: Vec<Place>,
    purge_armed: Option<Instant>,
    show_hidden: bool,
    /// Decoded thumbnails keyed by path; cleared wholesale when large.
    thumbs: HashMap<PathBuf, Arc<RenderImage>>,
    thumbs_inflight: HashSet<PathBuf>,
}

impl Focusable for Browser {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

const PURGE_ARM: Duration = Duration::from_secs(5);

impl Browser {
    pub(crate) fn new(dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let mut browser = Self {
            tabs: vec![Tab::new(Source::Dir(dir))],
            active: 0,
            clipboard: None,
            undo: Vec::new(),
            conflict_dialog: None,
            status: String::new(),
            progress: String::new(),
            busy: false,
            focus,
            places: Self::places(),
            purge_armed: None,
            show_hidden: false,
            thumbs: HashMap::new(),
            thumbs_inflight: HashSet::new(),
        };
        let show_hidden = browser.show_hidden;
        browser.tab_mut().reload(show_hidden);
        browser
    }

    /// XDG user directories; missing ones just don't render ("ask, don't
    /// assume": the sidebar is a function of what the machine answers).
    fn places() -> Vec<Place> {
        let mut places = Vec::new();
        let mut push = |name: &str, path: Option<PathBuf>| {
            if let Some(path) = path {
                places.push(Place {
                    name: name.into(),
                    path,
                });
            }
        };
        push("Home", dirs::home_dir());
        push("Desktop", dirs::desktop_dir());
        push("Documents", dirs::document_dir());
        push("Downloads", dirs::download_dir());
        push("Music", dirs::audio_dir());
        push("Pictures", dirs::picture_dir());
        push("Videos", dirs::video_dir());
        places
    }

    fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    /// Kick off a background decode for an image file; the cached result
    /// arrives with a notify. Safe to call every render: inflight and
    /// cached paths are no-ops.
    fn request_thumb(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.thumbs.contains_key(&path) || self.thumbs_inflight.contains(&path) {
            return;
        }
        let Ok(meta) = fs::symlink_metadata(&path) else {
            return;
        };
        // decoding cost is in the file read, not the resize; cap it
        if meta.len() > 32 * 1024 * 1024 {
            return;
        }
        self.thumbs_inflight.insert(path.clone());
        if self.thumbs.len() > 300 {
            self.thumbs.clear();
        }
        cx.spawn(async move |this, cx| {
            let bg_path = path.clone();
            let render = cx
                .background_spawn(async move {
                    std::panic::catch_unwind(|| {
                        icons::decode_thumbnail(&bg_path, 256, 256)
                    })
                    .unwrap_or(None)
                })
                .await;
            let update = this.update(cx, |this, cx| {
                this.thumbs_inflight.remove(&path);
                if let Some(render) = render {
                    this.thumbs.insert(path, Arc::new(render));
                    cx.notify();
                }
            });
            if let Err(err) = update {
                log::error!("thumbnail update failed: {err:#}");
            }
        })
        .detach();
    }

    /// New folder in the current directory, named to not collide.
    fn new_folder(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.tab().current_dir().map(Path::to_path_buf) else {
            return;
        };
        let mut name = "New Folder".to_string();
        let mut n = 2;
        while dir.join(&name).exists() {
            name = format!("New Folder {n}");
            n += 1;
        }
        match fs::create_dir(dir.join(&name)) {
            Ok(()) => {
                self.status = format!("created {name}");
                let show_hidden = self.show_hidden;
                let tab = self.tab_mut();
                tab.reload(show_hidden);
                let new_path = dir.join(&name);
                if let Some(entry) = tab.entries.iter().position(|e| e.path == new_path) {
                    tab.cursor = Some(entry);
                    tab.selection.clear();
                    tab.selection.insert(new_path);
                }
            }
            Err(err) => {
                log::error!("create_dir: {err}");
                self.status = format!("new folder failed: {err}");
            }
        }
        cx.notify();
    }

    fn toggle_hidden(&mut self, cx: &mut Context<Self>) {
        self.show_hidden = !self.show_hidden;
        let show_hidden = self.show_hidden;
        for tab in &mut self.tabs {
            tab.reload(show_hidden);
        }
        self.status = if self.show_hidden {
            "hidden files shown".into()
        } else {
            "hidden files hidden".into()
        };
        cx.notify();
    }

    fn set_view_mode(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        let tab = self.tab_mut();
        if tab.view_mode != mode {
            tab.view_mode = mode;
            cx.notify();
        }
    }

    fn open_trash(&mut self, cx: &mut Context<Self>) {
        self.load_source(Source::Trash, cx);
    }

    fn load_source(&mut self, source: Source, cx: &mut Context<Self>) {
        let show_hidden = self.show_hidden;
        let tab = self.tab_mut();
        if tab.source == source {
            return;
        }
        tab.history.push(tab.source.clone());
        tab.forward.clear();
        tab.source = source;
        tab.selection.clear();
        tab.cursor = None;
        tab.renaming = None;
        tab.reload(show_hidden);
        self.status.clear();
        self.purge_armed = None;
        cx.notify();
    }

    fn new_tab(&mut self, cx: &mut Context<Self>) {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        self.tabs.push(Tab::new(Source::Dir(home)));
        self.active = self.tabs.len() - 1;
        self.tabs.last_mut().unwrap().reload(self.show_hidden);
        self.status.clear();
        cx.notify();
    }

    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.tabs.len() == 1 {
            // the last tab becomes a fresh home tab rather than closing
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            self.tabs[0] = Tab::new(Source::Dir(home));
            self.tabs[0].reload(self.show_hidden);
        } else {
            self.tabs.remove(ix);
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len() - 1;
            } else if ix < self.active {
                self.active -= 1;
            }
        }
        cx.notify();
    }

    fn enqueue(&mut self, mut ops: Vec<Op>, cx: &mut Context<Self>) {
        ops.retain(|op| !matches!(op, Op::Nop));
        if ops.is_empty() {
            return;
        }
        if self.busy {
            self.status = "a file operation is still running".into();
            cx.notify();
            return;
        }
        self.busy = true;
        self.purge_armed = None;
        let total = ops.len();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let mut undo = Vec::new();
            let mut failure: Option<String> = None;
            for (i, op) in ops.drain(..).enumerate() {
                let update = this.update(cx, |this, cx| {
                    this.progress = format!("{} {} of {}", op.verb(), i + 1, total);
                    cx.notify();
                });
                if let Err(err) = update {
                    log::error!("progress update failed: {err:#}");
                    return;
                }
                let outcome = {
                    let run_op = op.clone();
                    cx.background_spawn(async move { run_op.run() }).await
                };
                match outcome {
                    Ok(Some(inverse)) => undo.push(inverse),
                    Ok(None) => {}
                    Err(err) => {
                        failure = Some(format!("{} failed: {err}", op.verb()));
                        break;
                    }
                }
            }
            let update = this.update(cx, |this, cx| {
                this.busy = false;
                this.progress.clear();
                match failure {
                    Some(err) => this.status = err,
                    None if !undo.is_empty() => {
                        this.status = "done; Ctrl+Z to undo".into();
                        this.undo.extend(undo);
                    }
                    None => {}
                }
                let show_hidden = this.show_hidden;
                this.tab_mut().reload(show_hidden);
                cx.notify();
            });
            if let Err(err) = update {
                log::error!("queue completion update failed: {err:#}");
            }
        })
        .detach();
    }

    fn copy_selection(&mut self, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self
            .tab()
            .entries
            .iter()
            .filter(|entry| self.tab().selection.contains(&entry.key))
            .map(|entry| entry.path.clone())
            .collect();
        if paths.is_empty() {
            return;
        }
        self.clipboard = Some((true, paths));
        self.status = "copied; Ctrl+V to paste".into();
        cx.notify();
    }

    fn cut_selection(&mut self, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self
            .tab()
            .entries
            .iter()
            .filter(|entry| self.tab().selection.contains(&entry.key))
            .map(|entry| entry.path.clone())
            .collect();
        if paths.is_empty() {
            return;
        }
        self.clipboard = Some((false, paths));
        self.status = "cut; Ctrl+V to paste".into();
        cx.notify();
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some((is_copy, paths)) = self.clipboard.clone() else {
            return;
        };
        let Source::Dir(dest_dir) = self.tab().source.clone() else {
            self.status = "paste goes to a folder; trash has no paste".into();
            cx.notify();
            return;
        };
        let mut ops = Vec::new();
        let mut conflicts = Vec::new();
        let mut can_replace_all = true;
        for path in paths {
            let name = path.file_name().unwrap_or_default();
            let to = dest_dir.join(name);
            let conflict = to.exists();
            let can_replace = path.is_file() && to.is_file();
            can_replace_all &= !conflict || can_replace;
            ops.push(PlannedOp {
                kind: if is_copy { OpKind::Copy } else { OpKind::Move },
                from: path,
                to,
                replace: false,
                state: if conflict {
                    PlanState::Conflict
                } else {
                    PlanState::Ready
                },
            });
            if conflict {
                conflicts.push(ops.len() - 1);
            }
        }
        if !is_copy {
            self.clipboard = None;
        }
        if conflicts.is_empty() {
            self.enqueue(ops.into_iter().map(PlannedOp::into_op).collect(), cx);
        } else {
            self.conflict_dialog = Some(ConflictDialog {
                ops,
                conflicts,
                can_replace_all,
                ix: 0,
                apply_all: false,
            });
            self.status = "a file with that name already exists".into();
            cx.notify();
        }
    }

    /// Resolve the conflict under the cursor with Replace / Keep both /
    /// Skip, or all of them at once when "apply to all" is checked.
    fn conflict_decide(&mut self, decision: ConflictDecision, cx: &mut Context<Self>) {
        let Some(mut dialog) = self.conflict_dialog.take() else {
            return;
        };
        let count = if dialog.apply_all {
            dialog.conflicts.len() - dialog.ix
        } else {
            1
        };
        for _ in 0..count {
            if dialog.ix >= dialog.conflicts.len() {
                break;
            }
            let op_ix = dialog.conflicts[dialog.ix];
            let op = &mut dialog.ops[op_ix];
            match decision {
                ConflictDecision::Replace => {
                    op.replace = true;
                    op.state = PlanState::Ready;
                }
                ConflictDecision::Skip => op.state = PlanState::Skipped,
                ConflictDecision::KeepBoth => {
                    op.to = unique_dest(&op.to);
                    op.state = PlanState::Ready;
                }
            }
            dialog.ix += 1;
        }
        if dialog.ix < dialog.conflicts.len() {
            self.conflict_dialog = Some(dialog);
        } else {
            self.enqueue(dialog.ops.into_iter().map(PlannedOp::into_op).collect(), cx);
        }
        cx.notify();
    }

    /// Esc during a conflict means skip the lot.
    fn conflict_skip_all(&mut self, cx: &mut Context<Self>) {
        let Some(mut dialog) = self.conflict_dialog.take() else {
            return;
        };
        while dialog.ix < dialog.conflicts.len() {
            dialog.ops[dialog.conflicts[dialog.ix]].state = PlanState::Skipped;
            dialog.ix += 1;
        }
        self.enqueue(dialog.ops.into_iter().map(PlannedOp::into_op).collect(), cx);
    }

    fn undo_last(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(op) = self.undo.pop() else {
            return;
        };
        log::info!("undo: {op:?}");
        self.enqueue(vec![op], cx);
    }

    fn trash_selection(&mut self, cx: &mut Context<Self>) {
        if self.tab().source == Source::Trash {
            self.purge_selection(cx);
            return;
        }
        let paths: Vec<PathBuf> = self
            .tab()
            .entries
            .iter()
            .filter(|entry| self.tab().selection.contains(&entry.key))
            .map(|entry| entry.path.clone())
            .collect();
        if paths.is_empty() {
            return;
        }
        self.enqueue(vec![Op::Trash { paths }], cx);
    }

    /// Purge is the one act with no inverse, so it wears the shell's
    /// arm-then-confirm pattern: the first Delete arms for 5s, the second
    /// within the window goes through.
    fn purge_selection(&mut self, cx: &mut Context<Self>) {
        let items: Vec<TrashItem> = self
            .tab()
            .entries
            .iter()
            .filter(|entry| self.tab().selection.contains(&entry.key))
            .filter_map(|entry| entry.item.clone())
            .collect();
        if items.is_empty() {
            return;
        }
        match self.purge_armed {
            Some(armed) if armed.elapsed() <= PURGE_ARM => {
                self.purge_armed = None;
                self.enqueue(vec![Op::Purge { items }], cx);
            }
            _ => {
                self.purge_armed = Some(Instant::now());
                self.status = "press Delete again to purge permanently (no undo)".into();
                cx.notify();
            }
        }
    }
}

impl Tab {
    fn new(source: Source) -> Self {
        Self {
            source,
            entries: Vec::new(),
            selection: HashSet::new(),
            cursor: None,
            history: Vec::new(),
            forward: Vec::new(),
            renaming: None,
            rename_buffer: String::new(),
            rename_cursor: 0,
            view_mode: ViewMode::List,
        }
    }

    fn reload(&mut self, show_hidden: bool) {
        match &self.source {
            Source::Dir(dir) => match fs::read_dir(dir) {
                Ok(read) => {
                    let mut entries = Vec::new();
                    for entry in read.flatten() {
                        let path = entry.path();
                        let name = entry.file_name().to_string_lossy().into_owned();
                        if !show_hidden && name.starts_with('.') {
                            continue;
                        }
                        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
                        let size = fs::symlink_metadata(&path).ok().and_then(|meta| {
                            if meta.is_dir() {
                                None
                            } else {
                                Some(meta.len())
                            }
                        });
                        entries.push(Entry {
                            key: path.clone(),
                            path,
                            name,
                            is_dir,
                            size,
                            item: None,
                        });
                    }
                    entries
                        .sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
                    self.entries = entries;
                }
                Err(err) => {
                    log::error!("read_dir {}: {err}", dir.display());
                    self.entries.clear();
                }
            },
            Source::Trash => {
                let items = match os_limited::list() {
                    Ok(items) => items,
                    Err(err) => {
                        log::error!("trash list: {err}");
                        self.entries.clear();
                        return;
                    }
                };
                let mut entries = Vec::new();
                for item in items {
                    let size = os_limited::metadata(&item)
                        .ok()
                        .map(|meta| meta.size)
                        .and_then(|size| size.size());
                    let is_dir = os_limited::metadata(&item)
                        .ok()
                        .map(|meta| matches!(meta.size, trash::TrashItemSize::Entries(_)))
                        .unwrap_or(false);
                    entries.push(Entry {
                        key: PathBuf::from(&item.id),
                        path: item.original_path(),
                        name: item.name.to_string_lossy().into_owned(),
                        is_dir,
                        size,
                        item: Some(item),
                    });
                }
                entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                self.entries = entries;
            }
        }
        self.selection.retain(|key| {
            self.entries
                .iter()
                .any(|entry| &entry.key == key)
        });
        if let Some(ix) = self.cursor
            && self.entries.get(ix).is_none()
        {
            self.cursor = None;
        }
    }

    fn current_dir(&self) -> Option<&Path> {
        match &self.source {
            Source::Dir(dir) => Some(dir.as_path()),
            Source::Trash => None,
        }
    }
}

impl Browser {
    fn route_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = event.keystroke.clone();

        if self.conflict_dialog.is_some() {
            match keystroke.key.as_str() {
                "escape" => self.conflict_skip_all(cx),
                _ => {}
            }
            return;
        }

        let tab = self.tab_mut();

        if tab.renaming.is_some() {
            match keystroke.key.as_str() {
                "enter" => self.commit_rename(cx),
                "escape" => {
                    tab.renaming = None;
                    cx.notify();
                }
                "backspace" => {
                    if tab.rename_cursor > 0 {
                        let head = &tab.rename_buffer[..tab.rename_cursor];
                        if let Some((prev, _)) = head.char_indices().next_back() {
                            tab.rename_buffer.remove(prev);
                            tab.rename_cursor = prev;
                        }
                    }
                    cx.notify();
                }
                "delete" => {
                    if tab.rename_buffer.is_char_boundary(tab.rename_cursor)
                        && tab.rename_cursor < tab.rename_buffer.len()
                    {
                        tab.rename_buffer.remove(tab.rename_cursor);
                    }
                    cx.notify();
                }
                "left" => {
                    if tab.rename_cursor > 0 {
                        let head = &tab.rename_buffer[..tab.rename_cursor];
                        if let Some((prev, _)) = head.char_indices().next_back() {
                            tab.rename_cursor = prev;
                        }
                    }
                    cx.notify();
                }
                "right" => {
                    if tab.rename_buffer.is_char_boundary(tab.rename_cursor)
                        && tab.rename_cursor < tab.rename_buffer.len()
                    {
                        let tail = &tab.rename_buffer[tab.rename_cursor..];
                        if let Some(ch) = tail.chars().next() {
                            tab.rename_cursor += ch.len_utf8();
                        }
                    }
                    cx.notify();
                }
                "home" => {
                    tab.rename_cursor = 0;
                    cx.notify();
                }
                "end" => {
                    tab.rename_cursor = tab.rename_buffer.len();
                    cx.notify();
                }
                _ if !keystroke.modifiers.control
                    && !keystroke.modifiers.alt
                    && !keystroke.modifiers.platform
                    && !keystroke.modifiers.function =>
                {
                    if let Some(character) = keystroke.key_char.as_deref() {
                        let cursor = if tab.rename_buffer.is_char_boundary(tab.rename_cursor) {
                            tab.rename_cursor
                        } else {
                            tab.rename_buffer.len()
                        };
                        tab.rename_buffer.insert_str(cursor, character);
                        tab.rename_cursor = cursor + character.len();
                        cx.notify();
                    }
                }
                _ => {}
            }
            return;
        }

        match keystroke.key.as_str() {
            "enter" => self.open_selection(cx),
            "backspace" => self.go_up(cx),
            "escape" => {
                tab.selection.clear();
                tab.cursor = None;
                self.purge_armed = None;
                cx.notify();
            }
            "delete" => self.trash_selection(cx),
            "f2" => self.start_rename(cx),
            "left" if keystroke.modifiers.alt => self.go_back(cx),
            "right" if keystroke.modifiers.alt => self.go_forward(cx),
            "down" => self.move_cursor(1, cx),
            "up" => self.move_cursor(-1, cx),
            "home" => self.jump_cursor(0, cx),
            "end" => {
                let last = self.tab().entries.len().saturating_sub(1);
                self.jump_cursor(last, cx);
            },
            "a" if keystroke.modifiers.control => {
                tab.selection = tab.entries.iter().map(|e| e.key.clone()).collect();
                cx.notify();
            }
            "c" if keystroke.modifiers.control => self.copy_selection(cx),
            "x" if keystroke.modifiers.control => self.cut_selection(cx),
            "v" if keystroke.modifiers.control => self.paste(cx),
            "z" if keystroke.modifiers.control => self.undo_last(cx),
            "t" if keystroke.modifiers.control => self.new_tab(cx),
            "w" if keystroke.modifiers.control => {
                let active = self.active;
                self.close_tab(active, cx);
            }
            "h" if keystroke.modifiers.control && !keystroke.modifiers.shift => {
                self.toggle_hidden(cx)
            }
            "1" if keystroke.modifiers.control => self.set_view_mode(ViewMode::List, cx),
            "2" if keystroke.modifiers.control => self.set_view_mode(ViewMode::Icons, cx),
            "n" if keystroke.modifiers.control && keystroke.modifiers.shift => {
                self.new_folder(cx)
            }
            _ => {}
        }
    }

    fn move_cursor(&mut self, step: isize, cx: &mut Context<Self>) {
        let tab = self.tab_mut();
        if tab.entries.is_empty() {
            return;
        }
        let count = tab.entries.len() as isize;
        let current = tab.cursor.map_or(-1, |ix| ix as isize);
        let next = (current + step).clamp(0, count - 1);
        tab.cursor = Some(next as usize);
        let key = tab.entries[next as usize].key.clone();
        tab.selection.clear();
        tab.selection.insert(key);
        cx.notify();
    }

    /// Home/End: jump to first/last entry. A no-op on an empty listing.
    fn jump_cursor(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.tab().entries.is_empty() {
            return;
        }
        let tab = self.tab_mut();
        let ix = ix.min(tab.entries.len() - 1);
        tab.cursor = Some(ix);
        let key = tab.entries[ix].key.clone();
        tab.selection.clear();
        tab.selection.insert(key);
        cx.notify();
    }

    fn open_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(ix) = self.tab().cursor
            && let Some(entry) = self.tab().entries.get(ix).cloned()
        {
            self.open(&entry, cx);
        }
    }

    fn open(&mut self, entry: &Entry, cx: &mut Context<Self>) {
        if entry.item.is_some() {
            // trash view: Enter restores
            if let Some(item) = entry.item.clone() {
                self.enqueue(vec![Op::Restore { items: vec![item] }], cx);
            }
            return;
        }
        if entry.is_dir {
            self.load_source(Source::Dir(entry.path.clone()), cx);
        } else {
            match Command::new("xdg-open").arg(&entry.path).spawn() {
                Ok(_) => self.status = format!("opened {}", entry.name),
                Err(err) => {
                    log::error!("xdg-open {}: {err}", entry.path.display());
                    self.status = format!("open failed: {err}");
                }
            }
            cx.notify();
        }
    }

    fn start_rename(&mut self, cx: &mut Context<Self>) {
        if self.tab().source == Source::Trash {
            return;
        }
        let tab = self.tab_mut();
        let Some(ix) = tab.cursor else {
            return;
        };
        let Some(entry) = tab.entries.get(ix) else {
            return;
        };
        tab.renaming = Some(entry.path.clone());
        tab.rename_buffer = entry.name.clone();
        tab.rename_cursor = tab.rename_buffer.len();
        cx.notify();
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let tab = self.tab_mut();
        let Some(renaming) = tab.renaming.take() else {
            return;
        };
        let target_name = tab.rename_buffer.trim().to_owned();
        let current_name = renaming
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        if target_name.is_empty() || Some(target_name.as_str()) == current_name.as_deref() {
            cx.notify();
            return;
        }
        let dest = renaming.with_file_name(&target_name);
        match fs::rename(&renaming, &dest) {
            Ok(()) => {
                log::info!("rename: {} -> {}", renaming.display(), dest.display());
                self.status = format!("renamed to {target_name}");
                let tab = self.tab_mut();
                tab.selection.retain(|key| *key != renaming);
                if let Some(entry) = tab.entries.iter_mut().find(|e| e.path == renaming) {
                    entry.path = dest;
                    entry.name = target_name;
                    entry.key = entry.path.clone();
                }
            }
            Err(err) => {
                log::error!("rename {} -> {}: {err}", renaming.display(), dest.display());
                self.status = format!("rename failed: {err}");
            }
        }
        let show_hidden = self.show_hidden;
        self.tab_mut().reload(show_hidden);
        cx.notify();
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        let show_hidden = self.show_hidden;
        let tab = self.tab_mut();
        let Some(previous) = tab.history.pop() else {
            return;
        };
        tab.forward.push(tab.source.clone());
        tab.source = previous;
        tab.selection.clear();
        tab.cursor = None;
        tab.renaming = None;
        tab.reload(show_hidden);
        cx.notify();
    }

    fn go_forward(&mut self, cx: &mut Context<Self>) {
        let show_hidden = self.show_hidden;
        let tab = self.tab_mut();
        let Some(next) = tab.forward.pop() else {
            return;
        };
        tab.history.push(tab.source.clone());
        tab.source = next;
        tab.selection.clear();
        tab.cursor = None;
        tab.renaming = None;
        tab.reload(show_hidden);
        cx.notify();
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.tab().current_dir().map(Path::to_path_buf) else {
            return;
        };
        if let Some(parent) = dir.parent().map(Path::to_path_buf) {
            self.load_source(Source::Dir(parent), cx);
        }
    }

    fn internal_move(&mut self, dragged: &DragEntry, target: &Entry, cx: &mut Context<Self>) {
        let name = dragged
            .path
            .file_name()
            .map_or_else(|| "unnamed".into(), |n| n.to_string_lossy().into_owned());
        let dest = target.path.join(&name);
        if dragged.path == dest || dragged.path == target.path {
            return;
        }
        self.enqueue(
            vec![Op::Move {
                from: dragged.path.clone(),
                to: dest,
                replace: false,
            }],
            cx,
        );
    }

    fn handle_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        self.route_key(event, cx);
    }

    fn row(&self, ix: usize, entry: &Entry, cx: &mut Context<Self>) -> Stateful<Div> {
        let selected = self.tab().selection.contains(&entry.key);
        let tab = self.tab();
        let renaming = tab.renaming.as_ref().is_some_and(|path| *path == entry.path);
        let in_trash = entry.item.is_some();
        let entry_key = entry.key.clone();
        let entry_path = entry.path.clone();
        let size_text = entry
            .size
            .map(human_size)
            .unwrap_or_else(|| String::from("folder"));

        let mut base = div()
            .id(ix)
            .flex()
            .items_center()
            .justify_between()
            .px_3()
            .py_1()
            .rounded_sm()
            .text_size(px(14.))
            .cursor_pointer()
            .bg(if selected {
                theme::row_selected()
            } else {
                theme::row()
            })
            .hover(|this| this.bg(theme::row_hover()))
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                if event.click_count() >= 2 && !in_trash {
                    if let Some(found) = this
                        .tab()
                        .entries
                        .iter()
                        .find(|e| e.path == entry_path)
                        .cloned()
                    {
                        this.open(&found, cx);
                    }
                    return;
                }
                let tab = this.tab_mut();
                if event.modifiers().control {
                    if tab.selection.contains(&entry_key) {
                        tab.selection.remove(&entry_key);
                    } else {
                        tab.selection.insert(entry_key.clone());
                    }
                } else {
                    tab.selection.clear();
                    tab.selection.insert(entry_key.clone());
                }
                tab.cursor = Some(ix);
                this.purge_armed = None;
                cx.notify();
            }));

        if in_trash {
            let origin = entry.path.display().to_string();
            base = base
                .drag_over::<DragEntry>(|style, _, _, _| style.bg(theme::drag_over()))
                .on_drop(cx.listener(move |this, _: &ExternalPaths, _, cx| {
                    this.status = "trash rows do not accept drops".into();
                    cx.notify();
                }));
            base = base
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(self.entry_icon(entry, px(16.)))
                        .child(entry.name.clone())
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme::text_dim())
                                .truncate()
                                .child(format!("from {}", origin)),
                        ),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::text_dim())
                        .child(size_text),
                );
            return base;
        }

        let dragged = DragEntry {
            path: entry.path.clone(),
            is_dir: entry.is_dir,
        };
        base = base
            .on_drag(dragged.clone(), |dragged: &DragEntry, position, _, cx| {
                let name = dragged
                    .path
                    .file_name()
                    .map_or_else(|| "unnamed".into(), |n| n.to_string_lossy().into_owned());
                log::info!("drag start: {}", dragged.path.display());
                cx.new(|_| Ghost { name, position })
            })
            .external_drag_payload::<DragEntry>(|dragged: &DragEntry, _, _| {
                log::info!("external payload resolved: {}", dragged.path.display());
                Some(ExternalDragPayload::Files(FileDragPaths::new([(
                    dragged.path.clone(),
                    dragged.is_dir,
                )])))
            });

        let name_child = if renaming {
            // the caret sits at rename_cursor (a byte offset on a char
            // boundary); render the buffer split around it
            let cursor = if tab.rename_buffer.is_char_boundary(tab.rename_cursor) {
                tab.rename_cursor
            } else {
                tab.rename_buffer.len()
            };
            let (before, after) = tab.rename_buffer.split_at(cursor);
            div()
                .flex_1()
                .border_1()
                .border_color(theme::accent())
                .rounded_sm()
                .px_1()
                .child(format!("{before}▏{after}"))
        } else {
            div().flex_1().truncate().child(entry.name.clone())
        };

        let base = if entry.is_dir {
            let target = entry.clone();
            let target_name = entry.name.clone();
            base.drag_over::<DragEntry>(|style, _, _, _| style.bg(theme::drag_over()))
                .drag_over::<ExternalPaths>(|style, _, _, _| style.bg(theme::drag_over()))
                .on_drop(cx.listener(move |this, dragged: &DragEntry, _, cx| {
                    this.internal_move(dragged, &target, cx);
                }))
                .on_drop(cx.listener(move |this, _paths: &ExternalPaths, _, cx| {
                    this.status =
                        format!("external drop onto {target_name}: imports land with the queue");
                    cx.notify();
                }))
        } else {
            base
        };

        base.child(self.entry_icon(entry, px(16.)))
            .child(name_child)
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::text_dim())
                    .child(size_text),
            )
    }

    /// The tinted type icon for an entry, at any size.
    fn entry_icon(&self, entry: &Entry, size: Pixels) -> AnyElement {
        let icon_path = icons::icon_path_for(&entry.name, entry.is_dir);
        let color = match icons::ink_for(icon_path) {
            icons::IconInk::Folder => theme::accent(),
            icons::IconInk::Doc => theme::text(),
            icons::IconInk::Plain => theme::text_dim(),
        };
        svg()
            .path(icon_path)
            .size(size)
            .flex_none()
            .text_color(color)
            .into_any_element()
    }

    /// One cell in the icon grid: preview (thumbnail when decodable),
    /// name underneath.
    fn icon_cell(&mut self, ix: usize, entry: &Entry, cx: &mut Context<Self>) -> Stateful<Div> {
        let selected = self.tab().selection.contains(&entry.key);
        let entry_key = entry.key.clone();
        let entry_path = entry.path.clone();
        let in_trash = entry.item.is_some();

        // thumbnails only for local image files: trash entries point at
        // paths that no longer exist
        let show_thumb = !in_trash && !entry.is_dir && icons::is_image(&entry.name);
        let thumb = if show_thumb {
            self.request_thumb(entry.path.clone(), cx);
            self.thumbs.get(&entry.path).cloned()
        } else {
            None
        };

        let preview: AnyElement = match thumb {
            Some(render) => img(ImageSource::Render(render))
                .size(px(76.))
                .object_fit(ObjectFit::Contain)
                .into_any_element(),
            None => {
                let (icon, ink) = if in_trash {
                    let icon = icons::icon_path_for(&entry.name, entry.is_dir);
                    (icon, icons::ink_for(icon))
                } else if show_thumb {
                    ("icons/image.svg", icons::IconInk::Plain)
                } else {
                    let icon = icons::icon_path_for(&entry.name, entry.is_dir);
                    (icon, icons::ink_for(icon))
                };
                let color = match ink {
                    icons::IconInk::Folder => theme::accent(),
                    icons::IconInk::Doc => theme::text(),
                    icons::IconInk::Plain => theme::text_dim(),
                };
                svg()
                    .path(icon)
                    .size(px(if entry.is_dir { 56. } else { 44. }))
                    .text_color(color)
                    .into_any_element()
            }
        };

        div()
            .id(ix)
            .w(px(112.))
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .p_2()
            .rounded_sm()
            .cursor_pointer()
            .bg(if selected {
                theme::row_selected()
            } else {
                theme::clear()
            })
            .hover(|this| this.bg(theme::row_hover()))
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                if event.click_count() >= 2 && !in_trash {
                    if let Some(found) = this
                        .tab()
                        .entries
                        .iter()
                        .find(|e| e.path == entry_path)
                        .cloned()
                    {
                        this.open(&found, cx);
                    }
                    return;
                }
                let tab = this.tab_mut();
                if event.modifiers().control {
                    if tab.selection.contains(&entry_key) {
                        tab.selection.remove(&entry_key);
                    } else {
                        tab.selection.insert(entry_key.clone());
                    }
                } else {
                    tab.selection.clear();
                    tab.selection.insert(entry_key.clone());
                }
                tab.cursor = Some(ix);
                this.purge_armed = None;
                cx.notify();
            }))
            .on_drag(
                DragEntry {
                    path: entry.path.clone(),
                    is_dir: entry.is_dir,
                },
                |dragged: &DragEntry, position, _, cx| {
                    let name = dragged
                        .path
                        .file_name()
                        .map_or_else(|| "unnamed".into(), |n| n.to_string_lossy().into_owned());
                    log::info!("drag start: {}", dragged.path.display());
                    cx.new(|_| Ghost { name, position })
                },
            )
            .external_drag_payload::<DragEntry>(|dragged: &DragEntry, _, _| {
                log::info!("external payload resolved: {}", dragged.path.display());
                Some(ExternalDragPayload::Files(FileDragPaths::new([(
                    dragged.path.clone(),
                    dragged.is_dir,
                )])))
            })
            .child(
                div()
                    .h(px(80.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(preview),
            )
            .child(
                div()
                    .max_w_full()
                    .text_size(px(12.))
                    .truncate()
                    .child(entry.name.clone()),
            )
    }

    fn place_row(&self, ix: usize, place: &Place, cx: &mut Context<Self>) -> Stateful<Div> {
        let here = self.tab().current_dir() == Some(place.path.as_path());
        let path = place.path.clone();
        div()
            .id(format!("place-{ix}"))
            .px_3()
            .py_1()
            .rounded_sm()
            .text_size(px(13.))
            .cursor_pointer()
            .text_color(if here {
                theme::accent()
            } else {
                theme::text_dim()
            })
            .bg(if here {
                theme::row_selected()
            } else {
                theme::clear()
            })
            .hover(|this| this.bg(theme::row_hover()))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.load_source(Source::Dir(path.clone()), cx);
            }))
            .child(place.name.clone())
    }

    /// The modal overlay for unresolved paste conflicts. `None` (rendered
    /// as no child) when no paste is waiting on a decision.
    fn conflict_overlay(&self, cx: &mut Context<Self>) -> Option<Div> {
        let dialog = self.conflict_dialog.as_ref()?;
        let op_ix = dialog.conflicts.get(dialog.ix).copied()?;
        let op = &dialog.ops[op_ix];
        let name = op
            .to
            .file_name()
            .map_or_else(|| op.to.display().to_string(), |n| n.to_string_lossy().into_owned());
        let can_replace = op.from.is_file() && op.to.is_file();
        let total = dialog.conflicts.len();
        let current = dialog.ix + 1;
        let verb = match op.kind {
            OpKind::Copy => "paste",
            OpKind::Move => "move",
        };

        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x00000066))
                .child(
                    div()
                        .w(px(420.))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_4()
                        .rounded_md()
                        .bg(theme::sidebar())
                        .border_1()
                        .border_color(theme::border())
                        .shadow_lg()
                        .text_size(px(13.))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_size(px(14.))
                                        .text_color(theme::text())
                                        .child(format!("\"{name}\" already exists here")),
                                )
                                .child(
                                    div()
                                        .text_color(theme::text_dim())
                                        .child(format!(
                                            "conflict {current} of {total} for this {verb}"
                                        )),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .children(can_replace.then(|| {
                                    div()
                                        .id("conflict-replace")
                                        .px_3()
                                        .py_1()
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .bg(theme::row_hover())
                                        .text_color(theme::error())
                                        .hover(|this| this.bg(theme::drag_over()))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.conflict_decide(
                                                ConflictDecision::Replace,
                                                cx,
                                            )
                                        }))
                                        .child("Replace")
                                }))
                                .child(
                                    div()
                                        .id("conflict-keep")
                                        .px_3()
                                        .py_1()
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .bg(theme::row_hover())
                                        .hover(|this| this.bg(theme::drag_over()))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.conflict_decide(
                                                ConflictDecision::KeepBoth,
                                                cx,
                                            )
                                        }))
                                        .child("Keep both"),
                                )
                                .child(
                                    div()
                                        .id("conflict-skip")
                                        .px_3()
                                        .py_1()
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .bg(theme::row_hover())
                                        .hover(|this| this.bg(theme::drag_over()))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.conflict_decide(ConflictDecision::Skip, cx)
                                        }))
                                        .child("Skip"),
                                ),
                        )
                        .children((total > 1 && dialog.can_replace_all).then(|| {
                            let apply_all = dialog.apply_all;
                            div()
                                .id("conflict-all")
                                .flex()
                                .items_center()
                                .gap_2()
                                .cursor_pointer()
                                .text_color(theme::text_dim())
                                .hover(|this| this.text_color(theme::text()))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(dialog) = &mut this.conflict_dialog {
                                        dialog.apply_all = apply_all;
                                    }
                                    cx.notify();
                                }))
                                .child(if apply_all { "[x]" } else { "[ ]" })
                                .child("apply to all conflicts in this paste")
                        })),
                ),
        )
    }

    fn tab_bar_row(&self, ix: usize, cx: &mut Context<Self>) -> Stateful<Div> {        let label = self.tabs[ix].source.label();
        let active = ix == self.active;
        div()
            .id(format!("tab-{ix}"))
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_1()
            .rounded_t_sm()
            .text_size(px(12.))
            .cursor_pointer()
            .bg(if active {
                theme::row()
            } else {
                theme::clear()
            })
            .text_color(if active {
                theme::text()
            } else {
                theme::text_dim()
            })
            .hover(|this| this.text_color(theme::text()))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.active = ix;
                cx.notify();
            }))
            .on_mouse_down(MouseButton::Middle, cx.listener(move |this, _, _, cx| {
                this.close_tab(ix, cx);
            }))
            .child(label)
            .child(
                div()
                    .id(format!("tab-close-{ix}"))
                    .cursor_pointer()
                    .text_color(theme::text_dim())
                    .hover(|this| this.text_color(theme::error()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_tab(ix, cx);
                        cx.stop_propagation();
                    }))
                    .child("×"),
            )
    }
}

impl Render for Browser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // owned so the icon cells can kick off thumbnail decodes (which
        // need &mut self) while iterating
        let entries: Vec<Entry> = self.tab().entries.iter().take(200).cloned().collect();
        let mut rows: Vec<Stateful<Div>> = Vec::new();
        for (ix, entry) in entries.iter().enumerate() {
            match self.tab().view_mode {
                ViewMode::List => rows.push(self.row(ix, entry, cx)),
                ViewMode::Icons => rows.push(self.icon_cell(ix, entry, cx)),
            }
        }

        let mut places: Vec<Stateful<Div>> = Vec::new();
        for (ix, place) in self.places.iter().enumerate() {
            places.push(self.place_row(ix, place, cx));
        }

        let mut tabs: Vec<Stateful<Div>> = Vec::new();
        for ix in 0..self.tabs.len() {
            tabs.push(self.tab_bar_row(ix, cx));
        }

        let in_trash = self.tab().source == Source::Trash;
        let error_status = self.status.contains("failed") || self.status.contains("cannot");
        let purge_armed = self
            .purge_armed
            .is_some_and(|armed| armed.elapsed() <= PURGE_ARM);

        let status_text = if self.busy {
            self.progress.clone()
        } else if purge_armed {
            "press Delete again to purge permanently (no undo)".into()
        } else {
            self.status.clone()
        };
        let status_color = if error_status {
            theme::error()
        } else if self.busy || purge_armed {
            theme::accent()
        } else {
            theme::text_dim()
        };

        div()
            .size_full()
            .relative()
            .flex()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                this.handle_key(event, cx);
            }))
            .bg(theme::bg())
            .text_color(theme::text())
            .children(self.conflict_overlay(cx))
            .child(
                div()
                    .w(px(170.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_3()
                    .bg(theme::sidebar())
                    .border_r_1()
                    .border_color(theme::border())
                    .children(places)
                    .child(
                        div()
                            .id("place-trash")
                            .px_3()
                            .py_1()
                            .rounded_sm()
                            .text_size(px(13.))
                            .cursor_pointer()
                            .text_color(if in_trash {
                                theme::accent()
                            } else {
                                theme::text_dim()
                            })
                            .bg(if in_trash {
                                theme::row_selected()
                            } else {
                                theme::clear()
                            })
                            .hover(|this| this.bg(theme::row_hover()))
                            .on_click(cx.listener(|this, _, _, cx| this.open_trash(cx)))
                            .child("Trash"),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .px_2()
                            .border_b_1()
                            .border_color(theme::border())
                            .children(tabs)
                            .child(
                                div()
                                    .id("tab-new")
                                    .px_2()
                                    .cursor_pointer()
                                    .text_size(px(13.))
                                    .text_color(theme::text_dim())
                                    .hover(|this| this.text_color(theme::text()))
                                    .on_click(cx.listener(|this, _, _, cx| this.new_tab(cx)))
                                    .child("+"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_3()
                            .py_2()
                            .border_b_1()
                            .border_color(theme::border())
                            .text_size(px(13.))
                            .text_color(theme::text_dim())
                            .child(
                                div()
                                    .id("back")
                                    .cursor_pointer()
                                    .hover(|this| this.text_color(theme::text()))
                                    .on_click(cx.listener(|this, _, _, cx| this.go_back(cx)))
                                    .child(
                                        svg()
                                            .path("icons/arrow_left.svg")
                                            .size(px(16.))
                                            .text_color(theme::text_dim()),
                                    ),
                            )
                            .child(
                                div()
                                    .id("forward")
                                    .cursor_pointer()
                                    .hover(|this| this.text_color(theme::text()))
                                    .on_click(cx.listener(|this, _, _, cx| this.go_forward(cx)))
                                    .child(
                                        svg()
                                            .path("icons/arrow_right.svg")
                                            .size(px(16.))
                                            .text_color(theme::text_dim()),
                                    ),
                            )
                            .child(
                                div()
                                    .id("up")
                                    .cursor_pointer()
                                    .hover(|this| this.text_color(theme::text()))
                                    .on_click(cx.listener(|this, _, _, cx| this.go_up(cx)))
                                    .child(
                                        svg()
                                            .path("icons/arrow_up.svg")
                                            .size(px(16.))
                                            .text_color(theme::text_dim()),
                                    ),
                            )
                            .child(
                                div()
                                    .id("new-folder")
                                    .cursor_pointer()
                                    .hover(|this| this.text_color(theme::text()))
                                    .on_click(cx.listener(|this, _, _, cx| this.new_folder(cx)))
                                    .child(
                                        svg()
                                            .path("icons/folder_add.svg")
                                            .size(px(16.))
                                            .text_color(theme::text_dim()),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .truncate()
                                    .text_color(theme::text())
                                    .child(match self.tab().current_dir() {
                                        Some(dir) => dir.display().to_string(),
                                        None => "Trash".into(),
                                    }),
                            )
                            .child(
                                div()
                                    .id("view-list")
                                    .px_2()
                                    .py_0p5()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .text_size(px(12.))
                                    .text_color(if self.tab().view_mode == ViewMode::List {
                                        theme::accent()
                                    } else {
                                        theme::text_dim()
                                    })
                                    .bg(if self.tab().view_mode == ViewMode::List {
                                        theme::row()
                                    } else {
                                        theme::clear()
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_view_mode(ViewMode::List, cx)
                                    }))
                                    .child("List"),
                            )
                            .child(
                                div()
                                    .id("view-icons")
                                    .px_2()
                                    .py_0p5()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .text_size(px(12.))
                                    .text_color(if self.tab().view_mode == ViewMode::Icons {
                                        theme::accent()
                                    } else {
                                        theme::text_dim()
                                    })
                                    .bg(if self.tab().view_mode == ViewMode::Icons {
                                        theme::row()
                                    } else {
                                        theme::clear()
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_view_mode(ViewMode::Icons, cx)
                                    }))
                                    .child("Icons"),
                            )
                            .child(
                                div()
                                    .id("toggle-hidden")
                                    .cursor_pointer()
                                    .hover(|this| this.text_color(theme::text()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.toggle_hidden(cx)
                                    }))
                                    .child(
                                        svg()
                                            .path(if self.show_hidden {
                                                "icons/eye.svg"
                                            } else {
                                                "icons/eye_off.svg"
                                            })
                                            .size(px(16.))
                                            .text_color(if self.show_hidden {
                                                theme::accent()
                                            } else {
                                                theme::text_dim()
                                            }),
                                    ),
                            ),
                    )
                    .child({
                        // list flows as rows, icons as a wrapping grid
                        let list_box = div()
                            .id("list")
                            .flex_1()
                            .min_h_0()
                            .p_2()
                            .overflow_y_scroll();
                        match self.tab().view_mode {
                            ViewMode::List => list_box
                                .flex()
                                .flex_col()
                                .gap_px()
                                .children(rows),
                            ViewMode::Icons => list_box
                                .flex()
                                .flex_wrap()
                                .content_start()
                                .gap_1()
                                .children(rows),
                        }
                    })
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .gap_4()
                            .px_3()
                            .py_1()
                            .border_t_1()
                            .border_color(theme::border())
                            .text_size(px(12.))
                            .text_color(status_color)
                            .child(format!(
                                "{} items, {} selected",
                                self.tab().entries.len(),
                                self.tab().selection.len()
                            ))
                            .child(div().flex_1().truncate().child(status_text))
                            .child(
                                div()
                                    .text_color(theme::text_dim())
                                    .child("Enter open · F2 rename · Del trash · Ctrl+C/X/V · Ctrl+Z undo · Ctrl+H hidden · Ctrl+1/2 views"),
                            ),
                    ),
            )
    }
}

fn human_size(size: u64) -> String {
    const KIB: f64 = 1024.0;
    let bytes = size as f64;
    if bytes < KIB {
        format!("{size} B")
    } else if bytes < KIB * KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else if bytes < KIB * KIB * KIB {
        format!("{:.1} MiB", bytes / (KIB * KIB))
    } else {
        format!("{:.1} GiB", bytes / (KIB * KIB * KIB))
    }
}

struct Ghost {
    name: String,
    position: Point<Pixels>,
}

impl Render for Ghost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .pl(self.position.x - px(60.))
            .pt(self.position.y - px(16.))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .rounded_sm()
                    .bg(theme::ghost())
                    .text_color(rgb(0xffffff))
                    .text_size(px(13.))
                    .shadow_md()
                    .child(self.name.clone()),
            )
    }
}
