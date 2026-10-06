use std::collections::HashSet;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{collections::HashMap, env, fs, io};

use gpui::{
    AnyElement, App, AppContext, Bounds, ClickEvent, ClipboardItem, Context, Div, DragMoveEvent,
    ExternalDragPayload, ExternalPaths, FileDragPaths, FocusHandle, Focusable, HighlightStyle,
    ImageSource, KeyDownEvent, MouseDownEvent, MouseButton, MouseUpEvent, ObjectFit, Pixels,
    Point, Render, RenderImage, Stateful, StyledText, UnderlineStyle, Window, div, img,
    prelude::*, px, relative, rgba, rgb, svg, FontStyle, FontWeight, SharedString,
};
use trash::{os_limited, TrashItem};
use std::sync::atomic::{AtomicUsize, Ordering};

use notify::Watcher as _;
use std::os::unix::fs::PermissionsExt;

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
    /// mtime as seconds since the epoch, for the details column.
    modified: Option<i64>,
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Name,
    Size,
    Modified,
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
    sort_key: SortKey,
    sort_asc: bool,
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

/// Snapshot for the properties dialog.
/// The right-click menu: where it opened plus a flat item list.
struct ContextMenu {
    x: f32,
    y: f32,
    items: Vec<MenuItem>,
}

#[derive(Clone, Copy)]
struct MenuItem {
    label: &'static str,
    action: MenuAction,
}

/// What a menu item does when clicked. Dispatched through
/// `run_menu_action`, so the menu and the keyboard share handlers.
#[derive(Clone, Copy, PartialEq)]
enum MenuAction {
    Open,
    Rename,
    Copy,
    Cut,
    CopyPath,
    CopyUri,
    Paste,
    Terminal,
    Trash,
    Delete,
    EmptyTrash,
    Info,
    NewFolder,
    NewFile,
    SortName,
    SortSize,
    SortModified,
    ToggleHidden,
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
    /// Info rail on the right, following the cursor entry. On by
    /// default; it only shows while an entry is selected.
    inspector: bool,
    /// Info rail docked to the bottom edge instead of the right.
    inspector_bottom: bool,
    /// Keybinding cheatsheet expanded in the places sidebar.
    keys_open: bool,
    /// Text snippet for the rail, when the focused entry is textual.
    text_preview: Option<TextPreview>,
    preview_key: Option<PathBuf>,
    preview_inflight: HashSet<PathBuf>,
    /// Hand-rolled right-click menu: position plus a flat item list.
    menu: Option<ContextMenu>,
    /// Cells in the first grid row, captured at paint time; 0 means
    /// unknown (list view, or nothing rendered yet).
    grid_row_len: Arc<AtomicUsize>,
    status: String,
    progress: String,
    busy: bool,
    focus: FocusHandle,
    places: Vec<Place>,
    purge_armed: Option<Instant>,
    delete_armed: Option<Instant>,
    /// Armed confirm for the trash tab's Empty Trash button.
    empty_armed: Option<Instant>,
    /// Available bytes on the active tab's volume, fetched in the
    /// background on navigation; keyed by directory.
    free_space: Option<(PathBuf, u64)>,
    free_inflight: Option<PathBuf>,
    show_hidden: bool,
    /// Decoded thumbnails keyed by path; cleared wholesale when large.
    thumbs: HashMap<PathBuf, Arc<RenderImage>>,
    thumbs_inflight: HashSet<PathBuf>,
    path_editing: bool,
    path_buffer: String,
    path_cursor: usize,
    /// Content zoom, 1.0 = normal; clamped to 0.75..=2.0.
    scale: f32,
    titled: Option<PathBuf>,
    watcher: Option<notify::RecommendedWatcher>,
    watched: Option<PathBuf>,
    /// Current-name filter, typed straight into the listing.
    filter: String,
    /// Where the icon-grid rubber band started and now sits, in window
    /// coordinates, plus the grid's own bounds for local conversion.
    rubber_origin: Option<Point<Pixels>>,
    rubber_current: Option<Point<Pixels>>,
    rubber_bounds: Option<Bounds<Pixels>>,
    rubber_ctrl: bool,
    places_refresh: Instant,
}

