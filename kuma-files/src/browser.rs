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

/// A sidebar place row being dragged to a new position.
#[derive(Clone)]
struct PlaceDrag(PathBuf);

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
    /// Recursive search hit: the containing folder's path relative to
    /// the tab's directory, shown as a dim second line. `None` for
    /// rows the flat listing produced.
    rel: Option<String>,
}

#[derive(Clone)]
enum Source {
    Dir(PathBuf),
    Trash,
    /// Virtual listing backed by ~/.local/share/recently-used.xbel
    Recent,
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
            (Source::Recent, Source::Recent) => true,
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
            Source::Recent => "Recent".into(),
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
    /// Unpack an archive into its own folder next to it.
    Extract { archive: PathBuf, dest: PathBuf },
    /// Pack the selection into a new archive.
    Compress { files: Vec<PathBuf>, archive: PathBuf },
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
            Op::Extract { .. } => "extracting",
            Op::Compress { .. } => "compressing",
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
            Op::Extract { archive, dest } => Self::run_extract(archive.clone(), dest.clone()),
            Op::Compress { files, archive } => {
                Self::run_compress(files.clone(), archive.clone())
            }
        }
    }

    /// Unpack an archive. Tool order: tar first (covers every tar.*),
    /// unzip for zip, single-file decompressors for lone .gz/.bz2/...,
    /// file-roller as the catch-all for 7z, rar, and friends.
    fn run_extract(archive: PathBuf, dest: PathBuf) -> Result<Option<Op>, String> {
        fs::create_dir_all(&dest).map_err(|err| format!("extract: {err}"))?;
        let name = archive
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let kind =
            archive_kind(name).ok_or_else(|| format!("{name} is not an archive"))?;
        let waited = |out: Result<std::process::Output, io::Error>,
                      tool: &str|
         -> Result<(), String> {
            match out {
                Ok(out) if out.status.success() => Ok(()),
                Ok(out) => {
                    let noise = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    Err(if noise.is_empty() {
                        format!("{tool} exited with an error")
                    } else {
                        format!("{tool}: {noise}")
                    })
                }
                Err(err) => Err(format!("{tool}: {err}")),
            }
        };
        match kind {
            "gz" | "bz2" | "xz" | "zst" => {
                let tool = match kind {
                    "gz" => "gzip",
                    "bz2" => "bzip2",
                    "xz" => "xz",
                    _ => "zstd",
                };
                if have_tool(tool) {
                    // decompress the single file into dest/<stem>
                    let inner = dest.join(archive_stem(name).unwrap_or_else(|| name.to_string()));
                    let file = fs::File::create(&inner)
                        .map_err(|err| format!("extract: {err}"))?;
                    waited(
                        Command::new(tool)
                            .arg("-dc")
                            .arg(&archive)
                            .stdout(file)
                            .output(),
                        tool,
                    )?;
                } else if have_tool("file-roller") {
                    waited(
                        Command::new("file-roller")
                            .arg(format!("--extract-to={}", dest.display()))
                            .arg("--force")
                            .arg(&archive)
                            .output(),
                        "file-roller",
                    )?;
                } else {
                    return Err("no extractor for this archive".into());
                }
            }
            kind if kind.starts_with("tar") && have_tool("tar") => {
                waited(
                    Command::new("tar")
                        .arg("-xf")
                        .arg(&archive)
                        .arg("-C")
                        .arg(&dest)
                        .output(),
                    "tar",
                )?;
            }
            "zip" if have_tool("unzip") => {
                waited(
                    Command::new("unzip")
                        .args(["-oq"])
                        .arg(&archive)
                        .arg("-d")
                        .arg(&dest)
                        .output(),
                    "unzip",
                )?;
            }
            _ => {
                if have_tool("file-roller") {
                    waited(
                        Command::new("file-roller")
                            .arg(format!("--extract-to={}", dest.display()))
                            .arg("--force")
                            .arg(&archive)
                            .output(),
                        "file-roller",
                    )?;
                } else {
                    return Err("no extractor for this archive".into());
                }
            }
        }
        Ok(None)
    }

    /// Pack the selection. tar.gz rides the universal tar; zip needs
    /// file-roller, whose add-to infers the format from the extension.
    fn run_compress(files: Vec<PathBuf>, archive: PathBuf) -> Result<Option<Op>, String> {
        if files.is_empty() {
            return Err("nothing to compress".into());
        }
        if archive.exists() {
            return Err(format!("{} already exists", archive.display()));
        }
        let is_zip = archive
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.to_lowercase().ends_with(".zip"))
            .unwrap_or(false);
        let dir = archive
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let out = if is_zip {
            if !have_tool("file-roller") {
                return Err("creating zip needs file-roller".into());
            }
            Command::new("file-roller")
                .arg(format!("--add-to={}", archive.display()))
                .args(&files)
                .output()
        } else {
            let basenames: Vec<&std::ffi::OsStr> = files
                .iter()
                .filter_map(|f| f.file_name())
                .collect();
            if basenames.len() != files.len() {
                return Err("compress: bad path".into());
            }
            // run from the target dir so both the archive name and the
            // source basenames resolve there: tar opens the archive
            // relative to its own cwd before any -C takes effect
            Command::new("tar")
                .arg("-czf")
                .arg(archive.file_name().unwrap_or_default())
                .args(basenames)
                .current_dir(&dir)
                .output()
        };
        match out {
            Ok(out) if out.status.success() => {
                log::info!("compress: {} created", archive.display());
                Ok(None)
            }
            Ok(out) => {
                let noise = String::from_utf8_lossy(&out.stderr).trim().to_string();
                log::info!("compress: tool failed: {noise}");
                Err(if noise.is_empty() {
                    "compress exited with an error".into()
                } else {
                    format!("compress: {noise}")
                })
            }
            Err(err) => Err(format!("compress: {err}")),
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
    /// pinned through the GTK bookmarks file (removable)
    bookmark: bool,
}

/// Snapshot for the properties dialog.
/// The right-click menu: where it opened plus a flat item list.
struct ContextMenu {
    x: f32,
    y: f32,
    items: Vec<MenuItem>,
    /// one dim helper line rendered under the items (used by the
    /// Open With picker to explain left vs right click)
    hint: Option<String>,
}

#[derive(Clone)]
struct MenuItem {
    label: String,
    action: MenuAction,
    /// quiet second line, e.g. an app's comment in the Open With picker
    detail: Option<String>,
    /// generic fallback entries render quieter than exact matches
    dim: bool,
}

impl MenuItem {
    fn new(label: impl Into<String>, action: MenuAction) -> Self {
        Self {
            label: label.into(),
            action,
            detail: None,
            dim: false,
        }
    }
}

/// What a menu item does when clicked. Dispatched through
/// `run_menu_action`, so the menu and the keyboard share handlers.
#[derive(Clone, Debug, PartialEq, Eq)]
enum MenuAction {
    Open,
    OpenWith,
    OpenWithApp(usize),
    Extract,
    Rename,
    Copy,
    Cut,
    CopyPath,
    CopyUri,
    /// Paste the clipboard into a specific folder (dir-row menu).
    PasteInto(PathBuf),
    Compress,
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
    /// Pin a folder in the GTK bookmarks file.
    Bookmark(PathBuf),
    /// Unpin a bookmarked folder.
    Unbookmark(PathBuf),
    /// Drop a file from the recency list.
    Forget,
    /// Open the accent (highlight color) picker.
    AccentPicker,
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

/// Compress dialog: target archive name, caret, format choice, and
/// whether the zip option has a tool behind it on this machine.
struct CompressDialog {
    name: String,
    cursor: usize,
    zip: bool,
    zip_available: bool,
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
    /// Compress dialog state: target name, caret, and format.
    compress: Option<CompressDialog>,
    /// Installed applications, scanned once on first Open With.
    desktop_apps: Option<Arc<Vec<DesktopApp>>>,
    /// The apps listed in the open-with picker, indexed by item action.
    /// The bool marks generic fallback entries (claim text/plain or
    /// octet-stream instead of the file's own type).
    openwith_apps: Vec<(DesktopApp, bool)>,
    /// Info rail on the right, following the cursor entry. On by
    /// default; it only shows while an entry is selected.
    inspector: bool,
    /// Info rail docked to the bottom edge instead of the right.
    inspector_bottom: bool,
    /// Pinned dock: None follows the window width (auto), Some pins
    /// bottom or right no matter how the window is resized.
    inspector_lock: Option<bool>,
    /// Chosen accent: None follows the wallpaper (kuma-shell palette
    /// when present), Some pins a user color over it.
    accent: Option<u32>,
    /// Last seen mtime of the shell's palette file, so wallpaper
    /// changes are noticed on the regular refresh tick.
    palette_mtime: Option<std::time::SystemTime>,
    theme_refresh: Instant,
    accent_picker: bool,
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
    /// Sidebar order the user arranged by dragging; paths not in the
    /// list (and list entries not in the sidebar) fall back to the
    /// computed order, so churny mounts stay safe.
    place_order: Vec<PathBuf>,
    /// Bumped on every filter edit, navigation, and tab switch; a
    /// recursive-search walker that finishes under an older
    /// generation drops its results.
    search_gen: u64,
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
            compress: None,
            desktop_apps: None,
            openwith_apps: Vec::new(),
            inspector: true,
            inspector_bottom: false,
            inspector_lock: None,
            accent: None,
            palette_mtime: None,
            theme_refresh: Instant::now(),
            accent_picker: false,
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
            place_order: Vec::new(),
            places_refresh: Instant::now(),
            search_gen: 0,
        };
        browser.load_state(cli_dir.as_deref());
        let show_hidden = browser.show_hidden;
        browser.tab_mut().reload(show_hidden);
        browser.palette_mtime = Browser::palette_mtime();
        browser.apply_theme();
        browser.places = browser.ordered_places();
        browser.start_dir_watch(cx);
        browser
    }

    /// The computed place list with the user's drag order applied.
    /// Unknown paths sort to the end, so a fresh bookmark or a just
    /// mounted volume shows up at the bottom until it is moved.
    fn ordered_places(&self) -> Vec<Place> {
        let mut places = Self::places();
        places.sort_by_key(|place: &Place| {
            self.place_order
                .iter()
                .position(|path| path == &place.path)
                .unwrap_or(usize::MAX)
        });
        places
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
                    bookmark: false,
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
                        bookmark: false,
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

        // pinned folders ride the GTK bookmarks file, so Thunar and
        // Nautilus agree with us about what is pinned; entries the
        // XDG dirs or mounts already cover are not repeated
        for (path, name) in Self::read_bookmarks() {
            if !path.is_dir() || places.iter().any(|place| place.path == path) {
                continue;
            }
            let name = name.unwrap_or_else(|| {
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string())
            });
            places.push(Place {
                name,
                path,
                bookmark: true,
            });
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
            self.places = self.ordered_places();
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
        self.filter_changed(cx);
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

    fn open_recent(&mut self, cx: &mut Context<Self>) {
        self.load_source(Source::Recent, cx);
    }

    /// Is this dir already pinned? Menus consult this so "Add
    /// Bookmark" never offers a duplicate.
    fn dir_is_bookmarked(path: &Path) -> bool {
        Self::read_bookmarks().iter().any(|(p, _)| p == path)
    }

    /// The GTK bookmarks file, parsed. Missing file just means none.
    fn read_bookmarks() -> Vec<(PathBuf, Option<String>)> {
        match bookmarks_path().and_then(|path| fs::read_to_string(path).ok()) {
            Some(text) => parse_bookmarks(&text),
            None => Vec::new(),
        }
    }

    /// Pin a folder in the GTK bookmarks file, deduped against the
    /// rest of the sidebar so XDG dirs never show twice.
    fn add_bookmark(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let mut marks = Self::read_bookmarks();
        if marks.iter().any(|(path, _)| *path == dir) {
            self.status = "already bookmarked".into();
            cx.notify();
            return;
        }
        marks.push((dir.clone(), None));
        if let Err(err) = Self::write_bookmarks(&marks) {
            log::error!("bookmark write: {err}");
            self.status = format!("bookmark failed: {err}");
        } else {
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| dir.display().to_string());
            self.status = format!("bookmarked {name}");
        }
        self.refresh_places_now();
        cx.notify();
    }

    /// Unpin a folder pinned through the bookmarks file.
    fn remove_bookmark(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let marks: Vec<_> = Self::read_bookmarks()
            .into_iter()
            .filter(|(path, _)| *path != dir)
            .collect();
        if let Err(err) = Self::write_bookmarks(&marks) {
            log::error!("bookmark write: {err}");
            self.status = format!("bookmark failed: {err}");
        } else {
            self.status = "bookmark removed".into();
        }
        self.refresh_places_now();
        cx.notify();
    }

    /// Rewrite the bookmarks file, preserving custom names. Other
    /// apps' non-file lines (sftp and friends) are kept untouched.
    fn write_bookmarks(marks: &[(PathBuf, Option<String>)]) -> std::io::Result<()> {
        let Some(path) = bookmarks_path() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut text = String::new();
        for (path, name) in marks {
            text.push_str(&path_uri(path));
            if let Some(name) = name {
                text.push(' ');
                text.push_str(name);
            }
            text.push('\n');
        }
        fs::write(path, text)
    }

    /// The kuma-shell palette file: the session's wallpaper-derived
    /// colors, published for any app that wants them. Absent means we
    /// are not running under kuma-shell (or adaptive is off) and the
    /// built-ins stand.
    fn palette_path() -> Option<PathBuf> {
        Some(PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("kuma-shell/palette"))
    }

    fn palette_mtime() -> Option<std::time::SystemTime> {
        fs::metadata(Self::palette_path()?)
            .ok()?
            .modified()
            .ok()
    }

    fn read_palette() -> Option<theme::Palette> {
        let text = fs::read_to_string(Self::palette_path()?).ok()?;
        Some(parse_palette(&text))
    }

    /// Accent precedence: an explicit color wins over the wallpaper;
    /// auto means the shell palette when the shell published one, the
    /// built-ins otherwise.
    fn apply_theme(&mut self) {
        if let Some(palette) = Self::read_palette() {
            theme::apply_palette(&palette);
        } else {
            theme::apply_palette(&theme::Palette::default());
        }
        if let Some(hex) = self.accent {
            theme::set_accent(hex);
        }
    }

    /// Wallpaper changes republish the palette; noticed on the
    /// regular two second tick.
    fn refresh_palette(&mut self, cx: &mut Context<Self>) {
        if self.theme_refresh.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.theme_refresh = Instant::now();
        let mtime = Self::palette_mtime();
        if mtime != self.palette_mtime {
            self.palette_mtime = mtime;
            self.apply_theme();
            log::info!(
                "palette: {} (user accent {})",
                if Self::read_palette().is_some() {
                    "applied shell palette"
                } else {
                    "applied built-ins"
                },
                match self.accent {
                    Some(hex) => format!("#{hex:06x}"),
                    None => "unset".into(),
                },
            );
            cx.notify();
        }
    }

    /// The accent picker's choice, written to the state file.
    fn set_accent(&mut self, hex: Option<u32>, cx: &mut Context<Self>) {
        self.accent = hex;
        self.accent_picker = false;
        self.apply_theme();
        self.save_state();
        self.status = match hex {
            Some(_) => "accent set".into(),
            None => {
                if Self::read_palette().is_some() {
                    "accent follows the wallpaper".into()
                } else {
                    "accent: auto (no kuma-shell palette found)".into()
                }
            }
        };
        cx.notify();
    }

    fn refresh_places_now(&mut self) {
        self.places = self.ordered_places();
        self.places_refresh = Instant::now();
    }

    /// A dragged place row landed on another: the dragged place takes
    /// the target's slot. The order is ours (state file), unlike the
    /// bookmarks file whose order belongs to every GTK app.
    fn reorder_places(&mut self, dragged: PathBuf, target: PathBuf, cx: &mut Context<Self>) {
        let live: Vec<PathBuf> = self
            .places
            .iter()
            .map(|place| place.path.clone())
            .collect();
        self.place_order = apply_place_order(self.place_order.clone(), &live, &dragged, &target);
        self.refresh_places_now();
        self.save_state();
        cx.notify();
    }

    /// The recency list from the xbel store, newest first.
    fn read_recents() -> Vec<RecentEntry> {
        match recent_xbel_path().and_then(|path| fs::read_to_string(path).ok()) {
            Some(text) => parse_xbel(&text),
            None => Vec::new(),
        }
    }

    /// Record an opened file in the xbel store, newest first, capped.
    fn note_recent(&mut self, path: &Path) {
        let Some(store) = recent_xbel_path() else {
            return;
        };
        let entries = Self::read_recents();
        let entries = merge_recent(entries, path.to_path_buf(), now_secs(), 1000);
        if let Some(parent) = store.parent()
            && let Err(err) = fs::create_dir_all(parent)
        {
            log::error!("recent dir: {err}");
            return;
        }
        if let Err(err) = fs::write(&store, xbel_text(&entries)) {
            log::error!("recent write: {err}");
        }
        // viewing the Recent list should reflect the open right away
        if self.tab().source == Source::Recent {
            let show_hidden = self.show_hidden;
            self.tab_mut().reload(show_hidden);
        }
    }

    /// Drop one file from the recency list (the Recent row menu's
    /// "Forget").
    fn forget_recent(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self
            .tab()
            .cursor
            .and_then(|ix| self.tab().entries.get(ix))
            .map(|entry| entry.path.clone())
        else {
            return;
        };
        let entries: Vec<_> = Self::read_recents()
            .into_iter()
            .filter(|entry| entry.path != path)
            .collect();
        if let Some(store) = recent_xbel_path()
            && let Err(err) = fs::write(&store, xbel_text(&entries))
        {
            log::error!("recent write: {err}");
        }
        let show_hidden = self.show_hidden;
        self.tab_mut().reload(show_hidden);
        self.status = "forgotten".into();
        cx.notify();
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
                .child(self.tab().source.label());
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

    /// Drop recursive-search rows (they carry a relative path); the
    /// flat listing keeps whatever it has. Reports whether any were
    /// dropped.
    fn prune_deep(&mut self) -> bool {
        let tab = self.tab_mut();
        let before = tab.entries.len();
        tab.entries.retain(|entry| entry.rel.is_none());
        before != tab.entries.len()
    }

    /// Append a walker's results to the listing.
    fn extend_deep(&mut self, matches: Vec<Entry>) {
        self.tab_mut().entries.extend(matches);
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

    /// The filter changed: prune stale deep rows, and while the
    /// filter is non-empty kick a recursive search of the tab's
    /// directory subtree. Flat matches are already in place; deep
    /// results land in the background and only if nothing (filter
    /// edit, navigation, tab switch) moved on first.
    fn filter_changed(&mut self, cx: &mut Context<Self>) {
        self.search_gen += 1;
        let pruned = self.prune_deep();
        if self.filter.is_empty() {
            if pruned {
                self.snap_cursor_visible();
                cx.notify();
            }
            return;
        }
        let Source::Dir(root) = self.tab().source.clone() else {
            return;
        };
        let generation = self.search_gen;
        let filter = self.filter.clone();
        let show_hidden = self.show_hidden;
        cx.spawn(async move |this, cx| {
            let matches =
                cx.background_spawn(async move { search_subtree(&root, &filter, show_hidden) })
                    .await;
            let update = this.update(cx, |this, cx| {
                if this.search_gen != generation {
                    return; // the listing moved on; results are stale
                }
                let deep = matches.len();
                this.extend_deep(matches);
                if deep > 0 {
                    this.snap_cursor_visible();
                }
                cx.notify();
            });
            if let Err(err) = update {
                log::error!("search results update failed: {err:#}");
            }
        })
        .detach();
        cx.notify();
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
                        let filtering = !this.filter.is_empty();
                        this.tab_mut().reload(show_hidden);
                        // reload rebuilt the flat listing; deep rows
                        // died with it, so re-run a live search
                        if filtering {
                            this.filter_changed(cx);
                        }
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
        // the filter survives navigation; search the new tree too
        self.filter_changed(cx);
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
        // the incoming tab needs its own deep rows for a live filter
        self.filter_changed(cx);
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

    /// A dir request from a second launch: open it as a new tab in
    /// the first window.
    pub(crate) fn open_dir_in_new_tab(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if !dir.is_dir() {
            self.status = format!("not a directory: {}", dir.display());
            cx.notify();
            return;
        }
        let mut tab = Tab::new(Source::Dir(dir));
        let current = self.tab();
        tab.view_mode = current.view_mode;
        tab.sort_key = current.sort_key;
        tab.sort_asc = current.sort_asc;
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.tabs.last_mut().unwrap().reload(self.show_hidden);
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
            "ptyxis".into(),
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
                    log::info!("terminal: spawned {name} in {}", dir.display());
                    // reap from a throwaway thread so the shell never
                    // lingers as a zombie under our pid
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                    self.status = format!("opened {name}");
                    cx.notify();
                    return;
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    log::info!("terminal: {name} not found");
                    continue;
                }
                Err(err) => {
                    log::info!("terminal: {name} failed: {err}");
                    self.status = format!("terminal: {err}");
                    cx.notify();
                    return;
                }
            }
        }
        self.status = "no terminal found; set $TERMINAL".into();
        cx.notify();
    }

    /// Swap the menu for the open-with picker: apps claiming the
    /// entry's mime type, name order. The apps vec is cached.
    fn open_with_menu(&mut self, x: f32, y: f32, cx: &mut Context<Self>) {
        let entry = self
            .tab()
            .cursor
            .and_then(|ix| self.tab().entries.get(ix))
            .cloned();
        let Some(entry) = entry.filter(|e| !e.is_dir && e.item.is_none()) else {
            return;
        };
        if self.desktop_apps.is_none() {
            self.desktop_apps = Some(Arc::new(scan_desktop_apps()));
        }
        let ext = Path::new(&entry.name)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_lowercase)
            .unwrap_or_default();
        let mime = mime_for_ext(&ext);
        let apps = self.desktop_apps.as_ref().unwrap();
        // fallback chain like shared-mime-info intends: apps claiming
        // the exact type, then plain-text handlers, then anything that
        // opens unknown files. Fallback entries are flagged so the
        // picker can render them quieter.
        let mut ranked: Vec<(DesktopApp, bool)> = Vec::new();
        let push_ranked =
            |app: &DesktopApp, fallback: bool, ranked: &mut Vec<(DesktopApp, bool)>| {
            // the same program often ships several .desktop entries
            // (kitty vs kitty-open); show it once
            let program = app.exec.split_whitespace().next().unwrap_or(&app.exec);
            if !ranked.iter().any(|(seen, _)| {
                seen.exec.split_whitespace().next() == Some(program)
            }) {
                ranked.push((app.clone(), fallback));
            }
        };
        for claimant in apps.iter().filter(|app| app.mimes.iter().any(|m| m == mime)) {
            push_ranked(claimant, false, &mut ranked);
        }
        if mime != "text/plain" && ranked.is_empty() {
            for claimant in apps
                .iter()
                .filter(|app| app.mimes.iter().any(|m| m == "text/plain"))
            {
                push_ranked(claimant, true, &mut ranked);
            }
        }
        if ranked.is_empty() {
            for claimant in apps
                .iter()
                .filter(|app| {
                    app.mimes
                        .iter()
                        .any(|m| m == "application/octet-stream")
                })
            {
                push_ranked(claimant, true, &mut ranked);
            }
        }
        log::info!(
            "open with: {} claims {} ({} total apps)",
            ranked.len(),
            mime,
            apps.len()
        );
        self.openwith_apps = ranked;
        if self.openwith_apps.is_empty() {
            self.menu = None;
            self.status = format!("no apps claim {mime}");
            cx.notify();
            return;
        }
        self.menu = Some(ContextMenu {
            x,
            y,
            hint: Some("click to open, right-click to always use".into()),
            items: self
                .openwith_apps
                .iter()
                .enumerate()
                .map(|(ix, (app, fallback))| MenuItem {
                    label: app.name.clone(),
                    action: MenuAction::OpenWithApp(ix),
                    detail: app.comment.clone(),
                    dim: *fallback,
                })
                .collect(),
        });
        cx.notify();
    }

    /// Launch a picked application on the cursor entry's path.
    fn launch_app(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.menu = None;
        let Some((app, _)) = self.openwith_apps.get(ix).cloned() else {
            return;
        };
        let Some(entry) = self
            .tab()
            .cursor
            .and_then(|entry_ix| self.tab().entries.get(entry_ix))
            .cloned()
        else {
            return;
        };
        let argv = exec_argv(&app.exec, &entry.path, &app.name);
        let Some((program, args)) = argv.split_first() else {
            return;
        };
        log::info!("open with: launching {program} {args:?} on {}", entry.name);
        match Command::new(program).args(args).spawn() {
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                self.status = format!("opened with {}", app.name);
            }
            Err(err) => {
                log::error!("launch {}: {err}", app.name);
                self.status = format!("could not launch {}: {err}", app.name);
            }
        }
        cx.notify();
    }

    /// Right-click in the open-with picker: write the app into
    /// ~/.config/mimeapps.list so xdg-open (and our own double-click)
    /// picks it for this file type from now on.
    fn set_default_app(&mut self, ix: usize, cx: &mut Context<Self>) {
        let mime = self
            .tab()
            .cursor
            .and_then(|i| self.tab().entries.get(i))
            .and_then(|entry| Path::new(&entry.name).extension().and_then(|e| e.to_str()))
            .map(|e| mime_for_ext(e.to_lowercase().as_str()))
            .unwrap_or_else(|| "application/octet-stream".into());
        let Some((app, _)) = self.openwith_apps.get(ix) else {
            return;
        };
        let Some(config) = dirs::config_dir() else {
            self.status = "could not find the config directory".into();
            cx.notify();
            return;
        };
        let path = config.join("mimeapps.list");
        match write_mimeapps(&path, &mime, &app.id) {
            Ok(()) => {
                self.menu = None;
                self.status = format!("default for {mime}: {}", app.name);
            }
            Err(err) => {
                log::error!("mimeapps write: {err}");
                self.status = format!("could not set default: {err}");
            }
        }
        cx.notify();
    }

    /// Extract the cursor entry's archive into a folder named after
    /// it, via the op queue so busy/progress come for free.
    fn extract_selection(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        let entry = self
            .tab()
            .cursor
            .and_then(|ix| self.tab().entries.get(ix))
            .cloned();
        let Some(entry) = entry.filter(|e| !e.is_dir && e.item.is_none()) else {
            return;
        };
        let Some(stem) = archive_stem(&entry.name) else {
            self.status = "not an archive".into();
            cx.notify();
            return;
        };
        let Some(dest) = entry.path.parent().map(|dir| dir.join(stem)) else {
            return;
        };
        self.enqueue(
            vec![Op::Extract {
                archive: entry.path.clone(),
                dest,
            }],
            cx,
        );
    }

    /// Open the compress dialog over the current selection.
    fn open_compress_dialog(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        if matches!(self.tab().source, Source::Dir(_)) && !self.tab().selection.is_empty() {
            let default_name = self
                .tab()
                .entries
                .iter()
                .find(|e| self.tab().selection.contains(&e.key))
                .and_then(|e| e.path.file_stem().map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "archive".into());
            self.compress = Some(CompressDialog {
                name: format!("{default_name}.tar.gz"),
                cursor: 0,
                zip: false,
                zip_available: have_tool("file-roller"),
            });
            cx.notify();
        }
    }

    /// Flip the dialog's format and keep the name field honest: a
    /// trailing .tar.gz becomes .zip and vice versa; custom names
    /// without the other suffix are left alone.
    fn set_zip_format(dialog: &mut CompressDialog, zip: bool) {
        if dialog.zip == zip {
            return;
        }
        let (want, other): (&str, &str) = if zip {
            (".zip", ".tar.gz")
        } else {
            (".tar.gz", ".zip")
        };
        if dialog.name.to_lowercase().ends_with(other) {
            dialog.name =
                format!("{}{want}", &dialog.name[..dialog.name.len() - other.len()]);
        }
        dialog.cursor = dialog.cursor.min(dialog.name.len());
        dialog.zip = zip;
    }

    /// Build the archive from the dialog: name, format, selection.
    fn compress_create(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.compress.take() else {
            return;
        };
        let name = dialog.name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let Some(dir) = self.tab().current_dir().map(Path::to_path_buf) else {
            return;
        };
        let archive = dir.join(&name);
        let suffix = if dialog.zip { ".zip" } else { ".tar.gz" };
        let archive = if name.to_lowercase().ends_with(suffix) {
            archive
        } else {
            dir.join(format!("{name}{suffix}"))
        };
        let files: Vec<PathBuf> = self
            .tab()
            .entries
            .iter()
            .filter(|e| self.tab().selection.contains(&e.key))
            .map(|e| e.path.clone())
            .collect();
        log::info!(
            "compress: {} item(s) into {} (zip={})",
            files.len(),
            archive.display(),
            dialog.zip
        );
        self.enqueue(vec![Op::Compress { files, archive }], cx);
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
            // the button itself turns red and asks; nothing in the
            // status bar, so it never lingers after the arm window
            self.empty_armed = Some(Instant::now());
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
        let Source::Dir(dest_dir) = self.tab().source.clone() else {
            self.status = "paste goes to a folder; trash has no paste".into();
            cx.notify();
            return;
        };
        self.paste_into(dest_dir, cx);
    }

    /// Paste the clipboard into one specific folder: the current dir
    /// via the empty-space menu, or a row's folder via the dir-row
    /// "Paste Into Folder" entry.
    fn paste_into(&mut self, dest_dir: PathBuf, cx: &mut Context<Self>) {
        let Some((is_copy, paths)) = self.clipboard.clone() else {
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
        // no state file (or no config dir): the CLI dir, if any, is
        // still the whole session, so do not drop it on the floor
        let Some(path) = Self::state_path() else {
            if let Some(dir) = cli_dir {
                self.tab_mut().source = Source::Dir(dir.to_path_buf());
            }
            return;
        };
        let Ok(text) = fs::read_to_string(path) else {
            if let Some(dir) = cli_dir {
                self.tab_mut().source = Source::Dir(dir.to_path_buf());
            }
            return;
        };

        // parse everything first: the view knobs must apply AFTER the
        // tabs are resolved, not to a placeholder that gets thrown away
        let mut view: Option<ViewMode> = None;
        let mut sort: Option<SortKey> = None;
        let mut sort_asc = true;
        let mut saved_tabs: Vec<(usize, PathBuf)> = Vec::new();
        let mut saved_places: Vec<(usize, PathBuf)> = Vec::new();
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
                ("inspector-lock", "auto") => self.inspector_lock = None,
                ("inspector-lock", "bottom") => self.inspector_lock = Some(true),
                ("inspector-lock", "right") => self.inspector_lock = Some(false),
                ("accent", "auto") => self.accent = None,
                ("accent", _) => self.accent = parse_hex_color(value),
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
                    if let Some(index) =
                        key.strip_prefix("place").and_then(|n| n.parse().ok())
                        && !value.is_empty()
                    {
                        saved_places.push((index, PathBuf::from(value)));
                    }
                }
            }
        }

        // an explicit CLI dir always wins; otherwise reopen the folders
        // that were open last time (trash tabs are not persisted)
        saved_tabs.sort_by_key(|(index, _)| *index);
        saved_places.sort_by_key(|(index, _)| *index);
        self.place_order = saved_places.into_iter().map(|(_, path)| path).collect();
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
            "view={}\nsort={}\nasc={}\nhidden={}\ninspector={}\ninspector-bottom={}\ninspector-lock={}\naccent={}\nkeys={}\nscale={}\n",
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
            match self.inspector_lock {
                Some(true) => "bottom",
                Some(false) => "right",
                None => "auto",
            },
            match self.accent {
                Some(hex) => format!("#{hex:06x}"),
                None => "auto".into(),
            },
            self.keys_open,
            self.scale,
        );
        let mut text = text;
        for (i, tab) in self.tabs.iter().enumerate() {
            if let Some(dir) = tab.current_dir() {
                text.push_str(&format!("tab{}={}\n", i, dir.display()));
            }
        }
        for (i, path) in self.place_order.iter().enumerate() {
            text.push_str(&format!("place{}={}\n", i, path.display()));
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
    /// Auto dock from the window width, with hysteresis so a window
    /// resting near the threshold never flickers between docks: right
    /// at 1024 and up, back to bottom only below 950, unchanged in
    /// between.
    fn auto_dock(&mut self, window: &mut Window) {
        if self.inspector_lock.is_some() {
            return;
        }
        let width = window.viewport_size().width.as_f32();
        if let Some(bottom) = auto_dock_for(width, self.inspector_bottom) {
            self.inspector_bottom = bottom;
        }
    }

    fn flip_inspector(&mut self, cx: &mut Context<Self>) {
        self.inspector_bottom = !self.inspector_bottom;
        // in auto mode the next render would snap the dock straight
        // back, so a manual flip pins the panel where it is put
        self.inspector_lock = Some(self.inspector_bottom);
        self.save_state();
        cx.notify();
    }

    /// The lock between chevron and close: pin the panel to its
    /// current dock, or release it back to the window-width rule.
    fn toggle_inspector_lock(&mut self, cx: &mut Context<Self>) {
        self.inspector_lock = match self.inspector_lock {
            None => Some(self.inspector_bottom),
            Some(_) => None,
        };
        let status = match self.inspector_lock {
            Some(true) => "pane pinned to the bottom",
            Some(false) => "pane pinned to the right",
            None => "pane follows the window width",
        };
        self.status = status.into();
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
                            rel: None,
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
            Source::Recent => {
                // the xbel store is the listing, newest first; capped
                // well below what the store may hold so we do not stat
                // thousands of dead screenshots
                let mut recents = Browser::read_recents();
                recents.sort_by(|a, b| b.modified.cmp(&a.modified));
                recents.truncate(200);
                let mut entries = Vec::new();
                for recent in recents {
                    // gone files do not belong in the listing
                    let Ok(meta) = fs::symlink_metadata(&recent.path) else {
                        continue;
                    };
                    let name = recent
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| recent.path.display().to_string());
                    entries.push(Entry {
                        key: recent.path.clone(),
                        path: recent.path,
                        name,
                        // recent files are files; pinned dirs live in
                        // the bookmarks section of the sidebar
                        is_dir: false,
                        size: Some(meta.len()),
                        modified: Some(recent.modified),
                        item: None,
                        rel: None,
                    });
                }
                self.entries = entries;
            }
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
                        rel: None,
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
            Source::Trash | Source::Recent => None,
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
                // enter picks the common intent; when the collision is a
                // folder Replace is not offered, so enter falls back to
                // Keep both, matching the rendered buttons
                "enter" => {
                    let can_replace = self.conflict_dialog.as_ref().is_some_and(|d| {
                        let op = &d.ops[d.conflicts[d.ix]];
                        op.from.is_file() && op.to.is_file()
                    });
                    if can_replace {
                        self.conflict_decide(ConflictDecision::Replace, cx);
                    } else {
                        self.conflict_decide(ConflictDecision::KeepBoth, cx);
                    }
                }
                "k" => self.conflict_decide(ConflictDecision::KeepBoth, cx),
                "s" => self.conflict_decide(ConflictDecision::Skip, cx),
                "a" => {
                    if let Some(mut dialog) = self.conflict_dialog.take() {
                        dialog.apply_all = !dialog.apply_all;
                        self.conflict_dialog = Some(dialog);
                        cx.notify();
                    }
                }
                _ => {}
            }
            return;
        }

        if self.compress.is_some() {
            let mut dialog = self.compress.take().unwrap();
            match keystroke.key.as_str() {
                "enter" => {
                    self.compress = Some(dialog);
                    self.compress_create(cx);
                    return;
                }
                "escape" => {
                    cx.notify();
                    return;
                }
                "backspace" => {
                    if dialog.cursor > 0 {
                        let head = &dialog.name[..dialog.cursor];
                        if let Some((prev, _)) = head.char_indices().next_back() {
                            dialog.name.remove(prev);
                            dialog.cursor = prev;
                        }
                    }
                }
                "left" => {
                    if dialog.cursor > 0 {
                        let head = &dialog.name[..dialog.cursor];
                        if let Some((prev, _)) = head.char_indices().next_back() {
                            dialog.cursor = prev;
                        }
                    }
                }
                "right" => {
                    if dialog.name.is_char_boundary(dialog.cursor)
                        && dialog.cursor < dialog.name.len()
                    {
                        let tail = &dialog.name[dialog.cursor..];
                        if let Some(ch) = tail.chars().next() {
                            dialog.cursor += ch.len_utf8();
                        }
                    }
                }
                _ if !keystroke.modifiers.control
                    && !keystroke.modifiers.alt
                    && !keystroke.modifiers.platform
                    && !keystroke.modifiers.function =>
                {
                    if let Some(character) = keystroke.key_char.as_deref() {
                        let cursor = if dialog.name.is_char_boundary(dialog.cursor) {
                            dialog.cursor
                        } else {
                            dialog.name.len()
                        };
                        dialog.name.insert_str(cursor, character);
                        dialog.cursor = cursor + character.len();
                    }
                }
                _ => {}
            }
            self.compress = Some(dialog);
            cx.notify();
            return;
        }

        if self.accent_picker {
            if keystroke.key == "escape" {
                self.accent_picker = false;
                cx.notify();
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
                    self.filter_changed(cx);
                } else {
                    self.go_up(cx);
                }
            }
            "escape" => {
                if filtering {
                    self.filter.clear();
                    self.filter_changed(cx);
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
                self.filter_changed(cx);
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
                self.filter_changed(cx);
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
                Ok(mut child) => {
                    // reap from a throwaway thread so the opener never
                    // lingers as a zombie under our pid
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                    self.status = format!("opened {}", entry.name);
                    // freedesktop recency: other file managers and the
                    // shell read the same store
                    self.note_recent(&entry.path);
                }
                Err(err) => {
                    log::error!("xdg-open {}: {err}", entry.path.display());
                    self.status = format!("open failed: {err}");
                }
            }
            cx.notify();
        }
    }

    fn start_rename(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.tab().source, Source::Dir(_)) {
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
        self.move_into(dragged, &target.path, cx);
    }

    /// Move a dragged entry into a destination folder. Drops have no
    /// modifier report in gpui, so drop means move; Cut + Paste is the
    /// copy path, same convention as the listing rows.
    fn move_into(&mut self, dragged: &DragEntry, dest_dir: &Path, cx: &mut Context<Self>) {
        let name = dragged
            .path
            .file_name()
            .map_or_else(|| "unnamed".into(), |n| n.to_string_lossy().into_owned());
        let dest = dest_dir.join(&name);
        if dragged.path == dest || dragged.path == dest_dir {
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
        let entry_is_dir = entry.is_dir;
        let entry_is_archive = is_archive(&entry.name);

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
                    } else if this.tab().source == Source::Recent {
                        Self::recent_menu_items()
                    } else {
                        let bookmarked = Self::dir_is_bookmarked(&menu_key);
                        Self::row_menu_items(
                            entry_is_dir,
                            entry_is_archive,
                            &menu_key,
                            bookmarked,
                            this.clipboard.is_some(),
                        )
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
        } else if let Some(rel) = &entry.rel {
            // recursive-search hit: name plus where it lives
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .justify_center()
                .child(
                    div()
                        .truncate()
                        .text_size(px(13. * s))
                        .child(entry.name.clone()),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(11. * s))
                        .text_color(theme::text_dim())
                        .child(rel.clone()),
                )
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
        let entry_is_dir = entry.is_dir;
        let entry_is_archive = is_archive(&entry.name);
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
                    } else if this.tab().source == Source::Recent {
                        Self::recent_menu_items()
                    } else {
                        let bookmarked = Self::dir_is_bookmarked(&menu_key);
                        Self::row_menu_items(
                            entry_is_dir,
                            entry_is_archive,
                            &menu_key,
                            bookmarked,
                            this.clipboard.is_some(),
                        )
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
            .child(if entry.rel.is_some() {
                // deep search hit: a second dim line under the name
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(
                        div()
                            .h(px(16. * s))
                            .text_size(px(12. * s))
                            .line_height(relative(1.3))
                            .overflow_hidden()
                            .text_center()
                            .truncate()
                            .child(entry.name.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(10. * s))
                            .text_color(theme::text_dim())
                            .overflow_hidden()
                            .text_center()
                            .truncate()
                            .child(entry.rel.clone().unwrap_or_default()),
                    )
            } else {
                div()
                    .h(px(16. * s))
                    .w_full()
                    .text_size(px(12. * s))
                    .line_height(relative(1.3))
                    .overflow_hidden()
                    .text_center()
                    .truncate()
                    .child(entry.name.clone())
            })
    }

    fn place_row(&self, ix: usize, place: &Place, cx: &mut Context<Self>) -> Stateful<Div> {
        let here = self.tab().current_dir() == Some(place.path.as_path());
        let path = place.path.clone();
        let unbookmark_path = place.path.clone();
        let bookmarked = place.bookmark;
        let place_drag = PlaceDrag(path.clone());
        let reorder_drag = path.clone();
        let move_target = path.clone();
        let external_name = place.name.clone();
        div()
            .id(format!("place-{ix}"))
            .on_drag(
                place_drag,
                |dragged: &PlaceDrag, position, _, cx| {
                    let name = dragged
                        .0
                        .file_name()
                        .map_or_else(|| "folder".into(), |n| n.to_string_lossy().into_owned());
                    cx.new(|_| Ghost { name, position })
                },
            )
            .drag_over::<PlaceDrag>(|style, _, _, _| style.bg(theme::drag_over()))
            .on_drop(cx.listener(move |this, dragged: &PlaceDrag, _, cx| {
                this.reorder_places(dragged.0.clone(), reorder_drag.clone(), cx);
            }))
            // dropping files on a place moves them into that folder,
            // the same gesture as dropping them on a dir row
            .drag_over::<DragEntry>(|style, _, _, _| style.bg(theme::drag_over()))
            .on_drop(cx.listener(move |this, dragged: &DragEntry, _, cx| {
                this.move_into(dragged, &move_target, cx);
            }))
            .on_drop(cx.listener(move |this, _paths: &ExternalPaths, _, cx| {
                this.status = format!(
                    "external drop onto {external_name}: imports land with the queue"
                );
                cx.notify();
            }))
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
            // pinned places can be unpinned from their row menu
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    if bookmarked {
                        this.open_menu(
                            f32::from(event.position.x),
                            f32::from(event.position.y),
                            vec![MenuItem::new(
                                "Remove Bookmark",
                                MenuAction::Unbookmark(unbookmark_path.clone()),
                            )],
                        );
                        cx.notify();
                    }
                }),
            )            .flex()
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

        let locked = self.inspector_lock == Some(bottom);
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
                // pin the panel to its current dock, or release it
                // back to the window-width rule
                div()
                    .id("inspector-lock")
                    .flex()
                    .h(px(20.))
                    .items_center()
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .text_color(if locked {
                        theme::accent()
                    } else {
                        theme::text_dim()
                    })
                    .hover(|this| this.text_color(theme::text()).bg(theme::row_hover()))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_inspector_lock(cx)))
                    .child(
                        svg()
                            .path(if locked {
                                "icons/lock.svg"
                            } else {
                                "icons/lock_open.svg"
                            })
                            .size(px(14.))
                            .text_color(if locked {
                                theme::accent()
                            } else {
                                theme::text_dim()
                            }),
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
            let action = item.action.clone();
            let mut row = div()
                .id(format!("menu-item-{i}"))
                .debug_selector(|| "menu-item-row".into())
                .flex()
                .flex_col()
                .px_3()
                .py_1()
                .cursor_pointer()
                .text_color(if item.dim {
                    theme::text_dim()
                } else {
                    theme::text()
                })
                .hover(|this| this.bg(theme::row_hover()))
                // eat the downs so the backdrop behind does not
                // close the menu mid-click: gpui dispatches bubble
                // handlers last-painted first, so the item's own
                // down runs before the backdrop's and stopping
                // here keeps the menu open until the click lands
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
            // in the open-with picker, right-click makes the app the
            // default handler for the file's type (mimeapps.list)
            let open_with_ix = match &action {
                MenuAction::OpenWithApp(ix) => Some(*ix),
                _ => None,
            };
            if let Some(ix) = open_with_ix {
                row = row.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.set_default_app(ix, cx);
                    }),
                );
            } else {
                row = row.on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation());
            }
            if let Some(detail) = &item.detail {
                row = row.child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::text_dim())
                        .child(detail.clone()),
                );
            }
            panel = panel.child(
                row.child(item.label.clone()).on_click(
                    cx.listener(move |this, _, _, cx| this.run_menu_action(action.clone(), cx)),
                ),
            );
        }
        if let Some(hint) = &menu.hint {
            panel = panel.child(
                div()
                    .mt_1()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(theme::border())
                    .text_size(px(11.))
                    .text_color(theme::text_dim())
                    .child(hint.clone()),
            );
        }

        Some(
            div()
                .absolute()
                .inset_0()
                // clicks on the backdrop close the menu via its own
                // handler and must not rubber-band the listing behind
                .occlude()
                .child(backdrop)
                .child(panel),
        )
    }

    /// The compress dialog: name field, format choice, create/cancel.
    /// Painted last so it sits above the listing.
    /// The accent picker: preset swatches plus a follow-the-wallpaper
    /// release. A small card, like the compress dialog.
    fn accent_overlay(&self, cx: &mut Context<Self>) -> Option<Div> {
        if !self.accent_picker {
            return None;
        }
        let current = self.accent;
        let palette_live = Self::read_palette().is_some();
        let swatches = ACCENT_PRESETS
            .iter()
            .enumerate()
            .map(|(ix, hex)| {
                let hex = *hex;
                let selected = current == Some(hex);
                div()
                    .id(format!("accent-swatch-{ix}"))
                    .size(px(28.))
                    .rounded_sm()
                    .cursor_pointer()
                    .bg(rgb(hex))
                    .border_1()
                    .border_color(if selected {
                        theme::text()
                    } else {
                        theme::border()
                    })
                    .hover(|this| this.border_color(theme::text()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_accent(Some(hex), cx);
                    }))
            })
            .collect::<Vec<_>>();
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x00000066))
                // a click outside the card closes it
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                    this.accent_picker = false;
                    cx.notify();
                }))
                .child(
                    div()
                        .w(px(300.))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_4()
                        .rounded_md()
                        .bg(theme::sidebar())
                        .border_1()
                        .border_color(theme::border())
                        // clicks inside the card must not fall through
                        // to the catcher above
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            div()
                                .text_size(px(14.))
                                .text_color(theme::text())
                                .child("Accent"),
                        )
                        .child(
                            div().flex().flex_wrap().gap_2().children(swatches),
                        )
                        .child(
                            div()
                                .id("accent-follow-wallpaper")
                                .w_full()
                                .flex()
                                .flex_col()
                                .gap_0p5()
                                .px_2()
                                .py_1p5()
                                .rounded_sm()
                                .cursor_pointer()
                                .text_color(if current.is_none() {
                                    theme::accent()
                                } else {
                                    theme::text_dim()
                                })
                                .hover(|this| this.bg(theme::row_hover()))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_accent(None, cx);
                                }))
                                .child("Follow Wallpaper")
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(theme::text_dim())
                                        .child(if palette_live {
                                            "kuma-shell palette detected"
                                        } else {
                                            "no kuma-shell palette found"
                                        }),
                                ),
                        ),
                ),
        )
    }

    fn compress_overlay(&self, cx: &mut Context<Self>) -> Option<Div> {        let dialog = self.compress.as_ref()?;
        let count = self.tab().selection.len();
        let cursor = if dialog.name.is_char_boundary(dialog.cursor) {
            dialog.cursor
        } else {
            dialog.name.len()
        };
        let (before, after) = dialog.name.split_at(cursor);
        Some(
            div()
                .absolute()
                .inset_0()
                // block the listing underneath: without this, clicks on
                // the dialog's buttons also land on the catcher behind
                // and wipe the selection (Create then finds nothing)
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x00000066))
                .child(
                    div()
                        .w(px(380.))
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
                                .text_size(px(14.))
                                .text_color(theme::text())
                                .child(format!(
                                    "Compress {} item{} into an archive",
                                    count,
                                    if count == 1 { "" } else { "s" }
                                )),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .border_1()
                                .border_color(theme::accent())
                                .rounded_sm()
                                .px_1()
                                .py_0p5()
                                .text_color(theme::text())
                                .child(format!("{before}▏{after}")),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    div()
                                        .id("fmt-tgz")
                                        .px_2()
                                        .py_0p5()
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .text_color(if dialog.zip {
                                            theme::text_dim()
                                        } else {
                                            theme::accent()
                                        })
                                        .bg(if dialog.zip {
                                            theme::clear()
                                        } else {
                                            theme::row()
                                        })
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if let Some(dialog) = this.compress.as_mut() {
                                                Self::set_zip_format(dialog, false);
                                            }
                                            cx.notify();
                                        }))
                                        .child("tar.gz"),
                                )
                                .child(if dialog.zip_available {
                                    div()
                                        .id("fmt-zip")
                                        .px_2()
                                        .py_0p5()
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .text_color(if dialog.zip {
                                            theme::accent()
                                        } else {
                                            theme::text_dim()
                                        })
                                        .bg(if dialog.zip {
                                            theme::row()
                                        } else {
                                            theme::clear()
                                        })
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if let Some(dialog) = this.compress.as_mut() {
                                                Self::set_zip_format(dialog, true);
                                            }
                                            cx.notify();
                                        }))
                                        .child("zip")
                                        .into_any_element()
                                } else {
                                    div()
                                        .px_2()
                                        .py_0p5()
                                        .text_color(theme::text_dim())
                                        .child("zip needs file-roller")
                                        .into_any_element()
                                }),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    div()
                                        .id("compress-cancel")
                                        .px_3()
                                        .py_1()
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .text_color(theme::text_dim())
                                        .hover(|this| this.text_color(theme::text()))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.compress = None;
                                            cx.notify();
                                        }))
                                        .child("Cancel"),
                                )
                                .child(
                                    div()
                                        .id("compress-create")
                                        .px_3()
                                        .py_1()
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .text_color(theme::accent())
                                        .bg(theme::row())
                                        .hover(|this| this.bg(theme::row_hover()))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.compress_create(cx)
                                        }))
                                        .child("Create"),
                                ),
                        ),
                ),
        )
    }

    fn run_menu_action(&mut self, action: MenuAction, cx: &mut Context<Self>) {
        let menu = self.menu.take();
        match action {
            MenuAction::Open => self.open_selection(cx),
            // the picker replaces this menu in place, so it needs the
            // position before the menu is dropped
            MenuAction::OpenWith => {
                let (x, y) = menu.map(|m| (m.x, m.y)).unwrap_or((120., 120.));
                self.open_with_menu(x, y, cx);
            }
            MenuAction::OpenWithApp(ix) => self.launch_app(ix, cx),
            MenuAction::Extract => self.extract_selection(cx),
            MenuAction::Compress => self.open_compress_dialog(cx),
            MenuAction::Rename => self.start_rename(cx),
            MenuAction::Copy => self.copy_selection(cx),
            MenuAction::Cut => self.cut_selection(cx),
            MenuAction::CopyPath => self.copy_paths_to_system(false, cx),
            MenuAction::CopyUri => self.copy_paths_to_system(true, cx),
            MenuAction::Paste => self.paste(cx),
            MenuAction::PasteInto(dir) => self.paste_into(dir, cx),
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
            MenuAction::Bookmark(dir) => self.add_bookmark(dir, cx),
            MenuAction::Unbookmark(dir) => self.remove_bookmark(dir, cx),
            MenuAction::Forget => self.forget_recent(cx),
            MenuAction::AccentPicker => {
                self.menu = None;
                self.accent_picker = true;
                cx.notify();
            }
        }
    }

    fn open_menu(&mut self, x: f32, y: f32, items: Vec<MenuItem>) {
        self.menu = Some(ContextMenu { x, y, items, hint: None });
    }

    /// Shift+F10 or the Menu key: the row menu for the keyboard
    /// cursor. The menu system is pointer-coordinate based, so it
    /// anchors at the current mouse position.
    fn open_cursor_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entry = self
            .tab()
            .cursor
            .and_then(|ix| self.tab().entries.get(ix).cloned());
        let Some(entry) = entry else {
            return;
        };
        let items = if self.tab().source == Source::Recent {
            Self::recent_menu_items()
        } else if matches!(self.tab().source, Source::Trash) {
            Self::trash_menu_items()
        } else {
            let bookmarked = Self::dir_is_bookmarked(&entry.path);
            Self::row_menu_items(
                entry.is_dir,
                is_archive(&entry.name),
                &entry.path,
                bookmarked,
                self.clipboard.is_some(),
            )
        };
        let pos = window.mouse_position();
        self.open_menu(f32::from(pos.x), f32::from(pos.y), items);
        cx.notify();
    }

    fn row_menu_items(
        is_dir: bool,
        archive: bool,
        path: &Path,
        bookmarked: bool,
        has_clipboard: bool,
    ) -> Vec<MenuItem> {
        let mut items = vec![MenuItem::new("Open", MenuAction::Open)];
        if !is_dir {
            items.push(MenuItem::new("Open With…", MenuAction::OpenWith));
        }
        if archive {
            items.push(MenuItem::new("Extract Here", MenuAction::Extract));
        }
        items.extend([
            MenuItem::new("Rename", MenuAction::Rename),
            MenuItem::new("Copy", MenuAction::Copy),
            MenuItem::new("Cut", MenuAction::Cut),
        ]);
        // paste straight into this folder, Explorer style; dimmed
        // (but present) while the clipboard is empty so the item
        // stays put between copy and use
        if is_dir {
            let mut paste = MenuItem::new(
                "Paste Into Folder",
                MenuAction::PasteInto(path.to_path_buf()),
            );
            paste.dim = !has_clipboard;
            items.push(paste);
        }
        items.extend([
            MenuItem::new("Copy Path", MenuAction::CopyPath),
            MenuItem::new("Copy URI", MenuAction::CopyUri),
        ]);
        // pinning only means something for directories, and only
        // once: a pinned dir offers nothing here
        if is_dir && !bookmarked {
            items.push(MenuItem::new(
                "Add Bookmark",
                MenuAction::Bookmark(path.to_path_buf()),
            ));
        }
        items.extend([
            MenuItem::new("Compress…", MenuAction::Compress),
            MenuItem::new("Trash", MenuAction::Trash),
            MenuItem::new("Delete permanently", MenuAction::Delete),
            MenuItem::new("Properties", MenuAction::Info),
        ]);
        items
    }

    fn trash_menu_items() -> Vec<MenuItem> {
        vec![
            MenuItem::new("Restore", MenuAction::Open),
            MenuItem::new("Delete permanently", MenuAction::Delete),
            MenuItem::new("Properties", MenuAction::Info),
        ]
    }

    fn recent_menu_items() -> Vec<MenuItem> {
        vec![
            MenuItem::new("Open", MenuAction::Open),
            MenuItem::new("Open With…", MenuAction::OpenWith),
            MenuItem::new("Copy", MenuAction::Copy),
            MenuItem::new("Copy Path", MenuAction::CopyPath),
            MenuItem::new("Copy URI", MenuAction::CopyUri),
            MenuItem::new("Forget", MenuAction::Forget),
            MenuItem::new("Properties", MenuAction::Info),
        ]
    }

    fn empty_menu_items(source: &Source) -> Vec<MenuItem> {
        let mut items = Vec::new();
        match source {
            Source::Dir(dir) => {
                items.extend([
                    MenuItem::new("New Folder", MenuAction::NewFolder),
                    MenuItem::new("New File", MenuAction::NewFile),
                    MenuItem::new("Paste", MenuAction::Paste),
                ]);
                if !Self::dir_is_bookmarked(dir) {
                    items.push(MenuItem::new(
                        "Bookmark This Folder",
                        MenuAction::Bookmark(dir.to_path_buf()),
                    ));
                }
                items.push(MenuItem::new("Open Terminal Here", MenuAction::Terminal));
                items.push(MenuItem::new("Accent…", MenuAction::AccentPicker));
            }
            // preserve the long-standing trash behavior: the file ops
            // target the selection, paste is inert without a clipboard
            Source::Trash => items.extend([
                MenuItem::new("New Folder", MenuAction::NewFolder),
                MenuItem::new("New File", MenuAction::NewFile),
                MenuItem::new("Paste", MenuAction::Paste),
            ]),
            Source::Recent => {}
        }
        items.extend([
            MenuItem::new("Sort by Name", MenuAction::SortName),
            MenuItem::new("Sort by Size", MenuAction::SortSize),
            MenuItem::new("Sort by Date", MenuAction::SortModified),
            MenuItem::new("Show Hidden", MenuAction::ToggleHidden),
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
                // same as the compress dialog: keep clicks out of the
                // listing underneath
                .occlude()
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
                                        .child("Replace (Enter)")
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
                                        .child("Keep both (K)"),
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
                                        .child("Skip (S)"),
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
                                    // toggle, not set: this handler once
                                    // assigned the captured value back,
                                    // so clicking the box did nothing
                                    if let Some(dialog) = &mut this.conflict_dialog {
                                        dialog.apply_all = !apply_all;
                                    }
                                    cx.notify();
                                }))
                                .child(if apply_all { "[x]" } else { "[ ]" })
                                .child("apply to all conflicts in this paste (A)")
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
        self.refresh_palette(cx);
        self.auto_dock(window);
        let inspector_docked_bottom = self.inspector_bottom;

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
        let in_recent = self.tab().source == Source::Recent;
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
        let deep = self.tab().entries.iter().filter(|e| e.rel.is_some()).count();
        let deep_note = if deep > 0 {
            format!(" (+{deep} in subfolders)")
        } else {
            String::new()
        };
        let items_text = format!(
            "{} items{}, {} selected{}",
            self.tab().entries.len() - deep,
            deep_note,
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
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let keystroke = &event.keystroke;
                // Shift+F10 / the Menu key: context menu at the
                // keyboard cursor
                if (keystroke.key == "f10" && keystroke.modifiers.shift)
                    || keystroke.key == "menu"
                {
                    this.open_cursor_menu(window, cx);
                    return;
                }
                // Ctrl+Q closes the window; everything else flows into
                // the keymap
                if keystroke.key == "q" && keystroke.modifiers.control {
                    window.remove_window();
                    return;
                }
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
                            .id("place-recent")
                            .px_3()
                            .py_1()
                            .rounded_sm()
                            .text_size(px(13.))
                            .cursor_pointer()
                            .text_color(if in_recent {
                                theme::accent()
                            } else {
                                theme::text_dim()
                            })
                            .bg(if in_recent {
                                theme::row_selected()
                            } else {
                                theme::clear()
                            })
                            .hover(|this| this.bg(theme::row_hover()))
                            .on_click(cx.listener(|this, _, _, cx| this.open_recent(cx)))
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                svg()
                                    .path("icons/clock.svg")
                                    .size(px(14.))
                                    .flex_none()
                                    .text_color(if in_recent {
                                        theme::accent()
                                    } else {
                                        theme::text_dim()
                                    }),
                            )
                            .child("Recent"),
                    )
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
                            )
                            .child(
                                // the window control: tabs close, the
                                // window itself closes here (or Ctrl+Q)
                                div()
                                    .id("window-close")
                                    .px_2()
                                    .h_full()
                                    .flex()
                                    .items_center()
                                    .cursor_pointer()
                                    .text_size(px(13.))
                                    .text_color(theme::text_dim())
                                    .hover(|this| {
                                        this.text_color(theme::error()).bg(theme::row_hover())
                                    })
                                    .on_click(cx.listener(|_this, _, window, _| {
                                        window.remove_window();
                                    }))
                                    .child("×"),
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
                                        "Are you sure?"
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
                                            let source = this.tab().source.clone();
                                            this.open_menu(
                                                f32::from(event.position.x),
                                                f32::from(event.position.y),
                                                Self::empty_menu_items(&source),
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
                                            let source = this.tab().source.clone();
                                            this.open_menu(
                                                f32::from(event.position.x),
                                                f32::from(event.position.y),
                                                Self::empty_menu_items(&source),
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
                    .child(if show_panel && inspector_docked_bottom {
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
                            .child(match &cursor_info {
                                // the info panel shows name, size, and
                                // age whenever it is open, so the footer
                                // readout is only for panel-closed use
                                Some(info) if !show_panel => div()
                                    .flex_none()
                                    .max_w(px(360.))
                                    .text_color(theme::text_dim())
                                    .truncate()
                                    .child(info.clone()),
                                _ => div(),
                            })
                            .child(match &free_text {
                                Some(text) => div()
                                    .flex_none()
                                    .pl_3()
                                    .text_color(theme::text_dim())
                                    .child(text.clone()),
                                None => div(),
                            }),
                    ),
            )
            .child(if show_panel && !inspector_docked_bottom {
                self.inspector_panel(false, cx)
            } else {
                div()
            })
            // overlays paint last so they land on top of the listing:
            // gpui paints children in tree order and absolute position
            // does not lift an element above later siblings
            .children(self.conflict_overlay(cx))
            .children(self.compress_overlay(cx))
            .children(self.accent_overlay(cx))
            .children(self.menu_overlay(window, cx))
    }
}

/// Is `name` on PATH?
fn have_tool(name: &str) -> bool {
    env::var_os("PATH").is_some_and(|paths| {
        env::split_paths(&paths).any(|dir| {
            let candidate = dir.join(name);
            candidate.is_file()
        })
    })
}

/// Record the default handler for a mime type in the user's
/// mimeapps.list (XDG spec): our app goes first, any previous
/// defaults stay on the line as fallbacks.
fn write_mimeapps(path: &Path, mime: &str, desktop_id: &str) -> std::io::Result<()> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut out = String::new();
    let mut in_defaults = false;
    let mut section_found = false;
    let mut inserted = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            // leaving the defaults section without having placed our
            // app: put it at the end of the section
            if in_defaults && !inserted {
                out.push_str(&format!("{mime}={desktop_id}\n"));
                inserted = true;
            }
            in_defaults = trimmed == "[Default Applications]";
            section_found |= in_defaults;
        } else if in_defaults && !inserted {
            let ours = trimmed.starts_with(&format!("{mime}="))
                || trimmed.starts_with(&format!("{mime} "));
            if ours {
                let rest = trimmed.split_once('=').map(|(_, v)| v).unwrap_or("");
                let others: Vec<&str> = rest
                    .split(';')
                    .filter(|a| !a.is_empty() && *a != desktop_id)
                    .collect();
                out.push_str(&format!("{mime}={desktop_id};{}\n", others.join(";")));
                inserted = true;
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    if !inserted {
        if !section_found {
            out.push_str("[Default Applications]\n");
        }
        out.push_str(&format!("{mime}={desktop_id}\n"));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, out)
}

/// What kind of archive is this file name, if any?
fn archive_kind(name: &str) -> Option<&'static str> {
    let lower = name.to_lowercase();
    let ends = |suffix: &str| lower.ends_with(suffix);
    if ends(".tar.gz") || ends(".tgz") {
        Some("tar.gz")
    } else if ends(".tar.bz2") || ends(".tbz2") {
        Some("tar.bz2")
    } else if ends(".tar.xz") || ends(".txz") {
        Some("tar.xz")
    } else if ends(".tar.zst") || ends(".tzst") {
        Some("tar.zst")
    } else if ends(".tar") {
        Some("tar")
    } else if ends(".zip") {
        Some("zip")
    } else if ends(".7z") {
        Some("7z")
    } else if ends(".rar") {
        Some("rar")
    } else if ends(".gz") {
        Some("gz")
    } else if ends(".bz2") {
        Some("bz2")
    } else if ends(".xz") {
        Some("xz")
    } else if ends(".zst") {
        Some("zst")
    } else {
        None
    }
}