impl Focusable for Browser {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

const PURGE_ARM: Duration = Duration::from_secs(5);

impl Browser {
    pub(crate) fn new(cli_dir: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let mut browser = Self {
            tabs: vec![Tab::new(Source::Dir(dirs::home_dir().unwrap_or_else(
                || PathBuf::from("."),
            )))],
            active: 0,
            clipboard: None,
            undo: Vec::new(),
            conflict_dialog: None,
            inspector: true,
            inspector_bottom: false,
            keys_open: false,
            text_preview: None,
            preview_key: None,
            preview_inflight: HashSet::new(),
            menu: None,
            grid_row_len: Arc::new(AtomicUsize::new(0)),
            status: String::new(),
            progress: String::new(),
            busy: false,
            focus,
            places: Self::places(),
            purge_armed: None,
            delete_armed: None,
            empty_armed: None,
            free_space: None,
            free_inflight: None,
            show_hidden: false,
            thumbs: HashMap::new(),
            thumbs_inflight: HashSet::new(),
            path_editing: false,
            path_buffer: String::new(),
            path_cursor: 0,
            scale: 1.0,
            titled: None,
            watcher: None,
            watched: None,
            filter: String::new(),
            rubber_origin: None,
            rubber_current: None,
            rubber_bounds: None,
            rubber_ctrl: false,
            places_refresh: Instant::now(),
        };
        browser.load_state(cli_dir.as_deref());
        let show_hidden = browser.show_hidden;
        browser.tab_mut().reload(show_hidden);
        browser.start_dir_watch(cx);
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

        // removable and remote mounts: udisks2 mount points, then the
        // gvfs-FUSE bridges that back samba, MTP, phones, and friends.
        // Mounted volumes appear here without any protocol code on our
        // side (gio mount / the desktop session do that part).
        let mount_root = |root: PathBuf, places: &mut Vec<Place>| {
            let Ok(read) = fs::read_dir(&root) else {
                return;
            };
            let mut mounts: Vec<Place> = read
                .flatten()
                .map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    Place {
                        name,
                        path: entry.path(),
                    }
                })
                .collect();
            mounts.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            places.extend(mounts);
        };
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
            let mut gvfs = PathBuf::from(&runtime);
            gvfs.push("gvfs");
            mount_root(gvfs, &mut places);
        }
        if let Some(user) = std::env::var_os("USER") {
            let mut media = PathBuf::from("/run/media");
            media.push(user);
            mount_root(media, &mut places);
        }
        places
    }

    fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    /// Mounts come and go without us doing anything; re-scan the places
    /// roots at most every couple of seconds.
    fn refresh_places(&mut self) {
        if self.places_refresh.elapsed() >= Duration::from_secs(2) {
            self.places = Self::places();
            self.places_refresh = Instant::now();
        }
    }

    fn zoom_in(&mut self, cx: &mut Context<Self>) {
        self.scale = ((self.scale * 1.25 * 100.).round() / 100.).min(2.0);
        self.save_state();
        self.status = format!("zoom {}%", (self.scale * 100.) as i32);
        cx.notify();
    }

    fn zoom_out(&mut self, cx: &mut Context<Self>) {
        self.scale = ((self.scale / 1.25 * 100.).round() / 100.).max(0.75);
        self.save_state();
        self.status = format!("zoom {}%", (self.scale * 100.) as i32);
        cx.notify();
    }

    fn zoom_reset(&mut self, cx: &mut Context<Self>) {
        self.scale = 1.0;
        self.save_state();
        self.status = "zoom 100%".into();
        cx.notify();
    }

    /// Mouse up over the grid with a rubber band active: every cell
    /// intersecting the band becomes selected (union with the old
    /// selection when Ctrl was held). Cell geometry is pinned, so the
    /// layout math is exact.
    fn finish_rubber(&mut self, cx: &mut Context<Self>) {
        let band = match (self.rubber_origin, self.rubber_current, self.rubber_bounds) {
            (Some(origin), Some(current), Some(bounds)) => Some((origin, current, bounds)),
            _ => None,
        };
        self.rubber_current = None;
        let Some((origin, current, bounds)) = band else {
            log::info!("rubber: drop without full band state");
            cx.notify();
            return;
        };
        log::info!("rubber: finishing band");

        let ctrl = self.rubber_ctrl;
        let scale = self.scale;
        let to_local = |p: Point<Pixels>| {
            (
                f32::from(p.x - bounds.origin.x),
                f32::from(p.y - bounds.origin.y),
            )
        };
        let (ax, ay) = to_local(origin);
        let (bx, by) = to_local(current);
        let (left, right) = if ax < bx { (ax, bx) } else { (bx, ax) };
        let (top, bottom) = if ay < by { (ay, by) } else { (by, ay) };

        let cell_w = 112. * scale;
        let cell_h = 116. * scale;
        let gap = 4. * scale;
        let pad = 8. * scale;
        let per_line = (((f32::from(bounds.size.width) - 2. * pad) / (cell_w + gap)).floor())
            .max(1.) as i32;

        let visible = self.visible_indices();
        let tab = self.tab_mut();
        if !ctrl {
            tab.selection.clear();
        }
        // with a filter on, the grid shows only visible entries and the
        // band selects their grid positions
        for (pos, &entry_ix) in visible.iter().enumerate() {
            let row = (pos as i32 / per_line) as f32;
            let col = (pos as i32 % per_line) as f32;
            let x0 = pad + col * (cell_w + gap);
            let y0 = pad + row * (cell_h + gap);
            if x0 < right && x0 + cell_w > left && y0 < bottom && y0 + cell_h > top {
                tab.selection.insert(tab.entries[entry_ix].key.clone());
            }
        }
        cx.notify();
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
            let is_pdf = icons::is_pdf(&bg_path.file_name().unwrap_or_default().to_string_lossy());
            let render = cx
                .background_spawn(async move {
                    std::panic::catch_unwind(|| {
                        if is_pdf {
                            icons::decode_pdf_thumbnail(&bg_path, 256)
                        } else {
                            icons::decode_thumbnail(&bg_path, 256, 256)
                        }
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
        self.save_state();
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
            self.rubber_origin = None;
            self.rubber_current = None;
            self.save_state();
            cx.notify();
        }
    }

    /// Header click: same key flips direction, a new key sorts ascending.
    fn set_sort(&mut self, key: SortKey, cx: &mut Context<Self>) {
        let show_hidden = self.show_hidden;
        let tab = self.tab_mut();
        if tab.sort_key == key {
            tab.sort_asc = !tab.sort_asc;
        } else {
            tab.sort_key = key;
            tab.sort_asc = true;
        }
        tab.reload(show_hidden);
        self.save_state();
        cx.notify();
    }

    fn open_trash(&mut self, cx: &mut Context<Self>) {
        self.load_source(Source::Trash, cx);
    }

    /// The path bar as clickable crumbs: every ancestor is a jump
    /// target, the current segment opens the path editor. A
    /// home-prefixed path renders from ~ onward.
    fn path_crumbs(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let Some(dir) = self.tab().current_dir().map(Path::to_path_buf) else {
            return div()
                .id("path-bar")
                .flex_1()
                .min_w_0()
                .text_color(theme::text())
                .child("Trash");
        };

        let home = dirs::home_dir();
        let display = match home.as_ref() {
            Some(home) if dir.as_path() == home.as_path() => Some(PathBuf::from("~")),
            Some(home) if dir.starts_with(home) => dir
                .strip_prefix(home)
                .ok()
                .map(|rest| PathBuf::from("~").join(rest)),
            _ => None,
        }
        .unwrap_or_else(|| dir.clone());
        let home_rel = display.starts_with("~");

        let mut row = div()
            .id("path-bar")
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .overflow_hidden()
            .text_color(theme::text());

        let count = display.components().count();
        let mut acc = PathBuf::new();
        for (ix, component) in display.components().enumerate() {
            let last = ix + 1 == count;
            let seg = component.as_os_str().to_string_lossy().into_owned();
            acc.push(component.as_os_str());

            // the absolute location this crumb jumps to
            let target = if home_rel {
                match (home.as_ref(), acc.strip_prefix("~")) {
                    (Some(home), Ok(rest)) => home.join(rest),
                    (Some(home), Err(_)) => home.clone(),
                    _ => acc.clone(),
                }
            } else {
                acc.clone()
            };

            let is_edit = last;
            let mut crumb = div()
                .id(format!("crumb-{ix}"))
                .flex_none()
                .cursor_pointer()
                .text_color(if last {
                    theme::text()
                } else {
                    theme::text_dim()
                })
                .hover(|this| this.text_color(theme::accent()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if is_edit {
                        this.start_path_edit(cx);
                    } else {
                        this.load_source(Source::Dir(target.clone()), cx);
                    }
                }));
            if last {
                crumb = crumb.truncate();
            }
            row = row.child(crumb.child(seg));
            if !last {
                row = row.child(
                    div()
                        .flex_none()
                        .px_1()
                        .text_color(theme::text_dim())
                        .child("/"),
                );
            }
        }
        row
    }

    /// The listing the user sees: all entries, or the nucleo matches
    /// for the type-in filter, best score first.
    fn visible_indices(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.tab().entries.len()).collect();
        }
        use nucleo::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
        use nucleo::{Config, Matcher, Utf32String};

        let pattern = Pattern::new(
            &self.filter,
            CaseMatching::Smart,
            Normalization::Smart,
            AtomKind::Fuzzy,
        );
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut scored: Vec<(u32, usize)> = self
            .tab()
            .entries
            .iter()
            .enumerate()
            .filter_map(|(ix, entry)| {
                let haystack = Utf32String::from(entry.name.as_str());
                pattern
                    .score(haystack.slice(..), &mut matcher)
                    .map(|score| (score, ix))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        scored.into_iter().map(|(_, ix)| ix).collect()
    }

    /// After a filter edit the cursor must sit on a visible entry.
    fn snap_cursor_visible(&mut self) {
        let visible = self.visible_indices();
        let tab = self.tab_mut();
        match tab.cursor {
            Some(ix) if visible.contains(&ix) => {}
            _ => tab.cursor = visible.first().copied(),
        }
    }

    /// Watch the active tab's directory so external changes (other apps,
    /// other windows, mounts) show up without touching anything. One
    /// watcher for the app's lifetime; paths are swapped on navigation.
    /// The raw notify channel is pumped by a plain thread that forwards
    /// coalesced batches over an async channel into the UI.
    fn start_dir_watch(&mut self, cx: &mut Context<Self>) {
        let (tx, rx) = std::sync::mpsc::channel();
        match notify::recommended_watcher(tx) {
            Ok(watcher) => self.watcher = Some(watcher),
            Err(err) => {
                log::error!("notify watcher: {err}");
                return;
            }
        }

        let (batch_tx, mut batch_rx) = futures::channel::mpsc::unbounded();
        std::thread::Builder::new()
            .name("kuma-files-watch".into())
            .spawn(move || loop {
                // the channel carries Result<Event, Error>; notify
                // errors are per-event noise, skip them
                let first = match rx.recv() {
                    Ok(Ok(event)) => event,
                    Ok(Err(_)) => continue,
                    Err(_) => return,
                };
                let mut batch = vec![first];
                // events arrive in bursts (create + metadata + writes);
                // wait out the burst, then deliver once
                loop {
                    match rx.recv_timeout(Duration::from_millis(50)) {
                        Ok(Ok(event)) => batch.push(event),
                        Ok(Err(_)) => continue,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                if batch_tx.unbounded_send(batch).is_err() {
                    return;
                }
            })
            .expect("spawn watch pump");

        cx.spawn(async move |this, cx| {
            use futures::StreamExt;
            while let Some(batch) = batch_rx.next().await {
                let update = this.update(cx, |this, cx| {
                    let current = this.tab().current_dir().map(Path::to_path_buf);
                    let relevant = batch.iter().flat_map(|event| event.paths.iter()).any(|p| {
                        matches!(&p.parent(), Some(parent) if Some(*parent) == current.as_deref())
                    });
                    if relevant {
                        let show_hidden = this.show_hidden;
                        this.tab_mut().reload(show_hidden);
                        log::info!("dir changed externally, reloaded");
                        cx.notify();
                    }
                });
                if update.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// Point the watcher at the active tab's directory. Called from
    /// render, so it stays correct through any navigation path.
    fn arm_watcher(&mut self) {
        let Some(dir) = self.tab().current_dir().map(Path::to_path_buf) else {
            return;
        };
        if self.watched.as_ref() == Some(&dir) {
            return;
        }
        if let Some(old) = self.watched.take()
            && let Some(watcher) = self.watcher.as_mut()
            && let Err(err) = watcher.unwatch(&old)
        {
            log::error!("unwatch {}: {err}", old.display());
        }
        let Some(watcher) = self.watcher.as_mut() else {
            return;
        };
        match watcher.watch(&dir, notify::RecursiveMode::NonRecursive) {
            Ok(()) => {
                self.watched = Some(dir.clone());
                log::info!("watching {}", dir.display());
            }
            Err(err) => log::error!("watch {}: {err}", dir.display()),
        }
    }

    fn start_path_edit(&mut self, cx: &mut Context<Self>) {        let Some(dir) = self.tab().current_dir().map(Path::to_path_buf) else {
            return;
        };
        self.path_editing = true;
        self.path_buffer = dir.display().to_string();
        self.path_cursor = self.path_buffer.len();
        cx.notify();
    }

    fn cancel_path_edit(&mut self, cx: &mut Context<Self>) {
        self.path_editing = false;
        cx.notify();
    }

    /// Enter in the path bar: ~ expands home, existing dirs navigate.
    fn commit_path_edit(&mut self, cx: &mut Context<Self>) {
        self.path_editing = false;
        let mut target = self.path_buffer.trim().to_string();
        if target == "~" {
            target = dirs::home_dir()
                .map(|home| home.display().to_string())
                .unwrap_or(target);
        } else if let Some(rest) = target.strip_prefix("~/").or_else(|| target.strip_prefix("~")) {
            if let Some(home) = dirs::home_dir() {
                target = home.join(rest).display().to_string();
            }
        }
        let path = PathBuf::from(target);
        match fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => self.load_source(Source::Dir(path), cx),
            Ok(_) => {
                self.status = format!("{} is not a folder", path.display());
                cx.notify();
            }
            Err(err) => {
                self.status = format!("no such folder: {err}");
                cx.notify();
            }
        }
    }

    fn load_source(&mut self, source: Source, cx: &mut Context<Self>) {
        let show_hidden = self.show_hidden;
        self.rubber_origin = None;
        self.rubber_current = None;
        self.menu = None;
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
        self.disarm();
        self.save_state();
        cx.notify();
    }

    /// Ctrl+Tab / Ctrl+Shift+Tab: rotate through the tabs.
    fn cycle_tab(&mut self, step: isize, cx: &mut Context<Self>) {
        if self.tabs.len() < 2 {
            return;
        }
        let count = self.tabs.len() as isize;
        let next = (self.active as isize + step).rem_euclid(count);
        self.active = next as usize;
        self.save_state();
        cx.notify();
    }

    /// New tabs start at home but keep the working view: the active
    /// tab's view mode and sort carry over.
    fn new_tab(&mut self, cx: &mut Context<Self>) {        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let mut tab = Tab::new(Source::Dir(home));
        let current = self.tab();
        tab.view_mode = current.view_mode;
        tab.sort_key = current.sort_key;
        tab.sort_asc = current.sort_asc;
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.tabs.last_mut().unwrap().reload(self.show_hidden);
        self.status.clear();
        self.save_state();
        cx.notify();
    }

    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.tabs.len() == 1 {
            // the last tab becomes a fresh home tab rather than closing
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            let mut tab = Tab::new(Source::Dir(home));
            let current = self.tab();
            tab.view_mode = current.view_mode;
            tab.sort_key = current.sort_key;
            tab.sort_asc = current.sort_asc;
            self.tabs[0] = tab;
            self.tabs[0].reload(self.show_hidden);
        } else {
            self.tabs.remove(ix);
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len() - 1;
            } else if ix < self.active {
                self.active -= 1;
            }
        }
        self.save_state();
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
        self.disarm();
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

    /// Copy the selected paths (or file:// URIs) to the system
    /// clipboard, so other apps can use them too.
    fn copy_paths_to_system(&mut self, as_uri: bool, cx: &mut Context<Self>) {
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
        let text = paths
            .iter()
            .map(|path| {
                if as_uri {
                    path_uri(path)
                } else {
                    path.display().to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.status = if as_uri {
            "copied uri(s) to the clipboard"
        } else {
            "copied path(s) to the clipboard"
        }
        .into();
        cx.notify();
    }

    /// Spawn the user's terminal in the current directory. $TERMINAL
    /// wins, then the freedesktop launcher, then the usual suspects.
    /// Each candidate is tried with the terminal's own cwd inheritance,
    /// which every emulator honors.
    fn open_terminal(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.tab().current_dir().map(Path::to_path_buf) else {
            self.status = "no folder to open a terminal in".into();
            cx.notify();
            return;
        };
        let mut candidates: Vec<String> = Vec::new();
        if let Ok(custom) = env::var("TERMINAL") {
            candidates.push(custom);
        }
        candidates.extend([
            "xdg-terminal-exec".into(),
            "kgx".into(),
            "gnome-terminal".into(),
            "konsole".into(),
            "alacritty".into(),
            "foot".into(),
            "kitty".into(),
            "wezterm".into(),
        ]);
        for name in candidates {
            match Command::new(&name).current_dir(&dir).spawn() {
                Ok(mut child) => {
                    // reap from a throwaway thread so the shell never
                    // lingers as a zombie under our pid
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                    self.status = format!("opened {name}");
                    cx.notify();
                    return;
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    self.status = format!("terminal: {err}");
                    cx.notify();
                    return;
                }
            }
        }
        self.status = "no terminal found; set $TERMINAL".into();
        cx.notify();
    }

    /// Empty the trash: purge every item. Armed like row purges:
    /// first click asks, second click within the arm window does it.
    fn empty_trash(&mut self, cx: &mut Context<Self>) {
        if self.tab().source != Source::Trash {
            return;
        }
        let armed = self
            .empty_armed
            .is_some_and(|armed| armed.elapsed() <= PURGE_ARM);
        if !armed {
            self.empty_armed = Some(Instant::now());
            self.status = "click again to empty the trash (no undo)".into();
            cx.notify();
            return;
        }
        self.empty_armed = None;
        match os_limited::list() {
            Ok(items) if !items.is_empty() => self.enqueue(vec![Op::Purge { items }], cx),
            Ok(_) => {
                self.status = "trash is already empty".into();
                cx.notify();
            }
            Err(err) => {
                self.status = format!("trash list failed: {err}");
                cx.notify();
            }
        }
    }

    /// Fetch free bytes for the tab's volume in the background. Safe
    /// to call every render: deduped by directory, refreshed whenever
    /// navigation lands somewhere new.
    fn request_free_space(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.tab().current_dir().map(Path::to_path_buf) else {
            return;
        };
        if self.free_inflight.is_some()
            || self
                .free_space
                .as_ref()
                .is_some_and(|(cached, _)| *cached == dir)
        {
            return;
        }
        self.free_inflight = Some(dir.clone());
        let bg_dir = dir.clone();
        cx.spawn(async move |this, cx| {
            let bytes = cx
                .background_spawn(async move { free_bytes(&bg_dir) })
                .await;
            let update = this.update(cx, |this, cx| {
                this.free_inflight = None;
                if let Some(bytes) = bytes {
                    this.free_space = Some((dir, bytes));
                    cx.notify();
                }
            });
            if let Err(err) = update {
                log::error!("free space update failed: {err:#}");
            }
        })
        .detach();
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

    /// Disarm any armed destructive op when the context moves on.
    fn disarm(&mut self) {
        self.purge_armed = None;
        self.delete_armed = None;
    }

    /// View state across restarts: one tiny key=value file under
    /// ~/.config. Not a settings system, just the knobs you set once
    /// and expect to keep.
    fn state_path() -> Option<PathBuf> {
        let mut path = dirs::config_dir()?;
        path.push("kuma-files/state");
        Some(path)
    }

    fn load_state(&mut self, cli_dir: Option<&Path>) {
        let Some(path) = Self::state_path() else {
            return;
        };
        let Ok(text) = fs::read_to_string(path) else {
            return;
        };

        // parse everything first: the view knobs must apply AFTER the
        // tabs are resolved, not to a placeholder that gets thrown away
        let mut view: Option<ViewMode> = None;
        let mut sort: Option<SortKey> = None;
        let mut sort_asc = true;
        let mut saved_tabs: Vec<(usize, PathBuf)> = Vec::new();
        let mut saved_active: Option<usize> = None;
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match (key, value) {
                ("view", "icons") => view = Some(ViewMode::Icons),
                ("view", "list") => view = Some(ViewMode::List),
                ("sort", "name") => sort = Some(SortKey::Name),
                ("sort", "size") => sort = Some(SortKey::Size),
                ("sort", "modified") => sort = Some(SortKey::Modified),
                ("asc", "true") => sort_asc = true,
                ("asc", "false") => sort_asc = false,
                ("hidden", "true") => self.show_hidden = true,
                ("inspector", "true") => self.inspector = true,
                ("inspector", "false") => self.inspector = false,
                ("inspector-bottom", "true") => self.inspector_bottom = true,
                ("inspector-bottom", "false") => self.inspector_bottom = false,
                ("keys", "true") => self.keys_open = true,
                ("keys", "false") => self.keys_open = false,
                ("active", _) => saved_active = value.parse().ok(),
                ("scale", _) => {
                    if let Ok(parsed) = value.parse::<f32>() {
                        self.scale = parsed.clamp(0.75, 2.0);
                    }
                }
                _ => {
                    if let Some(index) = key.strip_prefix("tab").and_then(|n| n.parse().ok())
                        && !value.is_empty()
                    {
                        saved_tabs.push((index, PathBuf::from(value)));
                    }
                }
            }
        }

        // an explicit CLI dir always wins; otherwise reopen the folders
        // that were open last time (trash tabs are not persisted)
        saved_tabs.sort_by_key(|(index, _)| *index);
        if cli_dir.is_none() && !saved_tabs.is_empty() {
            self.tabs = saved_tabs
                .into_iter()
                .map(|(_, dir)| Tab::new(Source::Dir(dir)))
                .collect();
            if let Some(active) = saved_active {
                self.active = active.min(self.tabs.len() - 1);
            }
        } else if let Some(dir) = cli_dir {
            self.tab_mut().source = Source::Dir(dir.to_path_buf());
        }

        // the knobs belong to every tab we ended up with
        for tab in &mut self.tabs {
            if let Some(view_mode) = view {
                tab.view_mode = view_mode;
            }
            if let Some(sort_key) = sort {
                tab.sort_key = sort_key;
            }
            tab.sort_asc = sort_asc;
        }
    }

    fn save_state(&self) {
        let Some(path) = Self::state_path() else {
            return;
        };
        if let Some(dir) = path.parent()
            && let Err(err) = fs::create_dir_all(dir)
        {
            log::error!("state dir: {err}");
            return;
        }
        let tab = self.tab();
        let sort = match tab.sort_key {
            SortKey::Name => "name",
            SortKey::Size => "size",
            SortKey::Modified => "modified",
        };
        let text = format!(
            "view={}\nsort={}\nasc={}\nhidden={}\ninspector={}\ninspector-bottom={}\nkeys={}\nscale={}\n",
            if tab.view_mode == ViewMode::Icons {
                "icons"
            } else {
                "list"
            },
            sort,
            tab.sort_asc,
            self.show_hidden,
            self.inspector,
            self.inspector_bottom,
            self.keys_open,
            self.scale,
        );
        let mut text = text;
        for (i, tab) in self.tabs.iter().enumerate() {
            if let Some(dir) = tab.current_dir() {
                text.push_str(&format!("tab{}={}\n", i, dir.display()));
            }
        }
        text.push_str(&format!("active={}\n", self.active));
        if let Err(err) = fs::write(path, text) {
            log::error!("save state: {err}");
        }
    }

    /// Shift+Delete in a directory view: skip the trash entirely. Same
    /// arm-then-confirm pattern as purge, because it is just as final.
    fn delete_selection(&mut self, cx: &mut Context<Self>) {
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
        match self.delete_armed {
            Some(armed) if armed.elapsed() <= PURGE_ARM => {
                self.delete_armed = None;
                cx.spawn(async move |this, cx| {
                    let failure = cx
                        .background_spawn(async move {
                            for path in &paths {
                                let outcome = if path.is_dir() {
                                    fs::remove_dir_all(path)
                                } else {
                                    fs::remove_file(path)
                                };
                                if let Err(err) = outcome {
                                    return Some(format!("{}: {err}", path.display()));
                                }
                            }
                            None
                        })
                        .await;
                    let update = this.update(cx, |this, cx| {
                        match failure {
                            Some(err) => {
                                log::error!("delete: {err}");
                                this.status = format!("delete failed: {err}");
                            }
                            None => this.status = "deleted permanently (no undo)".into(),
                        }
                        let show_hidden = this.show_hidden;
                        this.tab_mut().reload(show_hidden);
                        cx.notify();
                    });
                    if let Err(err) = update {
                        log::error!("delete update failed: {err:#}");
                    }
                })
                .detach();
            }
            _ => {
                self.delete_armed = Some(Instant::now());
                self.status = "press Shift+Delete again to delete permanently (no undo)".into();
                cx.notify();
            }
        }
    }

    /// New empty file next to New Folder, same collision-free naming.
    fn new_file(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.tab().current_dir().map(Path::to_path_buf) else {
            return;
        };
        let mut name = "New File".to_string();
        let mut n = 2;
        while dir.join(&name).exists() {
            name = format!("New File {n}");
            n += 1;
        }
        match fs::File::create(dir.join(&name)) {
            Ok(_) => {
                self.status = format!("created {name}");
                let show_hidden = self.show_hidden;
                let tab = self.tab_mut();
                tab.reload(show_hidden);
                let new_path = dir.join(&name);
                if let Some(ix) = tab.entries.iter().position(|e| e.path == new_path) {
                    tab.cursor = Some(ix);
                    tab.selection.clear();
                    tab.selection.insert(new_path);
                }
            }
            Err(err) => {
                log::error!("create file: {err}");
                self.status = format!("new file failed: {err}");
            }
        }
        cx.notify();
    }

    /// Move the info panel between the right edge and the bottom edge.
    fn flip_inspector(&mut self, cx: &mut Context<Self>) {
        self.inspector_bottom = !self.inspector_bottom;
        self.save_state();
        cx.notify();
    }

    /// Show or hide the keybinding cheatsheet in the sidebar.
    fn toggle_keys(&mut self, cx: &mut Context<Self>) {
        self.keys_open = !self.keys_open;
        self.save_state();
        cx.notify();
    }

    /// The collapsible keybinding cheatsheet, pinned to the bottom of
    /// the places sidebar.
    fn keys_section(&self, cx: &mut Context<Self>) -> Div {
        let mut section = div()
            .flex()
            .flex_col()
            .gap_px()
            .child(
                div()
                    .id("keys-toggle")
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .text_size(px(13.))
                    .text_color(theme::text_dim())
                    .hover(|this| this.bg(theme::row_hover()))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_keys(cx)))
                    .child("Keys")
                    .child(
                        svg()
                            .path(if self.keys_open {
                                "icons/chevron_up.svg"
                            } else {
                                "icons/chevron_down.svg"
                            })
                            .size(px(12.))
                            .text_color(theme::text_dim()),
                    ),
            );

        if self.keys_open {
            for (key, action) in KEY_HINTS {
                section = section.child(
                    div()
                        .flex()
                        .justify_between()
                        .px_3()
                        .pl_4()
                        .py_px()
                        .text_size(px(12.))
                        .child(div().text_color(theme::text()).child(*key))
                        .child(div().text_color(theme::text_dim()).child(*action)),
                );
            }
        }
        section
    }

    /// Alt+Enter: show or hide the info rail.
    fn toggle_inspector(&mut self, cx: &mut Context<Self>) {        self.inspector = !self.inspector;
        if !self.inspector {
            self.preview_key = None;
            self.text_preview = None;
        }
        self.save_state();
        cx.notify();
    }

    /// Load the rail's text snippet in the background: first lines of a
    /// small file that does not smell binary (no NUL in the head). The
    /// kind comes from the extension; styling happens at render.
    fn request_text_preview(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.preview_inflight.contains(&path) {
            return;
        }
        let Ok(meta) = fs::symlink_metadata(&path) else {
            return;
        };
        if meta.len() > 1024 * 1024 {
            return;
        }
        self.preview_inflight.insert(path.clone());
        let kind = text_kind(&path);
        let bg_path = path.clone();
        cx.spawn(async move |this, cx| {
            let lines = cx
                .background_spawn(async move {
                    let mut file = fs::File::open(&bg_path).ok()?;
                    let mut buf = vec![0u8; 64 * 1024];
                    let mut filled = 0;
                    while filled < buf.len() {
                        let n = io::Read::read(&mut file, &mut buf[filled..]).ok()?;
                        if n == 0 {
                            break;
                        }
                        filled += n;
                    }
                    if buf[..filled].contains(&0) {
                        return None;
                    }
                    let text = String::from_utf8_lossy(&buf[..filled]);
                    Some(
                        text.lines()
                            .take(48)
                            .map(str::to_string)
                            .collect::<Vec<_>>(),
                    )
                })
                .await;
            let update = this.update(cx, |this, cx| {
                this.preview_inflight.remove(&path);
                if this.preview_key.as_ref() == Some(&path) {
                    this.text_preview = lines.clone().map(|loaded| {
                        let blocks = if kind == TextKind::Markdown {
                            Some(parse_markdown(&loaded.join("\n")))
                        } else {
                            None
                        };
                        TextPreview {
                            kind,
                            lines: loaded,
                            blocks,
                        }
                    });
                    cx.notify();
                }
            });
            if let Err(err) = update {
                log::error!("preview update failed: {err:#}");
            }
        })
        .detach();
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
                self.disarm();
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
            sort_key: SortKey::Name,
            sort_asc: true,
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
                        let meta = fs::symlink_metadata(&path).ok();
                        let size = meta.as_ref().and_then(|meta| {
                            if meta.is_dir() {
                                None
                            } else {
                                Some(meta.len())
                            }
                        });
                        let modified = meta
                            .and_then(|meta| meta.modified().ok())
                            .and_then(|time| {
                                time.duration_since(std::time::UNIX_EPOCH)
                                    .ok()
                                    .map(|d| d.as_secs() as i64)
                            });
                        entries.push(Entry {
                            key: path.clone(),
                            path,
                            name,
                            is_dir,
                            size,
                            modified,
                            item: None,
                        });
                    }
                    sort_entries(&mut entries, self.sort_key, self.sort_asc);
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
                        // the Modified column shows when the item was
                        // trashed
                        modified: Some(item.time_deleted),
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
        let filtering = !self.filter.is_empty();

        if self.conflict_dialog.is_some() {
            match keystroke.key.as_str() {
                "escape" => self.conflict_skip_all(cx),
                _ => {}
            }
            return;
        }

        if self.menu.is_some() {
            if keystroke.key == "escape" {
                self.menu = None;
                cx.notify();
            }
            return;
        }

        if self.path_editing {
            match keystroke.key.as_str() {
                "enter" => self.commit_path_edit(cx),
                "escape" => self.cancel_path_edit(cx),
                "backspace" => {
                    if self.path_cursor > 0 {
                        let head = &self.path_buffer[..self.path_cursor];
                        if let Some((prev, _)) = head.char_indices().next_back() {
                            self.path_buffer.remove(prev);
                            self.path_cursor = prev;
                        }
                    }
                    cx.notify();
                }
                "left" => {
                    if self.path_cursor > 0 {
                        let head = &self.path_buffer[..self.path_cursor];
                        if let Some((prev, _)) = head.char_indices().next_back() {
                            self.path_cursor = prev;
                        }
                    }
                    cx.notify();
                }
                "right" => {
                    if self.path_buffer.is_char_boundary(self.path_cursor)
                        && self.path_cursor < self.path_buffer.len()
                    {
                        let tail = &self.path_buffer[self.path_cursor..];
                        if let Some(ch) = tail.chars().next() {
                            self.path_cursor += ch.len_utf8();
                        }
                    }
                    cx.notify();
                }
                "home" => {
                    self.path_cursor = 0;
                    cx.notify();
                }
                "end" => {
                    self.path_cursor = self.path_buffer.len();
                    cx.notify();
                }
                _ if !keystroke.modifiers.control
                    && !keystroke.modifiers.alt
                    && !keystroke.modifiers.platform
                    && !keystroke.modifiers.function =>
                {
                    if let Some(character) = keystroke.key_char.as_deref() {
                        let cursor = if self.path_buffer.is_char_boundary(self.path_cursor) {
                            self.path_cursor
                        } else {
                            self.path_buffer.len()
                        };
                        self.path_buffer.insert_str(cursor, character);
                        self.path_cursor = cursor + character.len();
                        cx.notify();
                    }
                }
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
            "enter" if keystroke.modifiers.alt => self.toggle_inspector(cx),
            "enter" => self.open_selection(cx),
            // Backspace edits the filter while one is active, otherwise
            // it goes to the parent directory
            "backspace" => {
                if filtering {
                    self.filter.pop();
                    self.snap_cursor_visible();
                    cx.notify();
                } else {
                    self.go_up(cx);
                }
            }
            "escape" => {
                if filtering {
                    self.filter.clear();
                    self.snap_cursor_visible();
                    cx.notify();
                } else {
                    tab.selection.clear();
                    tab.cursor = None;
                    self.disarm();
                    cx.notify();
                }
            }
            "delete" if keystroke.modifiers.shift => self.delete_selection(cx),
            "delete" => self.trash_selection(cx),
            "f2" => self.start_rename(cx),
            // path bar: type a location instead of clicking crumbs
            "l" if keystroke.modifiers.control => self.start_path_edit(cx),
            "f6" => self.start_path_edit(cx),
            // tab cycling, browser style
            "tab" if keystroke.modifiers.control && keystroke.modifiers.shift => {
                self.cycle_tab(-1, cx)
            }
            "tab" if keystroke.modifiers.control => self.cycle_tab(1, cx),
            "home" if keystroke.modifiers.alt => {
                let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
                self.load_source(Source::Dir(home), cx);
            }
            // manual refresh; the watcher usually beats you to it
            "f5" => {
                let show_hidden = self.show_hidden;
                self.tab_mut().reload(show_hidden);
                self.status = "refreshed".into();
                cx.notify();
            }

            "left" if keystroke.modifiers.alt => self.go_back(cx),
            "right" if keystroke.modifiers.alt => self.go_forward(cx),
            "up" if keystroke.modifiers.alt => self.go_up(cx),
            "left" if keystroke.modifiers.control => self.go_back(cx),
            "right" if keystroke.modifiers.control => self.go_forward(cx),
            "up" if keystroke.modifiers.control => self.go_up(cx),
            "down" => {
                let per_line = self.grid_per_line();
                if per_line > 1 {
                    self.move_cursor_grid(0, 1, per_line, cx)
                } else {
                    self.move_cursor(1, cx)
                }
            }
            "up" => {
                let per_line = self.grid_per_line();
                if per_line > 1 {
                    self.move_cursor_grid(0, -1, per_line, cx)
                } else {
                    self.move_cursor(-1, cx)
                }
            }
            "left" => {
                let per_line = self.grid_per_line();
                if per_line > 1 {
                    self.move_cursor_grid(-1, 0, per_line, cx)
                }
            }
            "right" => {
                let per_line = self.grid_per_line();
                if per_line > 1 {
                    self.move_cursor_grid(1, 0, per_line, cx)
                }
            }
            "home" => {
                let first = self.visible_indices().first().copied();
                if let Some(entry_ix) = first {
                    self.jump_cursor(entry_ix, cx);
                }
            }
            "end" => {
                let last = self.visible_indices().into_iter().next_back();
                if let Some(entry_ix) = last {
                    self.jump_cursor(entry_ix, cx);
                }
            },
            "a" if keystroke.modifiers.control => {
                tab.selection = tab.entries.iter().map(|e| e.key.clone()).collect();
                cx.notify();
            }
            "c" if keystroke.modifiers.control && !keystroke.modifiers.shift => {
                self.copy_selection(cx)
            }
            "c" if keystroke.modifiers.control && keystroke.modifiers.shift => {
                self.copy_paths_to_system(false, cx)
            }
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
            "=" | "+" if keystroke.modifiers.control => self.zoom_in(cx),
            "-" | "_" if keystroke.modifiers.control => self.zoom_out(cx),
            "0" if keystroke.modifiers.control => self.zoom_reset(cx),
            "n" if keystroke.modifiers.control && keystroke.modifiers.shift => {
                self.new_folder(cx)
            }
            "n" if keystroke.modifiers.control && keystroke.modifiers.alt => {
                self.new_file(cx)
            }
            // bare single characters feed the type-in filter; shift is
            // allowed (uppercase), everything else filters it out
            _ if !keystroke.modifiers.control
                && !keystroke.modifiers.alt
                && !keystroke.modifiers.platform
                && !keystroke.modifiers.function
                && keystroke.key.chars().count() == 1
                && !keystroke.key.chars().all(|ch| ch.is_control()) =>
            {
                self.filter.push_str(&keystroke.key);
                self.snap_cursor_visible();
                cx.notify();
            }
            _ => {}
        }
    }

    fn move_cursor(&mut self, step: isize, cx: &mut Context<Self>) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        let current_pos = self
            .tab()
            .cursor
            .and_then(|ix| visible.iter().position(|&v| v == ix));
        let next_pos = match current_pos {
            Some(pos) => (pos as isize + step).clamp(0, visible.len() as isize - 1) as usize,
            None => 0,
        };
        self.focus_visible(visible, next_pos, cx);
    }

    /// Cells per grid row from the last paint; 0 when unknown.
    fn grid_per_line(&self) -> usize {
        if self.tab().view_mode != ViewMode::Icons {
            return 0;
        }
        let count = self.grid_row_len.load(Ordering::Relaxed);
        if count > 1 {
            count
        } else {
            0
        }
    }

    /// Grid navigation for icon view. Horizontal steps stay inside the
    /// row; vertical steps jump a full row and clamp at the ends.
    fn move_cursor_grid(
        &mut self,
        d_col: isize,
        d_row: isize,
        per_line: usize,
        cx: &mut Context<Self>,
    ) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        let next_pos = match self
            .tab()
            .cursor
            .and_then(|ix| visible.iter().position(|&v| v == ix))
        {
            Some(pos) => {
                if d_row == 0 {
                    let col = (pos % per_line) as isize + d_col;
                    if col < 0 || col as usize >= per_line {
                        return;
                    }
                    (pos / per_line) * per_line + col as usize
                } else {
                    (pos as isize + d_row * per_line as isize)
                        .clamp(0, visible.len() as isize - 1) as usize
                }
            }
            None => 0,
        };
        self.focus_visible(visible, next_pos, cx);
    }

    /// Put the cursor on `visible[target]`, selecting just that entry.
    fn focus_visible(&mut self, visible: Vec<usize>, target: usize, cx: &mut Context<Self>) {
        let entry_ix = visible[target];
        let tab = self.tab_mut();
        tab.cursor = Some(entry_ix);
        let key = tab.entries[entry_ix].key.clone();
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
        self.save_state();
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
        self.save_state();
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
        let s = self.scale;
        let selected = self.tab().selection.contains(&entry.key);
        let tab = self.tab();
        let renaming = tab.renaming.as_ref().is_some_and(|path| *path == entry.path);
        let in_trash = entry.item.is_some();
        let entry_key = entry.key.clone();
        let entry_path = entry.path.clone();
        let size_text = entry.size.map(human_size).unwrap_or_default();
        let menu_key = entry_key.clone();

        let mut base = div()
            .id(ix)
            .flex()
            .items_center()
            .justify_between()
            .px_3()
            .py_1()
            .rounded_sm()
            .text_size(px(14. * s))
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
                this.disarm();
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    // Explorer convention: right-click selects the row
                    // unless it is already part of the selection
                    {
                        let tab = this.tab_mut();
                        if !tab.selection.contains(&menu_key) {
                            tab.selection.clear();
                            tab.selection.insert(menu_key.clone());
                        }
                        tab.cursor = Some(ix);
                    }
                    let items = if in_trash {
                        Self::trash_menu_items()
                    } else {
                        Self::row_menu_items()
                    };
                    this.open_menu(
                        f32::from(event.position.x),
                        f32::from(event.position.y),
                        items,
                    );
                    cx.notify();
                }),
            );

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
                .min_w_0()
                .border_1()
                .border_color(theme::accent())
                .rounded_sm()
                .px_1()
                .child(format!("{before}▏{after}"))
        } else {
            div().flex_1().min_w_0().truncate().child(entry.name.clone())
        };

        let modified_text = entry
            .modified
            .map(|secs| relative_time(secs, now_secs()))
            .unwrap_or_default();

        base.child(self.entry_icon(entry, px(16. * s)))
            .child(name_child)
            .child(
                div()
                    .w(px(72. * s))
                    .flex_none()
                    .text_size(px(12. * s))
                    .text_color(theme::text_dim())
                    .text_right()
                    .child(size_text),
            )
            .child(
                div()
                    .w(px(110. * s))
                    .flex_none()
                    .text_size(px(12. * s))
                    .text_color(theme::text_dim())
                    .text_right()
                    .truncate()
                    .child(modified_text),
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
        // pinned, scaled geometry: 8 pad + 80 preview + 4 gap + 16 name
        // + 8 pad = 116; the rubber band's hit math depends on this
        let s = self.scale;
        let selected = self.tab().selection.contains(&entry.key);
        let entry_key = entry.key.clone();
        let entry_path = entry.path.clone();
        let menu_key = entry_key.clone();
        let in_trash = entry.item.is_some();

        // thumbnails only for local image files: trash entries point at
        // paths that no longer exist
        let show_thumb = !in_trash && !entry.is_dir && icons::is_thumbable(&entry.name);
        let thumb = if show_thumb {
            self.request_thumb(entry.path.clone(), cx);
            self.thumbs.get(&entry.path).cloned()
        } else {
            None
        };

        let preview: AnyElement = match thumb {
            Some(render) => img(ImageSource::Render(render))
                .size(px(76. * s))
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
                    .size(px(if entry.is_dir { 56. * s } else { 44. * s }))
                    .text_color(color)
                    .into_any_element()
            }
        };

        div()
            .id(ix)
            .w(px(112. * s))
            .h(px(116. * s))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(4. * s))
            .p(px(8. * s))
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
                this.disarm();
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    // Explorer convention: right-click selects the cell
                    // unless it is already part of the selection
                    {
                        let tab = this.tab_mut();
                        if !tab.selection.contains(&menu_key) {
                            tab.selection.clear();
                            tab.selection.insert(menu_key.clone());
                        }
                        tab.cursor = Some(ix);
                    }
                    let items = if in_trash {
                        Self::trash_menu_items()
                    } else {
                        Self::row_menu_items()
                    };
                    this.open_menu(
                        f32::from(event.position.x),
                        f32::from(event.position.y),
                        items,
                    );
                    cx.notify();
                }),
            )
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
                    .h(px(80. * s))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(preview),
            )
            .child(
                div()
                    .h(px(16. * s))
                    .w_full()
                    .text_size(px(12. * s))
                    .line_height(relative(1.3))
                    .overflow_hidden()
                    .text_center()
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
            .flex()
            .items_center()
            .gap_2()
            .child(
                svg()
                    .path("icons/folder.svg")
                    .size(px(14.))
                    .flex_none()
                    .text_color(if here {
                        theme::accent()
                    } else {
                        theme::text_dim()
                    }),
            )
            .child(place.name.clone())
    }

    /// The details header for list view: click a column to sort, click
    /// again to flip. Directories stay first in every sort.
    fn sort_header(&self, cx: &mut Context<Self>) -> Div {
        let s = self.scale;
        let tab = self.tab();
        let arrow = |key: SortKey| {
            if tab.sort_key != key {
                String::new()
            } else if tab.sort_asc {
                " ▲".into()
            } else {
                " ▼".into()
            }
        };
        div()
            .flex()
            .items_center()
            .px_3()
            .py_1()
            .text_size(px(12. * s))
            .text_color(theme::text_dim())
            .child(
                div()
                    .id("sort-name")
                    .flex_1()
                    .cursor_pointer()
                    .hover(|this| this.text_color(theme::text()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_sort(SortKey::Name, cx);
                    }))
                    .child(format!("Name{}", arrow(SortKey::Name))),
            )
            .child(
                div()
                    .id("sort-size")
                    .w(px(72. * s))
                    .flex_none()
                    .text_right()
                    .cursor_pointer()
                    .hover(|this| this.text_color(theme::text()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_sort(SortKey::Size, cx);
                    }))
                    .child(format!("Size{}", arrow(SortKey::Size))),
            )
            .child(
                div()
                    .id("sort-modified")
                    .w(px(110. * s))
                    .flex_none()
                    .text_right()
                    .cursor_pointer()
                    .hover(|this| this.text_color(theme::text()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_sort(SortKey::Modified, cx);
                    }))
                    .child(format!("Modified{}", arrow(SortKey::Modified))),
            )
    }

    /// The modal overlay for unresolved paste conflicts. `None` (rendered
    /// as no child) when no paste is waiting on a decision.
    /// The info panel: preview plus metadata, following the cursor
    /// entry. Nothing is snapshotted; metadata is read fresh each
    /// render (one stat syscall while open). Docked right by default,
    /// or along the bottom edge when the user prefers landscape room.
    fn inspector_panel(&self, bottom: bool, cx: &mut Context<Self>) -> Div {
        let entry = self
            .tab()
            .cursor
            .and_then(|ix| self.tab().entries.get(ix));
        let meta = entry.and_then(|e| fs::symlink_metadata(&e.path).ok());
        let mode = meta.as_ref().map(|m| m.permissions().mode());
        let modified = meta.as_ref().and_then(|m| {
            m.modified().ok().and_then(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_secs() as i64)
            })
        });
        let is_dir = entry.is_some_and(|e| e.is_dir);
        let size = entry.and_then(|e| {
            if e.is_dir {
                None
            } else {
                meta.as_ref().map(|m| m.len())
            }
        });

        // preview: image thumb, text snippet, or the type icon
        let preview: AnyElement = if let Some(render) =
            entry.and_then(|e| self.thumbs.get(&e.path).cloned())
        {
            img(ImageSource::Render(render))
                .size_full()
                .object_fit(ObjectFit::Contain)
                .into_any_element()
        } else if let (Some(entry), Some(text)) = (&entry, &self.text_preview)
            && self.preview_key.as_ref() == Some(&entry.key)
        {
            let mono = div()
                .w_full()
                .h_full()
                .flex()
                .flex_col()
                .gap_px()
                .overflow_hidden()
                .text_size(px(11.))
                .text_color(theme::text())
                .font_family("monospace");
            match text.kind {
                TextKind::Csv => mono.children(csv_lines(&text.lines).iter().map(|line| {
                    preview_line(line.clone()).text_color(theme::text())
                })),
                TextKind::Code => mono.children(
                    text.lines
                        .iter()
                        .map(|line| code_line(line).into_any_element()),
                ),
                TextKind::Markdown => div()
                    .w_full()
                    .h_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .overflow_hidden()
                    .text_size(px(11.))
                    .text_color(theme::text())
                    .children(md_blocks(text)),
                TextKind::Plain => mono.children(
                    text.lines
                        .iter()
                        .map(|line| preview_line(line.clone()).text_color(theme::text())),
                ),
            }
            .into_any_element()
        } else if let Some(entry) = entry {
            self.entry_icon(entry, px(56.))
        } else {
            div().into_any_element()
        };

        let preview_box = div()
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .bg(theme::row())
            .border_1()
            .border_color(theme::border())
            .p_2()
            .overflow_hidden()
            .child(preview);

        let header = div()
            .flex()
            .items_center()
            .justify_end()
            .gap_1()
            .child(
                // dock the panel on the other edge
                div()
                    .id("inspector-dock")
                    .flex()
                    .h(px(20.))
                    .items_center()
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .text_color(theme::text_dim())
                    .hover(|this| this.text_color(theme::text()).bg(theme::row_hover()))
                    .on_click(cx.listener(|this, _, _, cx| this.flip_inspector(cx)))
                    .child(
                        svg()
                            .path(if bottom {
                                "icons/chevron_right.svg"
                            } else {
                                "icons/chevron_down.svg"
                            })
                            .size(px(14.))
                            .text_color(theme::text_dim()),
                    ),
            )
            .child(
                div()
                    .id("inspector-close")
                    .flex()
                    .h(px(20.))
                    .items_center()
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .text_color(theme::text_dim())
                    .hover(|this| this.text_color(theme::text()).bg(theme::row_hover()))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_inspector(cx)))
                    .text_size(px(14.))
                    .child("×"),
            );

        let rows = div()
            .flex()
            .flex_col()
            .gap_2()
            .min_w_0()
            .children(entry.map(|e| prop_row("name", e.name.clone())))
            .children(entry.map(|e| prop_row("kind", friendly_kind(&e.name, e.is_dir))))
            .children(entry.map(|e| prop_row("path", e.path.display().to_string())))
            .children(entry.map(|_| {
                prop_row(
                    "size",
                    size.map(human_size)
                        .unwrap_or_else(|| if is_dir { "folder".into() } else { "0 B".into() }),
                )
            }))
            .children(entry.map(|_| {
                prop_row(
                    "modified",
                    modified
                        .map(|secs| relative_time(secs, now_secs()))
                        .unwrap_or_default(),
                )
            }))
            .children(entry.map(|_| prop_row("permissions", mode.map(mode_string).unwrap_or_default())));

        if bottom {
            div()
                .w_full()
                .flex_none()
                .h(px(220.))
                .min_h_0()
                .flex()
                .gap_3()
                .p_3()
                .overflow_hidden()
                .text_size(px(13.))
                .bg(theme::sidebar())
                .border_t_1()
                .border_color(theme::border())
                .child(preview_box.w(px(280.)).h_full())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .overflow_hidden()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(header)
                        .child(rows),
                )
        } else {
            div()
                .w(px(280.))
                .flex_none()
                .h_full()
                .flex()
                .flex_col()
                .gap_3()
                .p_3()
                .overflow_hidden()
                .text_size(px(13.))
                .bg(theme::sidebar())
                .border_l_1()
                .border_color(theme::border())
                .child(header)
                .child(preview_box.w_full().h(px(200.)))
                .child(rows)
        }
    }

    /// The right-click menu: a backdrop that eats the next click (and
    /// closes) plus the panel itself, siblings so the backdrop never
    /// swallows an item's click through parent-first bubbling.
    fn menu_overlay(&self, window: &Window, cx: &mut Context<Self>) -> Option<Div> {
        let menu = self.menu.as_ref()?;
        let viewport = window.viewport_size();
        // keep the panel inside the window: rough height math, 26px a row
        let max_x = f32::from(viewport.width) - 200.;
        let max_y = f32::from(viewport.height) - (menu.items.len() as f32 * 26. + 8.);
        let x = menu.x.min(max_x.max(0.));
        let y = menu.y.min(max_y.max(0.));

        let close_left = cx.listener(|this, _: &MouseDownEvent, _, cx| {
            this.menu = None;
            cx.notify();
        });
        let close_right = cx.listener(|this, _: &MouseDownEvent, _, cx| {
            this.menu = None;
            cx.notify();
        });
        let backdrop = div()
            .absolute()
            .inset_0()
            .on_mouse_down(MouseButton::Left, close_left)
            .on_mouse_down(MouseButton::Right, close_right);

        let mut panel = div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .min_w(px(180.))
            .flex()
            .flex_col()
            .py_1()
            .rounded_md()
            .bg(theme::sidebar())
            .border_1()
            .border_color(theme::border())
            .shadow_lg()
            .text_size(px(13.));
        for (i, item) in menu.items.iter().enumerate() {
            let action = item.action;
            panel = panel.child(
                div()
                    .id(i)
                    .px_3()
                    .py_1()
                    .cursor_pointer()
                    .text_color(theme::text())
                    .hover(|this| this.bg(theme::row_hover()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.run_menu_action(action, cx)
                    }))
                    .child(item.label),
            );
        }

        Some(div().absolute().inset_0().child(backdrop).child(panel))
    }

    fn run_menu_action(&mut self, action: MenuAction, cx: &mut Context<Self>) {
        self.menu = None;
        match action {
            MenuAction::Open => self.open_selection(cx),
            MenuAction::Rename => self.start_rename(cx),
            MenuAction::Copy => self.copy_selection(cx),
            MenuAction::Cut => self.cut_selection(cx),
            MenuAction::CopyPath => self.copy_paths_to_system(false, cx),
            MenuAction::CopyUri => self.copy_paths_to_system(true, cx),
            MenuAction::Paste => self.paste(cx),
            MenuAction::Terminal => self.open_terminal(cx),
            MenuAction::Trash => self.trash_selection(cx),
            MenuAction::Delete => self.delete_selection(cx),
            MenuAction::EmptyTrash => self.empty_trash(cx),
            MenuAction::Info => self.toggle_inspector(cx),
            MenuAction::NewFolder => self.new_folder(cx),
            MenuAction::NewFile => self.new_file(cx),
            MenuAction::SortName => self.set_sort(SortKey::Name, cx),
            MenuAction::SortSize => self.set_sort(SortKey::Size, cx),
            MenuAction::SortModified => self.set_sort(SortKey::Modified, cx),
            MenuAction::ToggleHidden => self.toggle_hidden(cx),
        }
    }

    fn open_menu(&mut self, x: f32, y: f32, items: Vec<MenuItem>) {
        self.menu = Some(ContextMenu { x, y, items });
    }

    fn row_menu_items() -> Vec<MenuItem> {
        vec![
            MenuItem { label: "Open", action: MenuAction::Open },
            MenuItem { label: "Rename", action: MenuAction::Rename },
            MenuItem { label: "Copy", action: MenuAction::Copy },
            MenuItem { label: "Cut", action: MenuAction::Cut },
            MenuItem { label: "Copy Path", action: MenuAction::CopyPath },
            MenuItem { label: "Copy URI", action: MenuAction::CopyUri },
            MenuItem { label: "Trash", action: MenuAction::Trash },
            MenuItem { label: "Delete permanently", action: MenuAction::Delete },
            MenuItem { label: "Properties", action: MenuAction::Info },
        ]
    }

    fn trash_menu_items() -> Vec<MenuItem> {
        vec![
            MenuItem { label: "Restore", action: MenuAction::Open },
            MenuItem { label: "Delete permanently", action: MenuAction::Delete },
            MenuItem { label: "Properties", action: MenuAction::Info },
        ]
    }

    fn empty_menu_items(in_trash: bool) -> Vec<MenuItem> {
        let mut items = vec![
            MenuItem { label: "New Folder", action: MenuAction::NewFolder },
            MenuItem { label: "New File", action: MenuAction::NewFile },
            MenuItem { label: "Paste", action: MenuAction::Paste },
        ];
        // a terminal has no meaning in the trash listing
        if !in_trash {
            items.push(MenuItem { label: "Open Terminal Here", action: MenuAction::Terminal });
        }
        items.extend([
            MenuItem { label: "Sort by Name", action: MenuAction::SortName },
            MenuItem { label: "Sort by Size", action: MenuAction::SortSize },
            MenuItem { label: "Sort by Date", action: MenuAction::SortModified },
            MenuItem { label: "Show Hidden", action: MenuAction::ToggleHidden },
        ]);
        items
    }

    fn conflict_overlay(&self, cx: &mut Context<Self>) -> Option<Div> {        let dialog = self.conflict_dialog.as_ref()?;
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
                this.save_state();
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_places();
        self.arm_watcher();

        // the info rail follows the cursor entry; keep its preview fed
        // the panel only shows while an entry is selected: the knob
        // says the user wants it, the cursor says there is something
        // to show
        let show_panel = self.inspector && self.tab().cursor.is_some();
        if show_panel {
            let focus = self
                .tab()
                .cursor
                .and_then(|ix| self.tab().entries.get(ix))
                .map(|e| e.key.clone());
            match focus {
                Some(key) => {
                    if self.preview_key.as_ref() != Some(&key) {
                        self.preview_key = Some(key.clone());
                        self.text_preview = None;
                        let entry = self.tab().entries.iter().find(|e| e.key == key).cloned();
                        if let Some(entry) = entry.filter(|e| e.item.is_none()) {
                            if !entry.is_dir && icons::is_thumbable(&entry.name) {
                                self.request_thumb(entry.path.clone(), cx);
                            } else if !entry.is_dir {
                                self.request_text_preview(entry.path.clone(), cx);
                            }
                        }
                    }
                }
                None => {
                    self.preview_key = None;
                    self.text_preview = None;
                }
            }
        }

        // the titlebar follows the active folder
        let current = self.tab().current_dir().map(Path::to_path_buf);
        if current != self.titled {
            self.titled = current.clone();
            let name = current
                .as_deref()
                .and_then(Path::file_name)
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Koguma".into());
            window.set_window_title(&format!("Koguma: {name}"));
        }

        // owned so the icon cells can kick off thumbnail decodes (which
        // need &mut self) while iterating; rows keep their entry index
        // so element state and cursor stay entry-keyed
        let entries: Vec<Entry> = self.tab().entries.clone();
        let visible = self.visible_indices();
        let mut rows: Vec<Stateful<Div>> = Vec::new();
        for &entry_ix in visible.iter().take(200) {
            let entry = &entries[entry_ix];
            match self.tab().view_mode {
                ViewMode::List => rows.push(self.row(entry_ix, entry, cx)),
                ViewMode::Icons => rows.push(self.icon_cell(entry_ix, entry, cx)),
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
        let delete_armed = self
            .delete_armed
            .is_some_and(|armed| armed.elapsed() <= PURGE_ARM);

        let status_text = if self.busy {
            self.progress.clone()
        } else if purge_armed {
            "press Delete again to purge permanently (no undo)".into()
        } else if delete_armed {
            "press Shift+Delete again to delete permanently (no undo)".into()
        } else {
            self.status.clone()
        };

        // the volume this tab sits on: refresh free space in the
        // background whenever navigation lands somewhere new
        if matches!(self.tab().source, Source::Dir(_)) {
            self.request_free_space(cx);
        }
        let free_text = self
            .free_space
            .as_ref()
            .filter(|(dir, _)| Some(dir.as_path()) == self.tab().current_dir())
            .map(|(_, bytes)| format!("{} free", human_size(*bytes)));
        let status_color = if error_status {
            theme::error()
        } else if self.busy || purge_armed || delete_armed {
            theme::accent()
        } else {
            theme::text_dim()
        };

        // what the keyboard cursor sits on: name, size, age
        let cursor_info = self
            .tab()
            .cursor
            .and_then(|ix| self.tab().entries.get(ix))
            .map(|entry| {
                let size = entry
                    .size
                    .map(human_size)
                    .unwrap_or_else(|| if entry.is_dir { "folder".into() } else { String::new() });
                let age = entry
                    .modified
                    .map(|secs| relative_time(secs, now_secs()))
                    .unwrap_or_default();
                format!("{} · {} · {}", entry.name, size, age)
            });

        // selection summary: when something is selected, its count and
        // combined byte size ride along in the items cell
        let selected_bytes: u64 = self
            .tab()
            .entries
            .iter()
            .filter(|entry| self.tab().selection.contains(&entry.key))
            .filter_map(|entry| entry.size)
            .sum();
        let items_text = format!(
            "{} items, {} selected{}",
            self.tab().entries.len(),
            self.tab().selection.len(),
            if !self.tab().selection.is_empty() && selected_bytes > 0 {
                format!(", {}", human_size(selected_bytes))
            } else {
                String::new()
            }
        );

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
            .child(
                div()
                    .w(px(190.))
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
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                svg()
                                    .path("icons/trash.svg")
                                    .size(px(14.))
                                    .flex_none()
                                    .text_color(if in_trash {
                                        theme::accent()
                                    } else {
                                        theme::text_dim()
                                    }),
                            )
                            .child("Trash"),
                    )
                    .child(
                        // push the keys section to the bottom edge
                        div().flex_grow_1(),
                    )
                    .child(self.keys_section(cx)),
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
                            )
                            .child(
                                // double-click anywhere right of the tabs
                                // opens a new one; a sibling spacer, never a
                                // handler on this row (parent-first dispatch
                                // would eat the tabs' own clicks)
                                div()
                                    .id("tab-bar-empty")
                                    .flex_grow_1()
                                    .min_h(px(8.))
                                    .on_click(cx.listener(
                                        |this, event: &ClickEvent, _, cx| {
                                            if event.click_count() >= 2 {
                                                this.new_tab(cx);
                                            }
                                        },
                                    )),
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
                                    .id("new-file")
                                    .cursor_pointer()
                                    .hover(|this| this.text_color(theme::text()))
                                    .on_click(cx.listener(|this, _, _, cx| this.new_file(cx)))
                                    .child(
                                        svg()
                                            .path("icons/square_plus.svg")
                                            .size(px(16.))
                                            .text_color(theme::text_dim()),
                                    ),
                            )
                            .child(if self.path_editing {
                                let cursor =
                                    if self.path_buffer.is_char_boundary(self.path_cursor) {
                                        self.path_cursor
                                    } else {
                                        self.path_buffer.len()
                                    };
                                let (before, after) = self.path_buffer.split_at(cursor);
                                div()
                                    .id("path-edit")
                                    .flex_1()
                                    .min_w_0()
                                    .border_1()
                                    .border_color(theme::accent())
                                    .rounded_sm()
                                    .px_1()
                                    .text_color(theme::text())
                                    .child(format!("{before}▏{after}"))
                            } else {
                                self.path_crumbs(cx)
                            })
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
                                    .child(
                                        svg()
                                            .path("icons/list.svg")
                                            .size(px(14.))
                                            .text_color(if self.tab().view_mode == ViewMode::List {
                                                theme::accent()
                                            } else {
                                                theme::text_dim()
                                            }),
                                    ),
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
                                    .child(
                                        svg()
                                            .path("icons/grid.svg")
                                            .size(px(14.))
                                            .text_color(if self.tab().view_mode == ViewMode::Icons {
                                                theme::accent()
                                            } else {
                                                theme::text_dim()
                                            }),
                                    ),
                            )
                            .children(in_trash.then(|| {
                                let armed = self
                                    .empty_armed
                                    .is_some_and(|armed| armed.elapsed() <= PURGE_ARM);
                                div()
                                    .id("empty-trash")
                                    .px_2()
                                    .py_0p5()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .text_size(px(12.))
                                    .text_color(if armed {
                                        theme::error()
                                    } else {
                                        theme::text_dim()
                                    })
                                    .bg(if armed {
                                        theme::row()
                                    } else {
                                        theme::clear()
                                    })
                                    .hover(|this| this.text_color(theme::text()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.run_menu_action(MenuAction::EmptyTrash, cx)
                                    }))
                                    .child(if armed {
                                        "confirm: empty trash"
                                    } else {
                                        "Empty Trash"
                                    })
                            }))
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
                        // list flows as rows under sortable headers, icons
                        // as a wrapping grid. Empty space is a dedicated
                        // sibling spacer, never a handler on this ancestor:
                        // bubble dispatch runs parent-first, so an ancestor
                        // on_click would fire before the rows' own and eat
                        // their double-clicks. In icon view the spacer also
                        // starts the rubber band drag.
                        let s = self.scale;
                        let list_box = div()
                            .id("list")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll();
                        let clear_on_click = cx.listener(
                            |this, event: &ClickEvent, _, cx| {
                                if event.click_count() >= 2 {
                                    this.go_up(cx);
                                } else {
                                    let tab = this.tab_mut();
                                    tab.selection.clear();
                                    tab.cursor = None;
                                    this.disarm();
                                    cx.notify();
                                }
                            },
                        );
                        match self.tab().view_mode {
                            ViewMode::List => {
                                let header = self.sort_header(cx);
                                let empty_space = div()
                                    .id("list-empty")
                                    .flex_grow_1()
                                    .min_h(px(24.))
                                    .w_full()
                                    .on_click(clear_on_click)
                                    .on_mouse_down(
                                        MouseButton::Right,
                                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                            this.open_menu(
                                                f32::from(event.position.x),
                                                f32::from(event.position.y),
                                                Self::empty_menu_items(
                                                    this.tab().source == Source::Trash,
                                                ),
                                            );
                                            cx.notify();
                                        }),
                                    );
                                list_box
                                    .flex()
                                    .flex_col()
                                    .gap_px()
                                    .p_2()
                                    .child(header)
                                    .children(rows)
                                    .child(empty_space)
                            }
                            ViewMode::Icons => {
                                // a catcher under the grid: in a wrap
                                // container a grown spacer only fills its
                                // own line, so the open space below the
                                // last row must be a real flex_grow child
                                // of this column
                                let catcher = div()
                                    .id("list-empty")
                                    .flex_grow_1()
                                    .min_h(px(24.))
                                    .w_full()
                                    .on_click(clear_on_click)
                                    .on_mouse_down(
                                        MouseButton::Right,
                                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                            this.open_menu(
                                                f32::from(event.position.x),
                                                f32::from(event.position.y),
                                                Self::empty_menu_items(
                                                    this.tab().source == Source::Trash,
                                                ),
                                            );
                                            cx.notify();
                                        }),
                                    )
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(
                                            |this, event: &MouseDownEvent, _, cx| {
                                                log::info!(
                                                    "rubber: mouse down at {}",
                                                    event.position
                                                );
                                                this.rubber_origin = Some(event.position);
                                                this.rubber_current = None;
                                                cx.notify();
                                            },
                                        ),
                                    )
                                    .on_drag(RubberSelect, |_, _, _, cx| {
                                        log::info!("rubber: drag started");
                                        cx.new(|_| RubberGhost)
                                    })
                                    .on_mouse_up(
                                        MouseButton::Left,
                                        cx.listener(|this, _: &MouseUpEvent, _, cx| {
                                            if this.rubber_current.take().is_some() {
                                                cx.notify();
                                            }
                                        }),
                                    );

                                // the selection band, drawn over the grid;
                                // hit testing happens against the same
                                // pinned geometry at drop
                                let overlay = match (
                                    self.rubber_origin,
                                    self.rubber_current,
                                    self.rubber_bounds,
                                ) {
                                    (Some(origin), Some(current), Some(bounds)) => {
                                        let ax = f32::from(origin.x - bounds.origin.x);
                                        let ay = f32::from(origin.y - bounds.origin.y);
                                        let bx = f32::from(current.x - bounds.origin.x);
                                        let by = f32::from(current.y - bounds.origin.y);
                                        let (x0, x1) = if ax < bx { (ax, bx) } else { (bx, ax) };
                                        let (y0, y1) = if ay < by { (ay, by) } else { (by, ay) };
                                        Some(
                                            div()
                                                .absolute()
                                                .left(px(x0))
                                                .top(px(y0))
                                                .w(px(x1 - x0))
                                                .h(px(y1 - y0))
                                                .border_1()
                                                .border_color(theme::accent())
                                                .bg(theme::rubber_band()),
                                        )
                                    }
                                    _ => None,
                                };

                                list_box
                                    .flex()
                                    .flex_col()
                                    .p(px(8. * s))
                                    .relative()
                                    .child(
                                        div()
                                            .flex()
                                            .flex_wrap()
                                            .content_start()
                                            .gap(px(4. * s))
                                            // count the first row at paint
                                            // time: arrow-key navigation
                                            // uses the real grid shape
                                            .on_children_prepainted({
                                                let row_len = self.grid_row_len.clone();
                                                move |children, _, _| {
                                                    let Some(first) = children.first() else {
                                                        return;
                                                    };
                                                    let y = first.origin.y;
                                                    let count = children
                                                        .iter()
                                                        .filter(|b| b.origin.y == y)
                                                        .count();
                                                    row_len.store(
                                                        count as usize,
                                                        Ordering::Relaxed,
                                                    );
                                                }
                                            })
                                            .children(rows),
                                    )
                                    // the band paints after the grid so
                                    // selected-cell backgrounds do not
                                    // cover it; it has no hitbox, so
                                    // clicks pass through to the catcher
                                    .children(overlay)
                                    .child(catcher)
                                    .on_drag_move::<RubberSelect>(cx.listener(
                                        |this, event: &DragMoveEvent<RubberSelect>, _, cx| {
                                            if this.rubber_current.is_none() {
                                                log::info!("rubber: first drag move");
                                            }
                                            this.rubber_current = Some(event.event.position);
                                            this.rubber_bounds = Some(event.bounds);
                                            this.rubber_ctrl =
                                                event.event.modifiers.control;
                                            cx.notify();
                                        },
                                    ))
                                    .on_drop(cx.listener(|this, _: &RubberSelect, _, cx| {
                                        log::info!("rubber: drop");
                                        this.finish_rubber(cx);
                                    }))
                            }
                        }
                    })
                    .child(if show_panel && self.inspector_bottom {
                        self.inspector_panel(true, cx)
                    } else {
                        div()
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
                            .child(items_text)
                            .child(div().flex_1().truncate().child(status_text))
                            .child(if self.filter.is_empty() {
                                div()
                            } else {
                                div()
                                    .flex_none()
                                    .text_color(theme::accent())
                                    .truncate()
                                    .child(format!("filter: {} (Esc clears)", self.filter))
                            })
                            .child(match &free_text {
                                Some(text) => div()
                                    .flex_none()
                                    .text_color(theme::text_dim())
                                    .child(text.clone()),
                                None => div(),
                            })
                            .child(match cursor_info {
                                Some(info) => div()
                                    .flex_none()
                                    .max_w(px(360.))
                                    .text_color(theme::text_dim())
                                    .truncate()
                                    .child(info),
                                None => div(),
                            }),
                    ),
            )
            .child(if show_panel && !self.inspector_bottom {
                self.inspector_panel(false, cx)
            } else {
                div()
            })
            // overlays paint last so they land on top of the listing:
            // gpui paints children in tree order and absolute position
            // does not lift an element above later siblings
            .children(self.conflict_overlay(cx))
            .children(self.menu_overlay(window, cx))
    }
}

/// Free bytes on the volume holding dir, via libc statvfs (already in
/// the binary through gpui; no new build artifact).
fn free_bytes(dir: &Path) -> Option<u64> {
    let cpath = CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(cpath.as_ptr(), &mut vfs) };
    if rc != 0 {
        return None;
    }
    Some(vfs.f_bavail as u64 * vfs.f_frsize as u64)
}

/// A file:// URI with minimal percent-encoding for odd characters.
fn path_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.as_os_str().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'_' | b'-' | b'~' => {
                uri.push(*byte as char)
            }
            other => uri.push_str(&format!("%{other:02X}")),
        }
    }
    uri
}

/// A human kind for the info panel: folder, or an extension-mapped
/// label like "PNG image" instead of a bare "file".
fn friendly_kind(name: &str, is_dir: bool) -> String {
    if is_dir {
        return "folder".into();
    }
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tiff" | "avif" => {
            format!("{} image", ext.to_uppercase())
        }
        "pdf" => "PDF document".into(),
        "md" | "markdown" => "Markdown document".into(),
        "txt" | "log" => "text document".into(),
        "rs" => "Rust source".into(),
        "py" => "Python source".into(),
        "sh" | "bash" | "zsh" => "shell script".into(),
        "js" => "JavaScript source".into(),
        "ts" => "TypeScript source".into(),
        "c" | "h" => "C source".into(),
        "cpp" | "hpp" => "C++ source".into(),
        "go" => "Go source".into(),
        "json" => "JSON file".into(),
        "toml" => "TOML file".into(),
        "yaml" | "yml" => "YAML file".into(),
        "html" | "xml" | "css" | "kdl" => format!("{} file", ext.to_uppercase()),
        "zip" | "7z" | "rar" | "bz2" | "xz" | "zst" | "gz" | "tar" => {
            format!("{} archive", ext.to_uppercase())
        }
        "mp3" | "wav" | "flac" | "ogg" | "opus" | "m4a" => format!("{} audio", ext.to_uppercase()),
        "mp4" | "mkv" | "webm" | "mov" | "avi" => format!("{} video", ext.to_uppercase()),
        "iso" => "disk image".into(),
        "" => "file".into(),
        _ => format!("{ext} file"),
    }
}