/// How far below the tab's directory the search walks.
const SEARCH_MAX_DEPTH: usize = 6;
/// Bounds so a search from a huge tree cannot wedge the app. Walked
/// counts every fs entry visited; matches cap what the listing keeps.
const SEARCH_MAX_WALKED: usize = 4000;
const SEARCH_MAX_MATCHES: usize = 200;

/// The recursive half of the type-in filter: walk `root`'s subtree
/// looking for names the fuzzy filter scores, best first. Runs on the
/// background executor; the caller drops results if the filter or the
/// tab moved on. Skips dot-names unless `show_hidden`. Root-level
/// entries are not produced: the flat listing already has them.
fn search_subtree(root: &Path, filter: &str, show_hidden: bool) -> Vec<Entry> {
    use nucleo::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
    use nucleo::{Config, Matcher, Utf32String};

    let pattern = Pattern::new(
        filter,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut out: Vec<(u32, Entry)> = Vec::new();
    let mut walked = 0usize;
    // (dir, depth below root, dir's path relative to root)
    let mut stack = vec![(root.to_path_buf(), 0usize, String::new())];
    while let Some((dir, depth, rel_prefix)) = stack.pop() {
        let Ok(read) = fs::read_dir(&dir) else {
            continue;
        };
        for item in read.flatten() {
            walked += 1;
            if walked > SEARCH_MAX_WALKED || out.len() >= SEARCH_MAX_MATCHES {
                return best_first(out);
            }
            let name = item.file_name().to_string_lossy().into_owned();
            if !show_hidden && name.starts_with('.') {
                continue;
            }
            let path = item.path();
            let is_dir = item.file_type().is_ok_and(|t| t.is_dir());
            let child_rel = is_dir.then(|| {
                if rel_prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{rel_prefix}/{name}")
                }
            });
            if is_dir && depth < SEARCH_MAX_DEPTH {
                stack.push((path.clone(), depth + 1, child_rel.unwrap()));
            }
            // depth 0 is the flat listing's job; scoring here would
            // duplicate its rows
            if depth == 0 {
                continue;
            }
            let Some(score) =
                pattern.score(Utf32String::from(name.as_str()).slice(..), &mut matcher)
            else {
                continue;
            };
            let meta = fs::symlink_metadata(&path).ok();
            let size = meta.as_ref().filter(|_| !is_dir).map(|meta| meta.len());
            let modified = meta.and_then(|meta| {
                meta.modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
            });
            out.push((
                score,
                Entry {
                    key: path.clone(),
                    path,
                    name,
                    is_dir,
                    size,
                    modified,
                    item: None,
                    rel: Some(if rel_prefix.is_empty() {
                        ".".into()
                    } else {
                        rel_prefix.clone()
                    }),
                },
            ));
        }
    }
    best_first(out)
}