/// How to style a text snippet in the info panel.
#[derive(Clone, Copy, PartialEq)]
enum TextKind {
    Plain,
    Code,
    Csv,
    Markdown,
}

#[derive(Clone)]
struct TextPreview {
    kind: TextKind,
    lines: Vec<String>,
    /// Parsed markdown, when kind is Markdown; None or empty falls
    /// back to plain lines.
    blocks: Option<Vec<MdBlock>>,
}

fn text_kind(path: &Path) -> TextKind {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    match ext {
        "csv" => TextKind::Csv,
        "md" | "markdown" => TextKind::Markdown,
        "json" | "toml" | "yaml" | "yml" | "ini" | "conf" | "kdl" | "sh" | "bash" | "zsh"
        | "py" | "rs" | "go" | "c" | "h" | "cpp" | "js" | "ts" | "html" | "xml" | "css"
        | "service" | "desktop" => TextKind::Code,
        _ => TextKind::Plain,
    }
}

/// One styled line for the preview box.
fn preview_line(line: String) -> Div {
    div().w_full().truncate().child(line)
}

/// A code-ish line: keys accented, comments dimmed.
fn code_line(line: &str) -> Div {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    let line = preview_line(trimmed.to_string()).pl(px(indent as f32 * 6.6));
    if trimmed.starts_with('#') || trimmed.starts_with("//") {
        line.text_color(theme::text_dim())
    } else if trimmed.starts_with('[') {
        line.text_color(theme::accent())
    } else if trimmed.starts_with('<') {
        line.text_color(theme::text())
    } else if let Some((key, value)) = trimmed
        .split_once(": ")
        .or_else(|| trimmed.split_once('='))
    {
        div()
            .w_full()
            .flex()
            .gap_1()
            .pl(px(indent as f32 * 6.6))
            .child(
                div()
                    .truncate()
                    .text_color(theme::accent())
                    .child(key.to_string()),
            )
            .child(
                div()
                    .truncate()
                    .text_color(theme::text())
                    .child(value.to_string()),
            )
    } else {
        line.text_color(theme::text())
    }
}

/// A csv preview: cells padded into an aligned mono table, first row
/// as the header. Quote handling is out of scope for a glance.
fn csv_lines(lines: &[String]) -> Vec<String> {
    let rows: Vec<Vec<&str>> = lines
        .iter()
        .map(|line| line.split(',').collect::<Vec<_>>())
        .collect();
    let columns = rows
        .iter()
        .map(|row| row.len())
        .max()
        .unwrap_or(0)
        .min(12);
    let mut widths = vec![0usize; columns];
    for row in &rows {
        for (c, cell) in row.iter().enumerate().take(columns) {
            widths[c] = widths[c].max(cell.len().min(24));
        }
    }
    rows.iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .take(columns)
                .map(|(c, cell)| {
                    let mut cell = cell.to_string();
                    if cell.len() > 24 {
                        cell.truncate(23);
                        cell.push('…');
                    }
                    format!("{cell:<width$}", width = widths[c])
                })
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_string()
        })
        .collect()
}

/// Inline markdown runs as a wrapping StyledText element.
fn md_spans(spans: &[MdSpan]) -> StyledText {
    let mut text = String::new();
    let mut highlights = Vec::new();
    for span in spans {
        let start = text.len();
        text.push_str(&span.text);
        let mut style = HighlightStyle::default();
        if span.bold {
            style.font_weight = Some(FontWeight::BOLD);
        }
        if span.italic {
            style.font_style = Some(FontStyle::Italic);
        }
        if span.code {
            style.background_color = Some(theme::row().into());
            style.color = Some(theme::accent().into());
        }
        if span.link {
            style.color = Some(theme::accent().into());
            style.underline = Some(UnderlineStyle::default());
        }
        highlights.push((start..text.len(), style));
    }
    StyledText::new(SharedString::from(text)).with_highlights(highlights)
}