/// Sort walker output the way the flat filter ranks: score first,
/// then name. Plain files before folders within one score, mirroring
/// the listing's own sort.
fn best_first(mut scored: Vec<(u32, Entry)>) -> Vec<Entry> {
    scored.sort_by(|a, b| {
        b.0
            .cmp(&a.0)
            .then_with(|| a.1.is_dir.cmp(&b.1.is_dir))
            .then_with(|| a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase()))
    });
    scored.into_iter().map(|(_, entry)| entry).collect()
}

fn is_archive(name: &str) -> bool {
    archive_kind(name).is_some()
}

/// The file name with its archive suffix stripped, for the folder an
/// extraction unpacks into.
fn archive_stem(name: &str) -> Option<String> {
    let kind = archive_kind(name)?;
    let suffix = match kind {
        "tar.gz" => ".tar.gz",
        "tar.bz2" => ".tar.bz2",
        "tar.xz" => ".tar.xz",
        "tar.zst" => ".tar.zst",
        "tar" => ".tar",
        "zip" => ".zip",
        "7z" => ".7z",
        "rar" => ".rar",
        "gz" => ".gz",
        "bz2" => ".bz2",
        "xz" => ".xz",
        "zst" => ".zst",
        _ => return None,
    };
    Some(name[..name.len() - suffix.len()].to_string())
}