/// Lay out parsed markdown blocks for the preview box. Falls back to
/// the raw lines when a file parses to nothing.
fn md_blocks(preview: &TextPreview) -> Vec<AnyElement> {
    let blocks = match &preview.blocks {
        Some(blocks) if !blocks.is_empty() => blocks,
        _ => {
            return preview
                .lines
                .iter()
                .map(|line| {
                    preview_line(line.clone())
                        .text_color(theme::text())
                        .into_any_element()
                })
                .collect();
        }
    };
    blocks
        .iter()
        .map(|block| match block {
            MdBlock::Heading { level, spans } => div()
                .font_weight(FontWeight::BOLD)
                .text_size(px(match level {
                    1 => 16.,
                    2 => 15.,
                    _ => 13.,
                }))
                .text_color(if *level <= 2 {
                    theme::accent()
                } else {
                    theme::text()
                })
                .child(md_spans(spans))
                .into_any_element(),
            MdBlock::Para(spans) => div()
                .w_full()
                .text_color(theme::text())
                .child(md_spans(spans))
                .into_any_element(),
            MdBlock::Quote(spans) => div()
                .border_l_1()
                .border_color(theme::accent())
                .pl_2()
                .italic()
                .text_color(theme::text_dim())
                .child(md_spans(spans))
                .into_any_element(),
            MdBlock::Item(spans) => div()
                .w_full()
                .flex()
                .gap_1()
                .child(div().text_color(theme::accent()).child("·"))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(theme::text())
                        .child(md_spans(spans)),
                )
                .into_any_element(),
            MdBlock::Code(lines) => div()
                .w_full()
                .flex()
                .flex_col()
                .gap_px()
                .rounded_sm()
                .border_1()
                .border_color(theme::border())
                .p_1()
                .text_size(px(11.))
                .font_family("monospace")
                .text_color(theme::text())
                .children(lines.iter().map(|l| preview_line(l.clone())))
                .into_any_element(),
            MdBlock::Table(lines) => div()
                .w_full()
                .flex()
                .flex_col()
                .gap_px()
                .text_size(px(11.))
                .font_family("monospace")
                .children(
                    lines
                        .iter()
                        .map(|l| preview_line(l.clone()).text_color(theme::text_dim())),
                )
                .into_any_element(),
            MdBlock::Rule => div()
                .w_full()
                .h(px(1.))
                .flex_none()
                .bg(theme::border())
                .into_any_element(),
        })
        .collect()
}

/// One styled run of inline markdown text.
#[derive(Clone, Default)]
struct MdSpan {
    text: String,
    bold: bool,
    italic: bool,
    code: bool,
    link: bool,
}

/// A parsed markdown block, ready to lay out in the preview.
#[derive(Clone)]
enum MdBlock {
    Heading { level: u8, spans: Vec<MdSpan> },
    Para(Vec<MdSpan>),
    Quote(Vec<MdSpan>),
    Item(Vec<MdSpan>),
    Code(Vec<String>),
    Table(Vec<String>),
    Rule,
}

fn parse_markdown(text: &str) -> Vec<MdBlock> {
    use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);

    let mut blocks = Vec::new();
    let mut spans: Vec<MdSpan> = Vec::new();
    let mut bold: i32 = 0;
    let mut italic: i32 = 0;
    let mut in_link = false;
    let mut item_depth = 0;
    let mut quote_depth = 0;
    let mut in_code = false;
    let mut code_lines: Vec<String> = Vec::new();
    let mut in_table = false;
    let mut table_rows: Vec<Vec<String>> = Vec::new();

    let push_span = |spans: &mut Vec<MdSpan>, text: &str, style: (bool, bool, bool)| {
        if text.is_empty() {
            return;
        }
        let md_span = MdSpan {
            text: text.to_string(),
            bold: style.0,
            italic: style.1,
            code: false,
            link: style.2,
        };
        // merge into the previous run when the style is unchanged
        if let Some(last) = spans.last_mut()
            && last.bold == md_span.bold
            && last.italic == md_span.italic
            && last.code == md_span.code
            && last.link == md_span.link
        {
            last.text.push_str(text);
        } else {
            spans.push(md_span);
        }
    };

    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(Tag::CodeBlock(_)) => {
                in_code = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code = false;
                blocks.push(MdBlock::Code(std::mem::take(&mut code_lines)));
            }
            Event::Start(Tag::Table(_)) => in_table = true,
            Event::End(TagEnd::Table) => {
                in_table = false;
                let rows = std::mem::take(&mut table_rows);
                blocks.push(MdBlock::Table(csv_lines(
                    &rows.iter().map(|row| row.join(",")).collect::<Vec<_>>(),
                )));
            }
            Event::Start(Tag::TableHead | Tag::TableRow) => table_rows.push(Vec::new()),
            Event::Start(Tag::TableCell) => {
                if let Some(row) = table_rows.last_mut() {
                    row.push(String::new());
                }
            }
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                if item_depth == 0 && quote_depth == 0 && !in_table {
                    blocks.push(MdBlock::Para(std::mem::take(&mut spans)));
                }
            }
            Event::Start(Tag::Item) => item_depth += 1,
            Event::End(TagEnd::Item) => {
                item_depth -= 1;
                if item_depth == 0 {
                    blocks.push(MdBlock::Item(std::mem::take(&mut spans)));
                }
            }
            Event::Start(Tag::BlockQuote(_)) => quote_depth += 1,
            Event::End(TagEnd::BlockQuote(_)) => {
                quote_depth -= 1;
                blocks.push(MdBlock::Quote(std::mem::take(&mut spans)));
            }
            Event::Start(Tag::Heading { level, .. }) => {
                spans.clear();
                let _ = level;
            }
            Event::End(TagEnd::Heading(level)) => {
                let level = match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                };
                blocks.push(MdBlock::Heading {
                    level,
                    spans: std::mem::take(&mut spans),
                });
            }
            Event::Rule => blocks.push(MdBlock::Rule),
            Event::Text(t) => {
                if in_code {
                    for line in t.lines() {
                        code_lines.push(line.to_string());
                    }
                } else if in_table {
                    if let Some(row) = table_rows.last_mut()
                        && let Some(cell) = row.last_mut()
                    {
                        cell.push_str(&t);
                    }
                } else {
                    push_span(&mut spans, &t, (bold > 0, italic > 0, in_link));
                }
            }
            Event::Code(t) => spans.push(MdSpan {
                text: t.to_string(),
                bold: false,
                italic: false,
                code: true,
                link: false,
            }),
            Event::SoftBreak | Event::HardBreak => {
                push_span(&mut spans, " ", (bold > 0, italic > 0, in_link))
            }
            Event::TaskListMarker(checked) => push_span(
                &mut spans,
                if checked { "[x] " } else { "[ ] " },
                (bold > 0, italic > 0, in_link),
            ),
            Event::Start(Tag::Strong) => bold += 1,
            Event::End(TagEnd::Strong) => bold = bold.saturating_sub(1),
            Event::Start(Tag::Emphasis) => italic += 1,
            Event::End(TagEnd::Emphasis) => italic = italic.saturating_sub(1),
            Event::Start(Tag::Link { .. }) => in_link = true,
            Event::End(TagEnd::Link) => in_link = false,
            _ => {}
        }
    }
    blocks
}