/// The mime type for a common extension; the fallback is the
/// everything-blob, which still matches "open with anything" apps.
fn mime_for_ext(ext: &str) -> &'static str {
    match ext.to_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        "bz2" => "application/x-bzip2",
        "xz" => "application/x-xz",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" | "opus" => "audio/ogg",
        "wav" => "audio/x-wav",
        "m4a" => "audio/mp4",
        "mp4" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "html" => "text/html",
        "css" => "text/css",
        "js" => "text/javascript",
        "json" => "application/json",
        "toml" => "application/toml",
        "yaml" | "yml" => "application/yaml",
        "xml" => "application/xml",
        "csv" => "text/csv",
        "md" | "markdown" => "text/markdown",
        "sh" | "bash" => "text/x-shellscript",
        "py" => "text/x-python",
        "rs" => "text/rust",
        "c" | "h" => "text/x-csrc",
        "cpp" | "hpp" => "text/x-c++src",
        "txt" | "log" | "conf" | "ini" | "kdl" => "text/plain",
        _ => "application/octet-stream",
    }
}

/// One installed application from the freedesktop .desktop files.
#[derive(Clone)]
struct DesktopApp {
    name: String,
    /// the .desktop file id, needed to name the app in mimeapps.list
    id: String,
    exec: String,
    mimes: Vec<String>,
    /// unlocalized Comment, shown as a quiet line in the Open With picker
    comment: Option<String>,
}