/// The sidebar cheatsheet's lines: key, what it does.
const KEY_HINTS: &[(&str, &str)] = &[
    ("Enter", "open"),
    ("F2", "rename"),
    ("Del", "trash"),
    ("Shift+Del", "delete"),
    ("Alt+Enter", "info"),
    ("Right-click", "menu"),
    ("Ctrl+C/X/V/Z", "clipboard"),
    ("Ctrl+Shift+C", "copy path"),
    ("Ctrl+Left/Right", "back/fwd"),
    ("Ctrl+Up", "up folder"),
    ("Ctrl+L/F6", "edit path"),
    ("Ctrl+Tab", "switch tab"),
    ("Alt+Home", "home"),
    ("F5", "refresh"),
    ("Ctrl+H", "hidden"),
    ("Ctrl+1/2", "views"),
    ("Ctrl+=/-/0", "zoom"),
    ("Ctrl+T/W", "tabs"),
    ("Type", "filter"),
];

fn sort_entries(entries: &mut [Entry], key: SortKey, asc: bool) {
    entries.sort_by(|a, b| {
        // directories always first, then the chosen key
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            let ord = match key {
                SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortKey::Size => a.size.unwrap_or(0).cmp(&b.size.unwrap_or(0)),
                SortKey::Modified => a.modified.unwrap_or(0).cmp(&b.modified.unwrap_or(0)),
            };
            if asc {
                ord
            } else {
                ord.reverse()
            }
        })
    });
}

/// Relative mtime for the details column: minutes/hours/days ago, then
/// a plain UTC date (no tz dep; good enough for a listing).
fn relative_time(secs: i64, now: i64) -> String {
    let delta = now - secs;
    match delta {
        ..0 => "in the future".into(),
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", delta / 60),
        3600..=86399 => format!("{}h ago", delta / 3600),
        86400..=604799 => format!("{}d ago", delta / 86400),
        _ => utc_date(secs),
    }
}

/// Days-since-epoch to "YYYY-MM-DD" (Howard Hinnant's civil_from_days).
fn utc_date(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// One label/value line in the properties card.
fn prop_row(label: &str, value: String) -> Div {
    div()
        .flex()
        .gap_3()
        .child(
            div()
                .w(px(90.))
                .flex_none()
                .text_color(theme::text_dim())
                .child(label.to_string()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(theme::text())
                .child(value),
        )
}

fn mode_string(mode: u32) -> String {
    let mut out = String::with_capacity(9);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 7;
        out.push(if bits & 4 != 0 { 'r' } else { '-' });
        out.push(if bits & 2 != 0 { 'w' } else { '-' });
        out.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    out
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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

/// Marker payload for the icon-grid rubber band drag.
struct RubberSelect;

/// The drag ghost for a rubber band: nothing, the band itself is the
/// feedback.
struct RubberGhost;

impl Render for RubberGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_parses_into_blocks() {
        let blocks = parse_markdown(
            "# Title\n\nIntro with **bold** and `code`.\n\n- one\n- two\n\n> quoted\n\n```rust\nlet x = 1;\n```\n",
        );
        assert!(matches!(blocks[0], MdBlock::Heading { level: 1, .. }));
        assert!(matches!(blocks[1], MdBlock::Para(_)));
        let bolded = &blocks[1];
        let MdBlock::Para(spans) = bolded else {
            panic!("expected para");
        };
        assert!(spans.iter().any(|s| s.bold));
        assert!(spans.iter().any(|s| s.code));
        assert_eq!(
            blocks
                .iter()
                .filter(|b| matches!(b, MdBlock::Item(_)))
                .count(),
            2
        );
        assert!(blocks.iter().any(|b| matches!(b, MdBlock::Quote(_))));
        assert!(blocks
            .iter()
            .any(|b| matches!(b, MdBlock::Code(lines) if lines.len() == 1)));
    }

    #[test]
    fn markdown_table_becomes_aligned_lines() {
        let blocks = parse_markdown("| a | bb |\n|---|----|\n| 1 | 2  |\n");
        let MdBlock::Table(lines) = &blocks[0] else {
            panic!("expected table");
        };
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("a "));
    }

    #[test]
    fn free_bytes_reports_a_volume() {
        assert!(free_bytes(Path::new("/tmp")).is_some_and(|n| n > 0));
        assert!(free_bytes(Path::new("/no/such/dir/anywhere")).is_none());
    }

    #[test]
    fn path_uri_escapes_spaces() {
        assert_eq!(path_uri(Path::new("/tmp/a b/c.txt")), "file:///tmp/a%20b/c.txt");
        assert_eq!(path_uri(Path::new("/tmp/plain")), "file:///tmp/plain");
    }

    #[test]
    fn friendly_kinds_map_extensions() {
        assert_eq!(friendly_kind("thing/", true), "folder");
        assert_eq!(friendly_kind("photo.png", false), "PNG image");
        assert_eq!(friendly_kind("readme.md", false), "Markdown document");
        assert_eq!(friendly_kind("backup.zip", false), "ZIP archive");
        assert_eq!(friendly_kind("clip.mov", false), "MOV video");
        assert_eq!(friendly_kind("Makefile", false), "file");
    }

    #[test]
    fn csv_alignment_caps_columns() {
        let lines = csv_lines(&[
            "name,role,city".to_string(),
            "ada,pilot,berlin".to_string(),
        ]);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "name  role   city");
        assert_eq!(lines[1], "ada   pilot  berlin");
    }
}