/// Scan the standard applications directories. Runs once per session
/// and is cached; a few hundred small INI files read in milliseconds.
fn scan_desktop_apps() -> Vec<DesktopApp> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(data_home) = env::var("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(data_home).join("applications"));
    } else if let Ok(home) = env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }
    dirs.push(PathBuf::from("/usr/share/applications"));
    if let Ok(data_dirs) = env::var("XDG_DATA_DIRS") {
        for dir in env::split_paths(&data_dirs) {
            dirs.push(dir.join("applications"));
        }
    }

    let mut apps: Vec<DesktopApp> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for dir in dirs {
        let Ok(listing) = fs::read_dir(&dir) else {
            continue;
        };
        for file in listing.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let (mut name, mut exec, mut mimes) = (String::new(), String::new(), Vec::new());
            let (mut comment, mut id) = (None, String::new());
            let (mut hidden, mut nodisplay, mut is_app) = (false, false, false);
            let mut in_entry = false;
            if let Some(fname) = path.file_name().and_then(|f| f.to_str()) {
                id = fname.to_string();
            }
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('[') {
                    in_entry = line == "[Desktop Entry]";
                    continue;
                }
                if !in_entry {
                    continue;
                }
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                match key {
                    "Name" => name = value.to_string(),
                    "Comment" => comment = Some(value.to_string()),
                    "Exec" => exec = value.to_string(),
                    "MimeType" => {
                        mimes = value
                            .split(';')
                            .filter(|m| !m.is_empty())
                            .map(str::to_string)
                            .collect()
                    }
                    "NoDisplay" => nodisplay = value == "true",
                    "Hidden" => hidden = value == "true",
                    "Type" => is_app = value == "Application",
                    _ => {}
                }
            }
            if !is_app || hidden || nodisplay || exec.is_empty() {
                continue;
            }
            let label = if name.is_empty() { exec.clone() } else { name };
            if seen.insert(label.clone()) {
                apps.push(DesktopApp {
                    name: label,
                    id,
                    exec,
                    mimes,
                    comment,
                });
            }
        }
    }
    apps
}

/// Turn a .desktop Exec line into an argv for one path: tokenize with
/// quote awareness, substitute the field codes, and append the path
/// when the line names none.
fn exec_argv(exec: &str, path: &Path, app_name: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = exec.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                if let Some(&next) = chars.peek() {
                    cur.push(next);
                    chars.next();
                }
            }
            '\'' | '"' if quote.is_none() => quote = Some(ch),
            ending if Some(ending) == quote => quote = None,
            split if quote.is_none() && split.is_whitespace() => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            other => cur.push(other),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }

    let mut argv: Vec<String> = Vec::new();
    let mut has_file = false;
    for token in tokens {
        match token.as_str() {
            "%f" | "%u" | "%F" | "%U" | "%d" | "%D" | "%n" | "%N" => {
                argv.push(path.display().to_string());
                has_file = true;
            }
            "%c" => argv.push(app_name.to_string()),
            "%i" | "%k" | "%v" | "%m" => {}
            field if field.starts_with('%') => {}
            plain => argv.push(plain.to_string()),
        }
    }
    if !has_file {
        argv.push(path.display().to_string());
    }
    argv
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

/// The inverse of path_uri for file:// URIs from gtk bookmarks and
/// recent-files xbel. Undecodable bytes become replacement chars.
fn uri_decode(uri: &str) -> PathBuf {
    let uri = uri.strip_prefix("file://").unwrap_or(uri);
    let mut out: Vec<u8> = Vec::with_capacity(uri.len());
    let mut chars = uri.as_bytes().iter().copied();
    while let Some(byte) = chars.next() {
        if byte == b'%' {
            let hex = [chars.next(), chars.next()];
            if let [Some(hi), Some(lo)] = hex {
                if let Some(value) = (hi as char).to_digit(16).and_then(|hi| {
                    (lo as char).to_digit(16).map(|lo| hi * 16 + lo)
                }) {
                    out.push(value as u8);
                    continue;
                }
            }
            out.push(byte);
        } else {
            out.push(byte);
        }
    }
    PathBuf::from(String::from_utf8_lossy(&out).into_owned())
}

/// Escape text for XML attribute/character data (xbel hrefs).
fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// A dragged place row landed on another: the dragged place takes
/// the target's slot. The stored order is folded together with the
/// live sidebar first, so churny mounts and fresh bookmarks keep
/// their computed position and churned ones drop out.
fn apply_place_order(
    mut order: Vec<PathBuf>,
    live: &[PathBuf],
    dragged: &Path,
    target: &Path,
) -> Vec<PathBuf> {
    for path in live {
        if !order.contains(path) {
            order.push(path.clone());
        }
    }
    order.retain(|path| live.contains(path));
    if dragged == target {
        return order;
    }
    order.retain(|path| path != dragged);
    let at = order
        .iter()
        .position(|path| path == target)
        .unwrap_or(order.len());
    order.insert(at, dragged.to_path_buf());
    order
}

/// The auto-dock rule behind the inspector's responsive pane: right
/// when the window is wide, bottom when narrow, keep the current dock
/// in the hysteresis band between the two thresholds.
fn auto_dock_for(width: f32, current_bottom: bool) -> Option<bool> {
    if width >= 1024.0 {
        Some(false)
    } else if width < 950.0 {
        Some(true)
    } else {
        None
    }
    .filter(|bottom| *bottom != current_bottom)
}

/// Parse the shell's palette file: key=value hex lines, unknown or
/// broken lines ignored, missing keys keep the built-ins.
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

/// The accent presets in the picker: the built-in blue first, then
/// hues that hold up on a dark surface.
const ACCENT_PRESETS: [u32; 8] = [
    0x4f8cc9, 0x89b4fa, 0xa6da95, 0x8bd5ca, 0xc6a0f6, 0xf5a97f, 0xee99a0, 0xeed49f,
];

/// One recently-used file: where it lives and when we last opened it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RecentEntry {
    path: PathBuf,
    modified: i64,
}

/// The recently-used store: the freedesktop xbel other GTK apps write.
fn recent_xbel_path() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("recently-used.xbel"))
}

/// Days since 1970-01-01 from a civil date (Howard Hinnant's
/// days_from_civil); the arithmetic works for all of the common era.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// The inverse: a civil date from days since the epoch.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// ISO 8601 UTC ("2026-10-06T09:15:00Z") to unix seconds. Fractional
/// seconds and offsets are tolerated by taking the fields literally.
fn iso_to_secs(text: &str) -> Option<i64> {
    let text = text.trim().trim_end_matches('Z');
    let (date, time) = text.split_once('T')?;
    let time = time.split('.').next()?.trim_end_matches('Z');
    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;
    let mut time = time.split(':');
    let hour: i64 = time.next()?.parse().ok()?;
    let minute: i64 = time.next()?.parse().ok()?;
    let second: i64 = time.next().unwrap_or("0").parse().ok()?;
    Some(days_from_civil(year, month, day) * 86400 + hour * 3600 + minute * 60 + second)
}

/// Unix seconds back to ISO 8601 UTC, the shape GTK writes.
fn secs_to_iso(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Parse the bookmark entries out of an xbel document. Tolerant of
/// single-line or multi-line forms: each <bookmark ...> starts a
/// segment whose attributes run to the first '>'. Timestamps come
/// from the spec's `visited` ISO attribute, falling back to a unix
/// `timestamp` some writers use.
fn parse_xbel(text: &str) -> Vec<RecentEntry> {
    let mut out: Vec<RecentEntry> = Vec::new();
    for segment in text.split("<bookmark ").skip(1) {
        let Some((attrs, _)) = segment.split_once('>') else {
            continue;
        };
        let Some(href) = attr_value(attrs, "href") else {
            continue;
        };
        let Some(path) = href.strip_prefix("file://").map(uri_decode) else {
            continue;
        };
        let modified = attr_value(attrs, "visited")
            .and_then(|ts| iso_to_secs(&ts))
            .or_else(|| {
                attr_value(attrs, "timestamp").and_then(|ts| ts.parse::<i64>().ok())
            })
            .unwrap_or(0);
        // a file may appear more than once (appended by different
        // apps); keep the newest visit
        match out.iter_mut().find(|entry| entry.path == path) {
            Some(existing) => existing.modified = existing.modified.max(modified),
            None => out.push(RecentEntry {
                path,
                modified,
            }),
        }
    }
    out
}

/// One attribute out of a tag's attribute text: name="value".
fn attr_value(attrs: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = attrs.find(&needle)? + needle.len();
    let rest = &attrs[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Merge one open into the recency list: bump to front, newest
/// first, capped so our writes stay bounded (the store may hold
/// thousands of entries other apps put there).
fn merge_recent(
    mut entries: Vec<RecentEntry>,
    path: PathBuf,
    timestamp: i64,
    cap: usize,
) -> Vec<RecentEntry> {
    entries.retain(|entry| entry.path != path);
    entries.insert(0, RecentEntry {
        path,
        modified: timestamp,
    });
    entries.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.path.cmp(&b.path)));
    entries.truncate(cap);
    entries
}

/// Serialize recency entries back to the xbel shape GTK expects:
/// ISO `visited` on the bookmark, plus a bookmark:application child
/// carrying the same time in `modified`.
fn xbel_text(entries: &[RecentEntry]) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"no\"?>\n\
         <xbel version=\"1.0\"\n\
         \x20     xmlns:bookmark=\"http://www.freedesktop.org/standards/desktop-bookmarks\"\n\
         \x20     xmlns:mime=\"http://www.freedesktop.org/standards/thumbnail-module\">\n",
    );
    for entry in entries {
        let iso = secs_to_iso(entry.modified);
        out.push_str(&format!(
            "  <bookmark href=\"{}\" visited=\"{}\">\n    <info>\n      <metadata owner=\"http://freedesktop.org\">\n        <bookmark:applications>\n          <bookmark:application name=\"kuma-files\" exec=\"&apos;kuma-files&apos; %u\" modified=\"{}\" count=\"1\"/>\n        </bookmark:applications>\n      </metadata>\n    </info>\n  </bookmark>\n",
            xml_escape(&path_uri(&entry.path)),
            iso,
            iso,
        ));
    }
    out.push_str("</xbel>\n");
    out
}

/// The GTK bookmarks file: one file:// URI per line, optional custom
/// name after a space.
fn bookmarks_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("gtk-3.0/bookmarks"))
}

/// Parse bookmarks lines into (path, custom name) pairs. Non-file
/// URIs (sftp and friends from other apps) are kept out; we only pin
/// local dirs.
fn parse_bookmarks(text: &str) -> Vec<(PathBuf, Option<String>)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let (uri, name) = match line.split_once(' ') {
                Some((uri, name)) => (uri, Some(name.trim().to_string()).filter(|n| !n.is_empty())),
                None => (line, None),
            };
            if !uri.starts_with("file://") {
                return None;
            }
            let path = uri_decode(uri);
            Some((path, name))
        })
        .collect()
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
    ("Shift+F10", "menu (keyboard)"),
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
    ("Ctrl+Q", "close window"),
    ("Type", "filter; also searches subfolders"),
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
    fn archive_detection_and_stems() {
        assert_eq!(archive_kind("backup.tar.gz"), Some("tar.gz"));
        assert_eq!(archive_kind("PHOTO.ZIP"), Some("zip"));
        assert_eq!(archive_kind("readme.md"), None);
        assert_eq!(archive_stem("backup.tar.gz").as_deref(), Some("backup"));
        assert_eq!(archive_stem("site.tar.xz").as_deref(), Some("site"));
        assert_eq!(archive_stem("notes.zip").as_deref(), Some("notes"));
    }

    #[test]
    fn exec_field_codes_substitute() {
        let path = Path::new("/tmp/a b/f.txt");
        assert_eq!(
            exec_argv("gedit %F", path, "gedit"),
            vec!["gedit", "/tmp/a b/f.txt"]
        );
        // no field code means the path is appended
        assert_eq!(
            exec_argv("eogui --loose", path, "eogui"),
            vec!["eogui", "--loose", "/tmp/a b/f.txt"]
        );
        assert_eq!(exec_argv("%c %u", path, "Editor"), vec!["Editor", "/tmp/a b/f.txt"]);
    }

    #[test]
    fn mime_table_covers_common() {
        assert_eq!(mime_for_ext("png"), "image/png");
        assert_eq!(mime_for_ext("PDF"), "application/pdf");
        assert_eq!(mime_for_ext("???"), "application/octet-stream");
    }

    #[test]
    fn extract_and_compress_round_trip() {
        // only when tar exists (it does in the build container)
        if !have_tool("tar") {
            return;
        }
        let room = std::env::temp_dir().join(format!("koguma-test-{}", std::process::id()));
        fs::create_dir_all(&room).unwrap();
        let src = room.join("hello.txt");
        fs::write(&src, "hi there\n").unwrap();
        let archive = room.join("bundle.tar.gz");

        let made = Op::Compress {
            files: vec![src.clone()],
            archive: archive.clone(),
        };
        if let Err(err) = made.run() {
            panic!("compress failed: {err}");
        }
        assert!(archive.exists(), "archive should exist after compress");

        fs::remove_file(&src).unwrap();
        let op = Op::Extract {
            archive: archive.clone(),
            dest: room.join("bundle"),
        };
        op.run().unwrap();
        let restored = room.join("bundle/hello.txt");
        assert!(restored.exists(), "extract should restore the file");
        assert_eq!(fs::read_to_string(&restored).unwrap(), "hi there\n");

        let _ = fs::remove_dir_all(&room);
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

    #[test]
    fn mimeapps_creates_section_and_merges_defaults() {
        let path = std::env::temp_dir().join(format!(
            "koguma-mimeapps-{}-1.list",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);

        // fresh file: section + entry appear
        write_mimeapps(&path, "text/markdown", "app-a.desktop").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[Default Applications]"));
        assert!(text.contains("text/markdown=app-a.desktop"));

        // second write prepends our app and keeps the old default
        fs::write(&path, "[Default Applications]\ntext/markdown=old.desktop\n").unwrap();
        write_mimeapps(&path, "text/markdown", "app-b.desktop").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("text/markdown=app-b.desktop;old.desktop"));

        // other types and sections survive untouched
        fs::write(
            &path,
            "[Added Associations]\nx=y.desktop\n\n[Default Applications]\nimage/png=loupe.desktop\ntext/markdown=old.desktop\n",
        )
        .unwrap();
        write_mimeapps(&path, "text/markdown", "app-c.desktop").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[Added Associations]"));
        assert!(text.contains("x=y.desktop"));
        assert!(text.contains("image/png=loupe.desktop"));
        assert!(text.contains("text/markdown=app-c.desktop;old.desktop"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn zip_toggle_swaps_known_suffixes_only() {
        let mut dialog = CompressDialog {
            name: "crew.tar.gz".into(),
            cursor: 4,
            zip: false,
            zip_available: true,
        };
        Browser::set_zip_format(&mut dialog, true);
        assert_eq!(dialog.name, "crew.zip");
        assert!(dialog.zip);
        Browser::set_zip_format(&mut dialog, false);
        assert_eq!(dialog.name, "crew.tar.gz");

        // custom names without a known suffix stay as typed; Create
        // appends the chosen suffix at build time
        dialog.name = "backup 2026".into();
        dialog.cursor = 11;
        Browser::set_zip_format(&mut dialog, true);
        assert_eq!(dialog.name, "backup 2026");
        assert!(dialog.zip);
        Browser::set_zip_format(&mut dialog, false);
        assert_eq!(dialog.name, "backup 2026");
    }
    #[test]
    fn bookmarks_parse_custom_names_and_uris() {
        let text = "\
file:///home/martin/Documents
file:///home/martin/My%20Projects pen
sftp://remote/share skip-me
";
        let marks = parse_bookmarks(text);
        assert_eq!(marks.len(), 2, "non-file lines are kept out");
        assert_eq!(marks[0].0, PathBuf::from("/home/martin/Documents"));
        assert_eq!(marks[0].1, None);
        assert_eq!(marks[1].0, PathBuf::from("/home/martin/My Projects"));
        assert_eq!(marks[1].1, Some("pen".into()));
    }

    #[test]
    fn bookmarks_round_trip_through_the_file_format() {
        let dir = PathBuf::from("/home/martin/My Projects");
        let marks: Vec<(PathBuf, Option<String>)> = vec![
            (dir.clone(), Some("projects".to_string())),
            (PathBuf::from("/tmp"), None),
        ];
        let text = marks
            .iter()
            .map(|(path, name)| match name {
                Some(name) => format!("{} {name}", path_uri(path)),
                None => path_uri(path),
            })
            .collect::<Vec<_>>()
            .join("\n");
        let parsed = parse_bookmarks(&text);
        assert_eq!(parsed[0], (dir, Some("projects".into())));
        assert_eq!(parsed[1], (PathBuf::from("/tmp"), None));
    }

    #[test]
    fn xbel_parses_gtk_style_documents() {
        let text = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"no\"?>\n\
<xbel version=\"1.0\"\n\
      xmlns:bookmark=\"http://www.freedesktop.org/standards/desktop-bookmarks\">\n\
  <bookmark href=\"file:///home/martin/notes.md\" visited=\"2026-10-05T09:30:00Z\">\n\
    <info>\n\
      <metadata owner=\"http://freedesktop.org\">\n\
        <bookmark:application name=\"kuma-files\" exec=\"&apos;kuma-files&apos; %u\" modified=\"2026-10-05T09:30:00Z\" count=\"1\"/>\n\
      </metadata>\n\
    </info>\n\
  </bookmark>\n\
  <bookmark href=\"file:///home/martin/My%20Reports/q1.odt\" timestamp=\"1700000100\">\n\
    <info>\n\
      <metadata owner=\"http://freedesktop.org\"/>\n\
    </info>\n\
  </bookmark>\n\
</xbel>\n";
        let entries = parse_xbel(text);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, PathBuf::from("/home/martin/notes.md"));
        // 2026-10-05T09:30:00Z as unix seconds
        assert_eq!(entries[0].modified, 1791192600);
        assert_eq!(entries[1].path, PathBuf::from("/home/martin/My Reports/q1.odt"));
        // unix timestamps still parse for older writers
        assert_eq!(entries[1].modified, 1700000100);
    }

    #[test]
    fn iso_timestamps_round_trip() {
        for secs in [0i64, 1700000100, 1791287000, 253402300799] {
            assert_eq!(iso_to_secs(&secs_to_iso(secs)), Some(secs));
        }
        assert_eq!(iso_to_secs("not a date"), None);
        // gnome-shell writes fractional seconds
        assert_eq!(
            iso_to_secs("2026-05-23T21:55:35.626342Z"),
            iso_to_secs("2026-05-23T21:55:35Z")
        );
    }

    #[test]
    fn recent_merge_bumps_to_front_and_caps() {
        let a = PathBuf::from("/tmp/a.txt");
        let b = PathBuf::from("/tmp/b.txt");
        let c = PathBuf::from("/tmp/c.txt");
        let entries = merge_recent(Vec::new(), a.clone(), 1, 3);
        let entries = merge_recent(entries, b.clone(), 2, 3);
        let entries = merge_recent(entries, a.clone(), 9, 3);
        // a moved to the front, not duplicated
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, a);
        assert_eq!(entries[0].modified, 9);
        let entries = merge_recent(entries, c.clone(), 10, 3);
        let entries = merge_recent(entries, b.clone(), 11, 3);
        assert_eq!(entries.len(), 3, "cap holds");
        assert_eq!(entries[0].path, b);
        assert_eq!(entries[1].path, c, "ties and order fall out of the sort");
        // serializing and re-parsing gives the same list back
        let round = parse_xbel(&xbel_text(&entries));
        assert_eq!(round, entries);
    }

    #[test]
    fn uri_decode_undoes_path_uri() {
        let path = PathBuf::from("/tmp/opencode/my files/report 2026é.md");
        assert_eq!(uri_decode(&path_uri(&path)), path);
    }

    #[test]
    fn auto_dock_flips_with_hysteresis() {
        // wide: right; narrow: bottom; the band in between keeps the
        // current dock so a resting window cannot flicker
        assert_eq!(auto_dock_for(1200.0, true), Some(false));
        assert_eq!(auto_dock_for(1024.0, true), Some(false));
        assert_eq!(auto_dock_for(1000.0, true), None);
        assert_eq!(auto_dock_for(1000.0, false), None);
        assert_eq!(auto_dock_for(949.0, false), Some(true));
        assert_eq!(auto_dock_for(700.0, false), Some(true));
        // already on the target dock: nothing to change
        assert_eq!(auto_dock_for(1200.0, false), None);
        assert_eq!(auto_dock_for(700.0, true), None);
    }

    #[test]
    fn palette_parses_shell_key_values() {
        let text = "panel_bg=16151ef2\naccent=#89b4fa\ntext=e0e0e8\nbogus=zz\nfuture=0x123456\n";
        let palette = parse_palette(text);
        // alpha-carrying panel color: RGB is the top 24 bits, not the
        // bottom (the low-end mask once produced a bright blue rail)
        assert_eq!(palette.panel_bg, 0x16151e);
        assert_eq!(palette.accent, 0x89b4fa);
        assert_eq!(palette.text, 0xe0e0e8);
        // untouched keys keep the defaults
        assert_eq!(palette.text_dim, theme::Palette::default().text_dim);
    }

    #[test]
    fn hex_colors_accept_the_common_spellings() {
        assert_eq!(parse_hex_color("#89b4fa"), Some(0x89b4fa));
        assert_eq!(parse_hex_color("89b4fa"), Some(0x89b4fa));
        assert_eq!(parse_hex_color("0x89B4FA"), Some(0x89b4fa));
        assert_eq!(parse_hex_color("181825f2"), Some(0x181825f2), "the shell's alpha-carrying panel colors");
        assert_eq!(parse_hex_color("auto"), None);
        assert_eq!(parse_hex_color("#89b4"), None);
        assert_eq!(parse_hex_color("#89b4faa"), None);
    }

    #[test]
    fn accent_change_drags_the_tints_along() {
        // green accent: the selection tints keep the accent's hue
        theme::set_accent(0xa6da95);
        let tint = theme::rgb_to_hsl(theme::row_selected_hex() & 0xFFFFFF);
        assert!((tint.0 - 100.0).abs() < 30.0, "green hue, got {}", tint.0);
        // back to the default accent, the original blues return
        theme::set_accent(0x4f8cc9);
        let tint = theme::rgb_to_hsl(theme::row_selected_hex() & 0xFFFFFF);
        assert!((tint.0 - 215.0).abs() < 8.0, "blue hue, got {}", tint.0);
    }

    #[test]
    fn place_reorder_takes_the_target_slot() {
        let home = PathBuf::from("/home/u");
        let games = PathBuf::from("/home/u/Games");
        let docs = PathBuf::from("/home/u/Documents");
        let music = PathBuf::from("/home/u/Music");
        let live = vec![home.clone(), docs.clone(), music.clone(), games.clone()];

        // dragging Games onto Documents puts Games before Documents
        let order = apply_place_order(Vec::new(), &live, &games, &docs);
        assert_eq!(
            order,
            vec![home.clone(), games.clone(), docs.clone(), music.clone()]
        );

        // reordering is idempotent: saved order plus live list keeps
        // the arrangement across restarts
        let again = apply_place_order(order.clone(), &live, &games, &docs);
        assert_eq!(again, order);

        // dropping a place on itself changes nothing
        assert_eq!(apply_place_order(order.clone(), &live, &games, &games), order);

        // a mount that disappeared is forgotten, a new one appends
        let mounts_gone = vec![home.clone(), docs.clone()];
        let order = apply_place_order(order, &mounts_gone, &music, &docs);
        assert_eq!(order, vec![home.clone(), music.clone(), docs.clone()]);
        let with_new = vec![
            home.clone(),
            docs.clone(),
            games.clone(),
            PathBuf::from("/run/media/u/USB"),
        ];
        let order = apply_place_order(order, &with_new, &games, &docs);
        assert_eq!(
            order,
            vec![
                home.clone(),
                games,
                docs,
                PathBuf::from("/run/media/u/USB")
            ]
        );
    }
}

/// Minimal harness for the menu dispatch question: does a left click
/// land on an item inside an occluded overlay, or does it fall through
/// to the listing underneath?
#[cfg(test)]
mod overlay_click_repro {
    use super::*;
    use gpui::{point, TestApp};
    use std::rc::Rc;
    use std::cell::Cell;

    struct OverlayRepro {
        menu_open: bool,
        item_clicked: Rc<Cell<bool>>,
        catcher_clicked: Rc<Cell<bool>>,
        backdrop_closed: Rc<Cell<bool>>,
    }

    impl Render for OverlayRepro {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let catcher_clicked = self.catcher_clicked.clone();
            let backdrop_closed = self.backdrop_closed.clone();
            let open_menu = cx.listener(|this, _: &MouseDownEvent, _, cx| {
                this.menu_open = true;
                cx.notify();
            });
            let click_item = cx.listener(|this, _, _, cx| {
                this.item_clicked.set(true);
                cx.notify();
            });

            let mut root = div().size_full().child(
                div()
                    .id("list")
                    .size_full()
                    .bg(rgb(0x222222))
                    .on_mouse_down(MouseButton::Right, open_menu)
                    .on_mouse_down(MouseButton::Left, move |_, _, _| {
                        catcher_clicked.set(true);
                    }),
            );

            if self.menu_open {
                root = root.child(
                    div()
                        .absolute()
                        .inset_0()
                        .occlude()
                        .child(div().absolute().inset_0().on_mouse_down(
                            MouseButton::Left,
                            move |_, _, _| backdrop_closed.set(true),
                        ))
                        .child(
                            div()
                                .absolute()
                                .left(px(100.))
                                .top(px(100.))
                                .w(px(200.))
                                .h(px(100.))
                                .flex()
                                .flex_col()
                                .bg(rgb(0x333333))
                                .child(
                                    div()
                                        .id("menu-item-0")
                                        .flex()
                                        .flex_col()
                                        .h(px(50.))
                                        .px_3()
                                        .py_1()
                                        .cursor_pointer()
                                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                            cx.stop_propagation()
                                        })
                                        .on_click(click_item)
                                        .child("Open"),
                                ),
                        ),
                );
            }
            root
        }
    }

    #[test]
    fn clicks_land_on_occluded_overlay_items() {
        let item_clicked = Rc::new(Cell::new(false));
        let catcher_clicked = Rc::new(Cell::new(false));
        let backdrop_closed = Rc::new(Cell::new(false));
        let mut app = TestApp::new();
        let mut window = app.open_window(|_, _| OverlayRepro {
            menu_open: false,
            item_clicked: item_clicked.clone(),
            catcher_clicked: catcher_clicked.clone(),
            backdrop_closed: backdrop_closed.clone(),
        });
        // open the menu with a right click where the listing is, like
        // the real row menu does
        window.simulate_mouse_down(point(px(400.), px(300.)), MouseButton::Right);
        let pos = point(px(150.), px(125.));
        // a real user moves the mouse onto the item before clicking
        window.simulate_mouse_move(pos);
        window.simulate_mouse_down(pos, MouseButton::Left);
        window.simulate_mouse_up(pos, MouseButton::Left);
        assert!(item_clicked.get(), "menu item click never fired");
        assert!(!catcher_clicked.get(), "click fell through to the listing");
        assert!(!backdrop_closed.get(), "backdrop closed the menu mid-click");
    }
}

/// A regression test for the dead-menu bug: the item loop was once
/// restructured and the `.on_click` fell off the row, leaving every
/// context-menu item uncapturable (hover worked, clicks did nothing).
/// Open a one-item menu in a live Browser, click it, expect Rename.
#[cfg(test)]
mod browser_menu_repro {
    use super::*;
    use gpui::point;

    #[test]
    fn menu_item_click_dispatches_action() {
        // a tiny real directory with one file to rename
        let dir = std::env::temp_dir().join(format!("koguma-menu-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("notes.txt"), "hello").unwrap();

        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = app.open_window(|window, cx| {
            Browser::new(Some(dir.clone()), window, cx)
        });
        // let the async directory load land before poking at entries
        app.run_until_parked();
        window.update(|browser, window, cx| {
            assert!(
                !browser.tab().entries.is_empty(),
                "directory listing never loaded in the test harness (dir: {:?})",
                browser.tab().current_dir()
            );
            // put the cursor on the file so Rename has a target, then
            // open the same menu the right-click would open
            browser.tab_mut().cursor = Some(0);
            browser.open_menu(
                100.,
                100.,
                vec![MenuItem::new("Rename", MenuAction::Rename)],
            );
            cx.notify();
            window.refresh();
        });

        // where does gpui think the item is? (debug_selector on the row
        // feeds this; the harness clicks the item's own painted bounds)
        let pos = window.update(|_, window, _| {
            let bounds = window
                .debug_element_bounds("menu-item-row")
                .expect("menu item never painted");
            point(
                bounds.origin.x + bounds.size.width / 2.,
                bounds.origin.y + bounds.size.height / 2.,
            )
        });
        window.simulate_mouse_move(pos);
        window.simulate_mouse_down(pos, MouseButton::Left);
        window.simulate_mouse_up(pos, MouseButton::Left);
        app.run_until_parked();
        let renamed = window.update(|browser, _, _| browser.tab().renaming.is_some());
        assert!(renamed, "click on the item's own bounds never fired Rename");

        window.update(|browser, _, _| {
            assert!(
                browser.menu.is_none(),
                "menu did not close: item click never dispatched"
            );
            assert!(
                browser.tab().renaming.is_some(),
                "Rename action never ran"
            );
        });
        let _ = fs::remove_dir_all(&dir);
    }
}

/// Keyboard paths added late 2026: conflict dialog keys, paste into a
/// folder, and the keyboard-opened context menu. Runs a real Browser
/// against a real temp dir, same harness as browser_menu_repro.
#[cfg(test)]
mod browser_ux_keys {
    use super::*;
    use gpui::Keystroke;

    struct Lab {
        dir: PathBuf,
        src: PathBuf,
    }

    impl Lab {
        /// dir holds the browser's listing (a.txt, b.txt, c.txt, sub),
        /// src holds the clipboard payloads of the same names. The
        /// name keeps parallel tests out of each other's dirs.
        fn new(name: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("koguma-ux-{name}-{}", std::process::id()));
            let dir = base.join("dir");
            let src = base.join("src");
            fs::create_dir_all(&dir).unwrap();
            fs::create_dir_all(&src).unwrap();
            fs::write(dir.join("a.txt"), "old-a").unwrap();
            fs::write(dir.join("b.txt"), "old-b").unwrap();
            fs::write(dir.join("c.txt"), "old-c").unwrap();
            fs::create_dir_all(dir.join("sub")).unwrap();
            fs::write(src.join("a.txt"), "new-a").unwrap();
            fs::write(src.join("b.txt"), "new-b").unwrap();
            fs::write(src.join("c.txt"), "new-c").unwrap();
            Self { dir, src }
        }
    }

    impl Drop for Lab {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.dir.parent().unwrap());
        }
    }

    fn key(k: &str) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke::parse(k).unwrap(),
            is_held: false,
            prefer_character_input: false,
        }
    }

    fn open_browser(app: &mut gpui::TestApp, dir: &Path) -> gpui::TestAppWindow<Browser> {
        let window =
            app.open_window(|window, cx| Browser::new(Some(dir.to_path_buf()), window, cx));
        app.run_until_parked();
        window
    }

    #[test]
    fn paste_into_folder_targets_the_given_dir() {
        let lab = Lab::new("paste-into");
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            browser.clipboard = Some((true, vec![lab.src.join("a.txt")]));
            browser.paste_into(lab.dir.join("sub"), cx);
            assert!(browser.conflict_dialog.is_none(), "empty target dir must not conflict");
        });
        app.run_until_parked();
        assert_eq!(
            fs::read_to_string(lab.dir.join("sub").join("a.txt")).unwrap(),
            "new-a"
        );
    }

    #[test]
    fn conflict_dialog_keyboard_decide_and_toggle() {
        let lab = Lab::new("conflict-keys");
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);

        // "a" toggles apply-to-all without deciding
        window.update(|browser, _, cx| {
            browser.clipboard = Some((true, vec![lab.src.join("a.txt")]));
            browser.paste(cx);
            assert!(browser.conflict_dialog.is_some());
            browser.route_key(&key("a"), cx);
            assert!(browser.conflict_dialog.as_ref().unwrap().apply_all);
        });

        // enter = Replace, so dir/a.txt ends up with the new content
        window.update(|browser, _, cx| browser.route_key(&key("enter"), cx));
        app.run_until_parked();
        assert_eq!(fs::read_to_string(lab.dir.join("a.txt")).unwrap(), "new-a");

        // "s" skips: dir/b.txt keeps its content
        window.update(|browser, _, cx| {
            browser.clipboard = Some((true, vec![lab.src.join("b.txt")]));
            browser.paste(cx);
            assert!(browser.conflict_dialog.is_some());
            browser.route_key(&key("s"), cx);
        });
        app.run_until_parked();
        assert_eq!(fs::read_to_string(lab.dir.join("b.txt")).unwrap(), "old-b");

        // "k" keeps both: the collision lands beside it under a new name
        window.update(|browser, _, cx| {
            browser.clipboard = Some((true, vec![lab.src.join("c.txt")]));
            browser.paste(cx);
            browser.route_key(&key("k"), cx);
        });
        app.run_until_parked();
        assert_eq!(fs::read_to_string(lab.dir.join("c.txt")).unwrap(), "old-c");
        assert_eq!(
            fs::read_to_string(lab.dir.join("c (copy).txt")).unwrap(),
            "new-c"
        );
    }

    #[test]
    fn keyboard_menu_uses_the_cursor_row() {
        let lab = Lab::new("kb-menu");
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, window, cx| {
            let sub_ix = browser
                .tab()
                .entries
                .iter()
                .position(|e| e.name == "sub")
                .expect("sub dir not in the listing");
            browser.tab_mut().cursor = Some(sub_ix);
            browser.open_cursor_menu(window, cx);
            let menu = browser.menu.as_ref().expect("cursor menu never opened");
            assert_eq!(menu.items[0].label, "Open");
            let paste = menu
                .items
                .iter()
                .find(|item| item.label == "Paste Into Folder")
                .expect("dir row menu must offer Paste Into Folder");
            assert!(paste.dim, "empty clipboard should render the item dim");
            assert_eq!(
                match &paste.action {
                    MenuAction::PasteInto(dir) => Some(dir.clone()),
                    _ => None,
                },
                Some(lab.dir.join("sub"))
            );
        });
    }

    #[test]
    fn row_menu_items_offer_paste_into_folder_only_for_dirs() {
        let file = Path::new("/tmp/koguma-nothing.txt");
        let items = Browser::row_menu_items(false, false, file, false, true);
        assert!(!items.iter().any(|item| item.label == "Paste Into Folder"));

        let items = Browser::row_menu_items(true, false, file, false, false);
        let paste = items
            .iter()
            .find(|item| item.label == "Paste Into Folder")
            .expect("dir row menu must offer Paste Into Folder");
        assert!(paste.dim, "no clipboard: dim");
    }
}

/// Recursive search: the walker's tree semantics as pure tests, plus
/// a live-Browser run of filter_changed end to end.
#[cfg(test)]
mod browser_search {
    use super::*;
    use gpui::TestApp;

    struct Tree {
        root: PathBuf,
    }

    impl Tree {
        /// needle.txt/needle.md at the root, one a folder down, one
        /// exactly at the depth cap, one past it, and one under a
        /// hidden dir. Six nested dirs deep at the cap.
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("koguma-search-{name}-{}", std::process::id()));
            let deep = root
                .join("d1")
                .join("d2")
                .join("d3")
                .join("d4")
                .join("d5")
                .join("d6");
            fs::create_dir_all(&deep).unwrap();
            fs::create_dir_all(deep.join("d7")).unwrap();
            fs::create_dir_all(root.join("sub")).unwrap();
            fs::create_dir_all(root.join(".hidden")).unwrap();
            fs::write(root.join("needle.txt"), "x").unwrap();
            fs::write(root.join("needle.md"), "x").unwrap();
            fs::write(root.join("sub").join("needle.txt"), "x").unwrap();
            fs::write(deep.join("needle.txt"), "x").unwrap();
            fs::write(deep.join("d7").join("needle.txt"), "x").unwrap();
            fs::write(root.join(".hidden").join("needle.txt"), "x").unwrap();
            Self { root }
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn walker_finds_matches_depth_capped_and_skips_hidden() {
        let tree = Tree::new("walker");
        let hits = search_subtree(&tree.root, "needle", false);
        let found: Vec<(String, String)> = hits
            .iter()
            .map(|entry| {
                (
                    entry.name.clone(),
                    entry.rel.clone().unwrap_or_default(),
                )
            })
            .collect();
        // root-level rows are the flat listing's job: the walker must
        // not duplicate them
        assert_eq!(
            found
                .iter()
                .filter(|(name, _)| name == "needle.txt" || name == "needle.md")
                .count(),
            2,
            "root rows leaked into the walker: {found:?}"
        );
        // the sub-folder hit and the depth-cap hit are both in
        assert!(
            found.contains(&("needle.txt".into(), "sub".into())),
            "sub hit missing: {found:?}"
        );
        assert!(
            found.contains(&(
                "needle.txt".into(),
                "d1/d2/d3/d4/d5/d6".into()
            )),
            "depth-cap hit missing: {found:?}"
        );
        // one past the cap, and the hidden dir: excluded
        assert_eq!(found.len(), 2, "unexpected extra hits: {found:?}");
    }

    #[test]
    fn walker_respects_show_hidden() {
        let tree = Tree::new("hidden");
        let hits = search_subtree(&tree.root, "needle", true);
        assert!(
            hits.iter()
                .any(|entry| entry.rel.as_deref() == Some(".hidden")),
            "hidden dir skipped with show_hidden on"
        );
    }

    #[test]
    fn filter_change_lands_deep_rows_and_escape_clears() {
        let tree = Tree::new("live");
        let mut app = TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = app.open_window(|window, cx| {
            Browser::new(Some(tree.root.clone()), window, cx)
        });
        app.run_until_parked();
        window.update(|browser, _, cx| {
            browser.filter = "needle".into();
            browser.filter_changed(cx);
        });
        app.run_until_parked();
        window.update(|browser, _, _| {
            let deep: Vec<&Entry> =
                browser.tab().entries.iter().filter(|e| e.rel.is_some()).collect();
            assert_eq!(deep.len(), 2, "walker results never landed");
            assert!(browser.tab().entries.iter().any(|e| e.path == tree.root.join("sub").join("needle.txt")));
        });
        // esc clears the filter and prunes the deep rows
        window.update(|browser, _, cx| {
            browser.filter.clear();
            browser.filter_changed(cx);
        });
        app.run_until_parked();
        window.update(|browser, _, _| {
            assert!(
                browser.tab().entries.iter().all(|e| e.rel.is_none()),
                "deep rows survived esc"
            );
            assert!(browser.filter.is_empty());
        });
    }
}
