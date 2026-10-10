use std::collections::{HashSet, VecDeque};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::{collections::HashMap, env, fs, io};

use std::io::{BufRead as _, Write as _};

use gpui::{
    AnyElement, App, AppContext, Bounds, ClickEvent, ClipboardItem, Context, Div, DragMoveEvent,
    ExternalDragPayload, ExternalPaths, FileDragPaths, FocusHandle, Focusable, Font,
    HighlightStyle, ImageSource, KeyDownEvent, MouseDownEvent, MouseMoveEvent, MouseButton,
    MouseUpEvent, ObjectFit, Pixels, Point, Render, RenderImage, Rgba, ScrollWheelEvent, Stateful,
    StyledText, TextRun, UnderlineStyle, Window, div, img, list, point, prelude::*, px, relative,
    rgba, rgb, svg, uniform_list, FontStyle, FontWeight, SharedString,
};
use trash::{os_limited, TrashItem};
use std::sync::atomic::{AtomicUsize, Ordering};

use notify::Watcher as _;
use std::os::unix::fs::PermissionsExt;

use crate::input;

/// Thumbnail cache bound. 256x256 RGBA each, so ~300 thumbs is the
/// most memory the cache can hold (about 75 MiB worst case).
const THUMB_CACHE_MAX: usize = 300;

/// The grid thumb decode pool: at most this many decodes in flight
/// (the kumaOS ffmpeg contract's 2 to 4 worker pool), whatever the
/// kind: libav posters, pdftocairo pages, raw image files.
const THUMB_POOL: usize = 3;

/// Disk cache entries older than this get swept on startup; the key
/// carries (path, mtime, size), so stale entries can never be read
/// again anyway and are pure disk weight.
const THUMB_CACHE_SWEEP_SECS: u64 = 30 * 24 * 3600;

/// Insertion-order trim for the thumb cache: evicts down to the cap,
/// oldest first (dropping the oldest keeps the visible window's thumbs
/// warm, where a wholesale clear re-decoded everything at once), and
/// returns the evicted images so the caller drops their atlas tiles:
/// the img element never drops the tile it paints, so a heap-only
/// eviction would leave every thumbnail ever scrolled past painted
/// until the window closed (ADR-0016).
fn trim_thumbs(
    thumbs: &mut HashMap<PathBuf, Arc<RenderImage>>,
    order: &mut VecDeque<PathBuf>,
) -> Vec<Arc<RenderImage>> {
    let mut droplets = Vec::new();
    while thumbs.len() >= THUMB_CACHE_MAX {
        let Some(oldest) = order.pop_front() else {
            droplets.extend(thumbs.drain().map(|(_, image)| image));
            break;
        };
        if let Some(image) = thumbs.remove(&oldest) {
            droplets.push(image);
        }
    }
    droplets
}

/// One grid thumb job: the disk cache first (a PNG of the fitted
/// pixels, keyed (path, mtime, size)), the source decode second,
/// with the result written back before it lands. Returns the BGRA
/// `RenderImage` ready for the thumbs map. Runs off the UI thread,
/// inside the caller's catch_unwind.
fn thumb_job(path: &Path, cache_path: &Path) -> Option<gpui::RenderImage> {
    if let Ok(bytes) = fs::read(cache_path)
        && let Some(image) = image::load_from_memory(&bytes).ok()
    {
        return Some(icons::decode_to_render(image, 256, 256));
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let dynamic = if icons::is_pdf(&name) {
        icons::decode_pdf_dynamic(path, 1, 256)
    } else if icons::is_epub(&name) {
        // a pathological cover stays sane: cap the bytes handed to
        // the decoder
        crate::epub::read_epub_meta(path).and_then(|meta| {
            let bytes = meta.cover?;
            (bytes.len() <= 16 * 1024 * 1024)
                .then_some(bytes)
                .and_then(|bytes| icons::decode_cover_dynamic(&bytes))
        })
    } else if icons::is_video(&name) {
        crate::video::poster_dynamic(path, 256).ok()
    } else {
        icons::decode_thumbnail_dynamic(path)
    }?;
    // the cache stores the fitted pixels, not the raw decode: a
    // 12 MP photo would balloon the cache dir. Pdfs, epub covers,
    // and video posters arrive fitted already; fitting a fitted
    // image is a no-op (never enlarged).
    let fitted = if dynamic.width() > 256 || dynamic.height() > 256 {
        dynamic.thumbnail(256, 256)
    } else {
        dynamic
    };
    // best effort: the cache is a durability win, not a dependency
    let _ = write_thumb_cache(cache_path, &fitted);
    Some(icons::decode_to_render(fitted, 256, 256))
}

/// PNG the fitted pixels into the cache root, creating it as needed.
fn write_thumb_cache(cache_path: &Path, image: &image::DynamicImage) -> std::io::Result<()> {
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut bytes = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    fs::write(cache_path, bytes)
}

/// The disk cache filename for a thumbable path: a hash over the
/// (path, mtime seconds, byte size) triple, so a changed file never
/// reads a stale PNG. std's hasher is only stable within a build: a
/// toolchain change re-decodes once and repopulates, which is
/// harmless.
fn thumb_cache_key(path: &Path, mtime: u64, size: u64) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.as_os_str().hash(&mut hasher);
    mtime.hash(&mut hasher);
    size.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Startup sweep: disk cache entries older than the age bound are
/// unreachable by their keys' nature only when their files change,
/// which resets the key anyway; anything this old is pure disk
/// weight. Best effort.
fn sweep_thumb_cache(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let cutoff = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(THUMB_CACHE_SWEEP_SECS))
        .unwrap_or(0);
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .ok()
            .and_then(|m| {
                m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            })
            .map(|modified| modified.as_secs() < cutoff)
            .unwrap_or(false);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// The details pane's video poster slot: the fitted frame, or the
/// named codec-missing state (the portability rule: a missing codec
/// is honest degradation, not a generic failure).
#[derive(Clone)]
enum VideoPoster {
    Frame(Arc<gpui::RenderImage>),
    CodecMissing,
}

/// Undo memory bound: individual file operations, not steps, so a
/// thousand-file paste is a thousand undo records.
const UNDO_MAX_OPS: usize = 1000;

/// Quick Look's decode bound: page-style files decode once at glance
/// scale (the listing's 256px thumbs are too small for a pane). Zoom
/// past the decoded pixels goes soft; re-decode-on-zoom is out of
/// scope (see #34).
pub(crate) const QL_DECODE_MAX: u32 = 2048;

/// Quick Look's zoom bounds around the fit: 1.0 is fit, 6x is as deep
/// as a glance needs.
pub(crate) const QL_ZOOM_MAX: f32 = 6.0;

/// The whole-file read for the text reading pane caps here; past it
/// the file shows its head and a tail marker says so honestly.
const TEXT_READ_MAX: usize = 2 * 1024 * 1024;

/// One line of the reading pane caps here: a single enormous line
/// must not become one enormous layout.
const TEXT_LINE_MAX: usize = 4096;

/// Contain-scale for the Quick Look pane: the image fills the smaller
/// ratio, never crops. Degenerate sizes read as "already fit".
pub(crate) fn ql_fit_scale(natural: (f32, f32), avail: (f32, f32)) -> f32 {
    if natural.0 <= 0.0 || natural.1 <= 0.0 || avail.0 <= 0.0 || avail.1 <= 0.0 {
        return 1.0;
    }
    (avail.0 / natural.0).min(avail.1 / natural.1)
}

/// Pan clamps so a zoomed image still covers the pane: at most half
/// the overflow to each side, none when fit.
pub(crate) fn ql_clamp_pan(pan: (f32, f32), display: (f32, f32), avail: (f32, f32)) -> (f32, f32) {
    let max_x = ((display.0 - avail.0) / 2.0).max(0.0);
    let max_y = ((display.1 - avail.1) / 2.0).max(0.0);
    (pan.0.clamp(-max_x, max_x), pan.1.clamp(-max_y, max_y))
}

use crate::{epub, icons, theme};

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
    /// the in-place rename editor, live while `renaming` is Some
    rename_field: input::Field,
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
    /// a gvfs-FUSE bridge or udisks mount point: lives under the
    /// Network or Removable header, not pinnable (it appears and
    /// disappears on its own)
    mount: Option<MountKind>,
    /// a bookmark into the gvfs root whose mount is gone: renders
    /// dimmed, and clicking opens the connect dialog prefilled with
    /// the URI parsed from the gvfs dir name
    stale: bool,
}

/// What a mount place unplugs as. Both ride `gio mount -u`; the
/// distinction is the verb the menu shows and the status line speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MountKind {
    /// a gvfs-FUSE bridge for a network protocol (samba, sftp, webdav)
    Network,
    /// a udisks2 mount point or a local-device gvfs backend
    /// (USB drives, MTP phones, cameras, iPods)
    Removable,
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
    /// Disconnect a network mount or eject a removable one:
    /// `gio mount -u` on the mount point; the kind picks the verb.
    Unmount(PathBuf, MountKind),
    /// A stale network place: open the connect dialog prefilled with
    /// the URI parsed from the gvfs dir name.
    Reconnect(PathBuf),
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

/// Compress dialog: target archive name, format choice, and whether
/// the zip option has a tool behind it on this machine.
struct CompressDialog {
    name: input::Field,
    zip: bool,
    zip_available: bool,
}

/// Connect-to-server state: the protocol picked and its fields
/// before the mount starts, then whatever the gio pump is asking for.
struct ConnectDialog {
    proto: ConnectProto,
    /// the per-protocol fields, indexed by FIELD_*; which rows show
    /// depends on the protocol
    fields: [input::Field; 5],
    /// which field the caret sits in (an index into fields)
    focus: usize,
    /// the composed URI, set when the pump starts; the locked server
    /// line during the prompt phase
    uri: String,
    /// credential buffer for the current prompt
    input: input::Field,
    /// the prompt gio printed ("User", "Password", "Domain [X]")
    prompt: Option<String>,
    /// password-style prompts render the buffer as bullets
    mask: bool,
    status: String,
    /// gio's non-prompt output (identity text, [1]/[2] choices,
    /// errors), newest last
    notes: Vec<String>,
    /// when the current gio run started, for the elapsed counter
    since: Option<std::time::Instant>,
    session: Option<ConnectSession>,
    /// the form's password already answered one prompt this run, so
    /// the next password prompt (it was wrong) surfaces for typing
    password_tried: bool,
}

/// The field rows the dialog shows, in tab order. SMB mounts a share
/// (no port); sftp and ftp take an optional port and no share; every
/// protocol takes an optional password that pre-answers gio's first
/// password prompt.
const FIELD_HOST: usize = 0;
const FIELD_USER: usize = 1;
const FIELD_PORT: usize = 2;
const FIELD_SHARE: usize = 3;
const FIELD_PASSWORD: usize = 4;

impl ConnectDialog {
    /// The visible field indexes, in tab order.
    fn layout(&self) -> [usize; 4] {
        match self.proto {
            ConnectProto::Smb => [FIELD_HOST, FIELD_USER, FIELD_SHARE, FIELD_PASSWORD],
            ConnectProto::Sftp | ConnectProto::Ftp => {
                [FIELD_HOST, FIELD_USER, FIELD_PORT, FIELD_PASSWORD]
            }
        }
    }

    fn focused_mut(&mut self) -> &mut input::Field {
        &mut self.fields[self.focus]
    }

    /// Move the caret one visible field back or forward (wrapping).
    fn cycle(&mut self, back: bool) {
        let layout = self.layout();
        let pos = layout.iter().position(|&ix| ix == self.focus).unwrap_or(0);
        self.fields[self.focus].collapse();
        self.focus = layout[if back { (pos + 3) % 4 } else { (pos + 1) % 4 }];
    }

    fn last_visible(&self) -> usize {
        self.layout()[3]
    }
}

/// The protocols the dialog speaks. Blossom rides here later, as its
/// own picker entry with its own fields (issue #25 phase 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectProto {
    Sftp,
    Ftp,
    Smb,
}

impl ConnectProto {
    const ALL: [ConnectProto; 3] = [ConnectProto::Sftp, ConnectProto::Ftp, ConnectProto::Smb];

    fn scheme(&self) -> &'static str {
        match self {
            Self::Sftp => "sftp",
            Self::Ftp => "ftp",
            Self::Smb => "smb",
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Sftp => "SFTP",
            Self::Ftp => "FTP",
            Self::Smb => "SMB",
        }
    }
}

/// The URI the dialog's fields compose to: scheme from the protocol,
/// user@ when given, :port for sftp/ftp (digits only), /share for smb.
fn compose_server_uri(
    proto: ConnectProto,
    host: &str,
    user: &str,
    port: &str,
    share: &str,
) -> String {
    let mut uri = format!("{}://", proto.scheme());
    let user = user.trim();
    if !user.is_empty() {
        uri.push_str(user);
        uri.push('@');
    }
    uri.push_str(host.trim());
    let port = port.trim();
    if proto != ConnectProto::Smb
        && !port.is_empty()
        && port.bytes().all(|b| b.is_ascii_digit())
    {
        uri.push(':');
        uri.push_str(port);
    }
    if proto == ConnectProto::Smb {
        let share = share.trim().trim_start_matches('/');
        if !share.is_empty() {
            uri.push('/');
            uri.push_str(share);
        }
    }
    uri
}

/// Split a pasted (or reconnected) server URI into protocol and
/// fields. A pasted password is dropped: gio re-asks through the
/// pump. None when the scheme is not one the dialog speaks.
fn parse_server_uri(uri: &str) -> Option<(ConnectProto, String, String, String, String)> {
    let (scheme, rest) = uri.split_once("://")?;
    let proto = match scheme.to_lowercase().as_str() {
        "smb" => ConnectProto::Smb,
        "sftp" => ConnectProto::Sftp,
        "ftp" => ConnectProto::Ftp,
        _ => return None,
    };
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, path),
        None => (rest, ""),
    };
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((userinfo, hostport)) => (userinfo, hostport),
        None => ("", authority),
    };
    // a pasted password rides after ':' in the userinfo; drop it
    let user = match userinfo.split_once(':') {
        Some((user, _)) => user.to_string(),
        None => userinfo.to_string(),
    };
    // bracketed IPv6 hosts: the brackets do not compose back
    let (host, port) = if let Some(inner) = hostport.strip_prefix('[') {
        match inner.split_once(']') {
            Some((host, tail)) => (host.to_string(), tail.trim_start_matches(':').to_string()),
            None => (hostport.to_string(), String::new()),
        }
    } else {
        match hostport.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), port.to_string()),
            None => (hostport.to_string(), String::new()),
        }
    };
    Some((
        proto,
        host,
        user,
        port,
        path.trim_start_matches('/').to_string(),
    ))
}

/// The live `gio mount` pump: answers go down the channel, the child
/// handle is kept for Cancel (kill).
struct ConnectSession {
    answers: std::sync::mpsc::Sender<String>,
    child: Arc<Mutex<Option<std::process::Child>>>,
}

/// What the gio pump reports back to the view.
#[derive(Debug)]
enum ConnectEvent {
    /// gio is asking for a credential ("User", "Password", "Domain [W]")
    Prompt { text: String, mask: bool },
    /// non-prompt output lines, kept for the error message
    Note(String),
    /// process exited; mount is the new gvfs entry when it succeeded
    Done {
        ok: bool,
        message: String,
        mount: Option<PathBuf>,
    },
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

/// A gvfs mount directory name like `sftp:host=server,user=bob` becomes
/// a human place label: `server (sftp)`, `server/share (smb)`. Names
/// without a parseable scheme shape pass through unchanged.
fn mount_label(raw: &str) -> String {
    let Some((scheme, rest)) = raw.split_once(':') else {
        return raw.to_string();
    };
    // the smb backend's pseudo-scheme reads plainly in a label
    let scheme = if scheme == "smb-share" {
        "smb"
    } else {
        scheme
    };
    let mut host = None;
    let mut share = None;
    for part in rest.split(',') {
        if let Some((key, value)) = part.split_once('=') {
            match key {
                // the smb backend names its mounts
                // `smb-share:server=X,share=Y`; the others use host=
                "host" | "server" if !value.is_empty() => host = Some(value),
                "share" if !value.is_empty() => share = Some(value),
                _ => {}
            }
        }
    }
    match (host, share) {
        (Some(host), Some(share)) => format!("{host}/{share} ({scheme})"),
        (Some(host), None) => format!("{host} ({scheme})"),
        _ => raw.to_string(),
    }
}

/// Is this path inside the session's gvfs-FUSE root?
fn is_gvfs_path(path: &std::path::Path) -> bool {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|runtime| PathBuf::from(runtime).join("gvfs"))
        .is_some_and(|gvfs| path.starts_with(&gvfs))
}

/// Parse a gvfs-FUSE mount directory name back into the server URI a
/// reconnect needs: `smb-share:server=X,share=Y` is `smb://X/Y`,
/// `sftp:host=server,user=bob` is `sftp://bob@server`. The backend
/// appends `;N` when the same URI mounts twice; digits after a `;`
/// are stripped. None for names without a network host.
fn uri_from_gvfs_name(raw: &str) -> Option<String> {
    let (scheme, rest) = raw.split_once(':')?;
    let scheme = if scheme == "smb-share" {
        "smb"
    } else {
        scheme
    };
    let mut host = None;
    let mut share = None;
    let mut user = None;
    for part in rest.split(',') {
        if let Some((key, value)) = part.split_once('=') {
            // the backend's `;N` repeat counter rides the last value
            let value = match value.split_once(';') {
                Some((head, tail)) if tail.bytes().all(|b| b.is_ascii_digit()) => head,
                _ => value,
            };
            if value.is_empty() {
                continue;
            }
            match key {
                "host" | "server" => host = Some(value),
                "share" => share = Some(value),
                "user" => user = Some(value),
                _ => {}
            }
        }
    }
    let host = host?;
    let mut uri = format!("{scheme}://");
    if let Some(user) = user {
        uri.push_str(user);
        uri.push('@');
    }
    uri.push_str(host);
    if let Some(share) = share {
        uri.push('/');
        uri.push_str(share);
    }
    Some(uri)
}

/// Which sidebar section a place renders in. Home shares the top
/// block with Recent and Trash; the rest group under headers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaceSection {
    Top,
    Places,
    Network,
    Removable,
}

/// Network protocols mount under Network; local-device backends
/// (MTP phones, cameras, iPods) ride with the removable drives.
fn gvfs_mount_kind(raw: &str) -> MountKind {
    let scheme = raw.split(':').next().unwrap_or_default();
    match scheme {
        "smb-share" | "smb" | "sftp" | "ftp" | "ftps" | "webdav" | "dav" | "davs" | "afp"
        | "nfs" => MountKind::Network,
        _ => MountKind::Removable,
    }
}

fn place_section(place: &Place) -> PlaceSection {
    match place.mount {
        Some(MountKind::Network) => PlaceSection::Network,
        Some(MountKind::Removable) => PlaceSection::Removable,
        None if place.stale => PlaceSection::Network,
        None => {
            if is_home_dir(&place.path) {
                PlaceSection::Top
            } else {
                PlaceSection::Places
            }
        }
    }
}

fn is_home_dir(path: &std::path::Path) -> bool {
    dirs::home_dir().is_some_and(|home| home == path)
}

/// Is this gio output chunk (text up to a colon) a credential prompt?
/// Real prompts are bare words: `User: `, `Password: `, `Domain [X]: `.
/// Context lines like `Enter user and password for [host]:` end in a
/// colon too but carry prose, so only exact word prompts qualify.
fn is_prompt(text: &str) -> bool {
    let last = text.rsplit('\n').next().unwrap_or("").trim().to_lowercase();
    last == "user"
        || last == "login"
        || last == "choice"
        || last.starts_with("password")
        || last.starts_with("passphrase")
        || last.starts_with("domain")
}

/// Password-style prompts hide the typed characters in the dialog.
fn mask_prompt(text: &str) -> bool {
    let last = text.rsplit('\n').next().unwrap_or("").trim().to_lowercase();
    last.starts_with("password") || last.starts_with("passphrase")
}

/// Kill the pump's child through its cell; a taken cell means it is
/// already exiting under the pump's control.
fn child_cell_lock_kill(cell: &Mutex<Option<std::process::Child>>) {
    if let Ok(mut guard) = cell.lock() {
        if let Some(child) = guard.as_mut() {
            let _ = child.kill();
        }
    }
}

/// The gvfsd-fuse binary, if this system ships it. Fedora keeps it in
/// the separate gvfs-fuse package, which a minimal image can miss.
fn gvfs_fuse_bin() -> Option<String> {
    for candidate in ["/usr/libexec/gvfsd-fuse", "/usr/lib/gvfs/gvfsd-fuse"] {
        if fs::metadata(candidate).is_ok() {
            return Some(candidate.into());
        }
    }
    // unusual packaging: plain PATH scan, no probing (the daemon does
    // not have a help mode and must not be started here)
    // unusual packaging: plain PATH scan, no probing (the daemon does
    // not have a help mode and must not be started here)
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).find_map(|dir| {
                let candidate = dir.join("gvfsd-fuse");
                (fs::metadata(&candidate).is_ok())
                    .then(|| candidate.display().to_string())
            })
        })
        .unwrap_or(None)
}

/// Make sure the gvfs-FUSE bridge is running: it exposes mounted
/// network shares as POSIX paths under XDG_RUNTIME_DIR/gvfs, and
/// without it a mount succeeds at the DBus level while there is no
/// folder anywhere to browse. A desktop session usually starts it;
/// kumaOS (niri) has none, so Koguma starts it on demand. Safe to
/// call repeatedly: a mounted bridge is detected by device id.
fn ensure_gvfs_fuse() {
    if gvfs_fuse_bin().is_none() {
        return;
    }
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return;
    };
    let runtime = PathBuf::from(runtime);
    let gvfs = runtime.join("gvfs");
    use std::os::unix::fs::MetadataExt;
    if let (Ok(parent), Ok(mount)) = (fs::metadata(&runtime), fs::metadata(&gvfs)) {
        if parent.dev() != mount.dev() {
            return; // bridge already mounted
        }
    }
    let _ = fs::create_dir_all(&gvfs);
    let bin = gvfs_fuse_bin().unwrap_or_else(|| "gvfsd-fuse".into());
    match std::process::Command::new(bin).arg(&gvfs).spawn() {
        Ok(child) => {
            log::info!("connect: started gvfsd-fuse at {}", gvfs.display());
            // reaped on a thread: a dropped Child would zombie
            std::thread::spawn(move || {
                let mut child = child;
                let _ = child.wait();
            });
        }
        Err(err) => log::error!("connect: gvfsd-fuse failed to start: {err}"),
    }
}

/// Drive one `gio mount` child to completion: pipe its stdout through
/// the prompt classifier, relay prompts to the dialog and feed the
/// user's answers back down stdin, then diff the gvfs-FUSE directory
/// so the view learns where the new mount landed. Runs on the
/// background pool. The child sits in a cell so Cancel (another
/// thread) can kill it while this blocks on its stdout; a killed
/// child surfaces as EOF, a failed wait, and a Done with ok=false.
fn run_mount_process(
    mut stdin: std::process::ChildStdin,
    stdout: std::process::ChildStdout,
    stderr: std::process::ChildStderr,
    child: Arc<Mutex<Option<std::process::Child>>>,
    gvfs_dir: PathBuf,
    ev: mpsc::Sender<ConnectEvent>,
    answers: mpsc::Receiver<String>,
) {
    let snapshot = || {
        fs::read_dir(&gvfs_dir)
            .map(|read| {
                read.flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let before = snapshot();

    // gio reports failures on stderr (prompts ride stdout); a thread
    // relays each line live and the last one names the failure in the
    // Done message ("Hostname not known" instead of a bare "failed")
    let last_err = Arc::new(Mutex::new(String::new()));
    {
        let last_err = last_err.clone();
        let ev = ev.clone();
        std::thread::spawn(move || {
            let mut err = io::BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match err.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                *last_err.lock().unwrap() = line.clone();
                let _ = ev.send(ConnectEvent::Note(line));
            }
        });
    }

    let mut reader = io::BufReader::new(stdout);
    let mut carry = String::new();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b':', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let chunk = String::from_utf8_lossy(&buf);
        let ends_colon = chunk.ends_with(':');
        carry.push_str(chunk.trim_end_matches(':'));
        if ends_colon && is_prompt(&carry) {
            // the last fragment is the prompt; the lines above it are
            // context the user needs (identity text, [1]/[2] choices)
            let text = carry.rsplit('\n').next().unwrap_or("").trim().to_string();
            if let Some(pos) = carry.rfind('\n') {
                for line in carry[..pos].lines() {
                    if !line.trim().is_empty() {
                        let _ = ev.send(ConnectEvent::Note(line.to_string()));
                    }
                }
            }
            let mask = mask_prompt(&text);
            carry.clear();
            if ev.send(ConnectEvent::Prompt { text, mask }).is_err() {
                // the view is gone (window closed): kill the child
                let _ = child_cell_lock_kill(&child);
                return;
            }
            match answers.recv() {
                Ok(answer) => {
                    let _ = writeln!(stdin, "{answer}");
                }
                Err(_) => {
                    let _ = child_cell_lock_kill(&child);
                    return;
                }
            }
            continue;
        }
        // flush complete lines, keep the trailing fragment
        if let Some(pos) = carry.rfind('\n') {
            for line in carry[..pos].lines() {
                if !line.trim().is_empty() {
                    let _ = ev.send(ConnectEvent::Note(line.to_string()));
                }
            }
            carry.drain(..pos + 1);
        }
    }
    if !carry.trim().is_empty() {
        let _ = ev.send(ConnectEvent::Note(carry.trim().to_string()));
    }
    // reap: take the child out of the cell (Cancel may have killed it
    // already; a taken cell means it is exiting either way)
    let child = child
        .lock()
        .ok()
        .and_then(|mut cell| cell.take())
        .and_then(|mut child| child.wait().ok());
    let ok = child.map(|status| status.success()).unwrap_or(false);
    // give the stderr relay a beat to catch the final lines (they are
    // usually printed just before exit)
    if !ok {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // the new gvfs entry is the mount's FUSE path; a failed run
    // changes nothing
    let mount = if ok {
        let after = snapshot();
        after
            .iter()
            .find(|name| !before.contains(name))
            .map(|name| gvfs_dir.join(name))
    } else {
        None
    };
    let _ = ev.send(ConnectEvent::Done {
        ok,
        message: if ok {
            "connected".into()
        } else {
            let err = last_err.lock().unwrap().clone();
            if err.is_empty() {
                "connection failed".into()
            } else {
                err
            }
        },
        mount,
    });
}

pub(crate) struct Browser {
    tabs: Vec<Tab>,
    active: usize,
    clipboard: Option<(bool, Vec<PathBuf>)>,
    undo: Vec<Op>,
    conflict_dialog: Option<ConflictDialog>,
    connect: Option<ConnectDialog>,
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
    /// Quick Look's whole-file read for text-ish entries (capped;
    /// the rail keeps its 48-line snippet). Keyed by path; None
    /// while loading. Plain data, no tile.
    ql_text: Option<(PathBuf, TextPreview)>,
    /// The whole-file read in flight, if any.
    ql_text_inflight: Option<PathBuf>,
    /// The path whose whole-file read failed or smelled binary: the
    /// card stands in and the failed slot stops the respawn loop.
    ql_text_failed: Option<PathBuf>,
    /// The reading pane's scroll for the uniform line rows.
    ql_text_scroll: gpui::UniformListScrollHandle,
    /// The markdown reading pane's list state (item counts ride it).
    ql_md_state: gpui::ListState,
    /// Quick Look: the full-pane preview overlay. `None` when closed;
    /// while open it owns the keyboard (route_key's rung) and follows
    /// the cursor entry. `zoom` is 1.0 = fit; pan only matters zoomed.
    quicklook: Option<QuickLook>,
    /// The one held Quick Look decode, keyed by the path and (for
    /// PDFs) the page it belongs to. Dropped via `cx.drop_image` on
    /// close and flip: the img element never drops its own atlas
    /// tile (ADR-0016).
    ql_render: Option<(PathBuf, usize, Arc<RenderImage>)>,
    /// The decode in flight, if any.
    ql_inflight: Option<(PathBuf, usize)>,
    /// The path whose opening decode landed empty (corrupt file,
    /// missing pdftocairo), or whose codec this system's libav has
    /// no decoder for: stands in as the card so sync does not
    /// re-spawn the decode every frame.
    ql_failed: Option<(PathBuf, crate::video::PosterFail)>,
    /// pdfinfo has been asked for the current PDF's page count (one
    /// ask per open path; reset on file flips).
    ql_counting: bool,
    /// The open epub's metadata (title, author, cover flag), landed
    /// by the same background decode; reset on file flips.
    ql_book: Option<epub::BookMeta>,
    /// The open video's duration label, when the container header
    /// answered.
    ql_video_len: Option<(PathBuf, String)>,
    /// The container header has been read for the current video's
    /// duration (one ask per open path; reset on file flips). A
    /// header that will not read just leaves the label out.
    video_meta_asked: bool,
    /// The selected video's poster slot for the details pane, keyed
    /// by entry key like the text snippet. Grid and search previews
    /// keep type icons; Quick Look has its own pane-scale poster.
    sel_video: Option<(PathBuf, VideoPoster)>,
    /// The selected video's duration label, same keying.
    sel_video_len: Option<(PathBuf, String)>,
    /// The armed settle kick (key, path): the decode spawns only
    /// after the selection rests out the delay, and a spent kick
    /// stays armed so an absent tool cannot respawn per frame.
    sel_video_kick: Option<(PathBuf, PathBuf)>,
    /// Bumps on every selection change so a resting timer can tell
    /// it lost the race.
    sel_video_gen: usize,
    /// The open epub's reading pane: spine chapters decoded in order
    /// by one background chain, blocks appending as each lands.
    /// Plain data, no tile.
    ql_text_blocks: Arc<Vec<epub::ChapterBlock>>,
    /// Block index where each decoded chapter starts (chapter k is
    /// starts[k - 1]); a failed chapter's marker gets one too.
    ql_text_starts: Vec<usize>,
    /// The book the accumulation belongs to; set once starts the
    /// chain, cleared on flip kills it.
    ql_text_for: Option<PathBuf>,
    /// Where the reader is in an epub: 0 is the cover, k >= 1 is
    /// chapter k at the last jump or scroll.
    ql_book_at: usize,
    /// The epub reading pane's list state (item counts ride it).
    ql_book_state: gpui::ListState,
    /// The overlay's text column scrolls through the snippet.
    ql_scroll: gpui::ScrollHandle,
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
    /// Pending thumb kicks, FIFO: the pool pops from here as slots
    /// free, and pops die for paths the last paint stopped asking
    /// about (off-screen work dies).
    thumbs_queue: VecDeque<PathBuf>,
    /// The paths the current paint asked thumbnails for: refreshed
    /// every render, consulted when the pool pops the queue.
    thumb_wanted: HashSet<PathBuf>,
    /// The grid thumb disk cache root, keyed (path, mtime, size),
    /// PNGs of the fitted pixels. Survives restarts.
    thumb_cache: PathBuf,
    /// Insertion order into `thumbs`, so trimming drops the oldest
    /// entries instead of clearing the whole cache.
    thumb_order: VecDeque<PathBuf>,
    path_editing: bool,
    /// the path bar's editor, live while `path_editing`
    path_field: input::Field,
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
    /// A search walker is in flight; drives the banner's indicator.
    searching: bool,
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
            connect: None,
            compress: None,
            desktop_apps: None,
            openwith_apps: Vec::new(),
            inspector: true,
            inspector_bottom: true,
            inspector_lock: None,
            accent: None,
            palette_mtime: None,
            theme_refresh: Instant::now(),
            accent_picker: false,
            keys_open: false,
            text_preview: None,
            preview_key: None,
            preview_inflight: HashSet::new(),
            ql_text: None,
            ql_text_inflight: None,
            ql_text_failed: None,
            ql_text_scroll: gpui::UniformListScrollHandle::new(),
            ql_md_state: gpui::ListState::new(0, gpui::ListAlignment::Top, px(256.)),
            quicklook: None,
            ql_render: None,
            ql_inflight: None,
            ql_failed: None,
            ql_counting: false,
            ql_book: None,
            ql_video_len: None,
            video_meta_asked: false,
            sel_video: None,
            sel_video_len: None,
            sel_video_kick: None,
            sel_video_gen: 0,
            ql_text_blocks: Arc::new(Vec::new()),
            ql_text_starts: Vec::new(),
            ql_text_for: None,
            ql_book_at: 0,
            ql_book_state: gpui::ListState::new(0, gpui::ListAlignment::Top, px(256.)),
            ql_scroll: gpui::ScrollHandle::new(),
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
            thumbs_queue: VecDeque::new(),
            thumb_wanted: HashSet::new(),
            thumb_cache: dirs::cache_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("kuma/files"),
            thumb_order: VecDeque::new(),
            path_editing: false,
            path_field: Default::default(),
            scale: 1.25,
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
            searching: false,
        };
        // the epub reading pane's indicator follows the wheel: the
        // chapter under the visible top becomes the position, so the
        // label never lies about where the reader is
        let entity = cx.entity();
        browser.ql_book_state.set_scroll_handler(move |event, _, cx| {
            entity.update(cx, |browser, cx| {
                if browser.ql_book_at == 0 {
                    return;
                }
                let at = browser
                    .ql_text_starts
                    .iter()
                    .rposition(|&start| start <= event.visible_range.start)
                    .map_or(1, |i| i + 1);
                if at != browser.ql_book_at {
                    browser.ql_book_at = at;
                    cx.notify();
                }
            });
        });
        browser.load_state(cli_dir.as_deref());
        let show_hidden = browser.show_hidden;
        browser.tab_mut().reload(show_hidden);
        browser.palette_mtime = Browser::palette_mtime();
        browser.apply_theme();
        browser.places = browser.ordered_places();
        browser.start_dir_watch(cx);
        // the gvfs-FUSE bridge, so mounts made anywhere in the session
        // (ours or gio's CLI) turn into browsable folders
        ensure_gvfs_fuse();
        // the idle tick: palette republishes and mount changes land
        // without any input, and render cannot tick (an idle window
        // draws no frames). Check on a timer, wake the UI only when
        // something actually changed.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(2))
                    .await;
                let update = this.update(cx, |this, cx| {
                    let palette = this.refresh_palette(cx);
                    let places = this.refresh_places();
                    // a running connect shows its elapsed seconds: a
                    // silent server (slow banner, filtered port) looks
                    // alive instead of hung
                    let mut connect_tick = false;
                    if let Some(dialog) = this.connect.as_mut() {
                        if dialog.session.is_some() {
                            if let Some(since) = dialog.since {
                                let uri = dialog.uri.trim().to_string();
                                dialog.status =
                                    format!("connecting to {uri}… {}s", since.elapsed().as_secs());
                                connect_tick = true;
                            }
                        }
                    }
                    if palette || places || connect_tick {
                        cx.notify();
                    }
                });
                if update.is_err() {
                    return;
                }
            }
        })
        .detach();
        // stale disk cache entries are pure disk weight: sweep them
        // once per process start, in the background
        let cache_root = browser.thumb_cache.clone();
        cx.spawn(async move |_this, cx| {
            cx.background_spawn(async move { sweep_thumb_cache(&cache_root) })
                .await;
        })
        .detach();
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
                    mount: None,
                    stale: false,
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
        let mount_root = |root: PathBuf, classify: fn(&str) -> MountKind, places: &mut Vec<Place>| {
            let Ok(read) = fs::read_dir(&root) else {
                return;
            };
            let mut mounts: Vec<Place> = read
                .flatten()
                .map(|entry| {
                    let raw = entry.file_name().to_string_lossy().into_owned();
                    let kind = classify(&raw);
                    let name = match kind {
                        MountKind::Network => mount_label(&raw),
                        MountKind::Removable => raw,
                    };
                    Place {
                        name,
                        path: entry.path(),
                        bookmark: false,
                        mount: Some(kind),
                        stale: false,
                    }
                })
                .collect();
            mounts.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            places.extend(mounts);
        };
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
            let mut gvfs = PathBuf::from(&runtime);
            gvfs.push("gvfs");
            mount_root(gvfs, gvfs_mount_kind, &mut places);
        }
        if let Some(user) = std::env::var_os("USER") {
            let mut media = PathBuf::from("/run/media");
            media.push(user);
            mount_root(media, |_| MountKind::Removable, &mut places);
        }

        // pinned folders ride the GTK bookmarks file, so Thunar and
        // Nautilus agree with us about what is pinned; entries the
        // XDG dirs or mounts already cover are not repeated
        for (path, name) in Self::read_bookmarks() {
            let raw = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            // a bookmark into the gvfs root whose mount is gone
            // (reboot, disconnect): keep it, dimmed, as a reconnect
            // affordance, instead of letting the saved connection
            // vanish. Local dead bookmarks (moved folders) still drop.
            if !path.is_dir() {
                if !is_gvfs_path(&path)
                    || uri_from_gvfs_name(&raw).is_none()
                    || places.iter().any(|place| place.path == path)
                {
                    continue;
                }
                places.push(Place {
                    name: name.unwrap_or_else(|| mount_label(&raw)),
                    path,
                    bookmark: true,
                    mount: None,
                    stale: true,
                });
                continue;
            }
            if places.iter().any(|place| place.path == path) {
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
                mount: None,
                stale: false,
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
    /// roots at most every couple of seconds. Reports whether the
    /// sidebar changed, so the idle tick only wakes the renderer on
    /// real changes.
    fn refresh_places(&mut self) -> bool {
        if self.places_refresh.elapsed() < Duration::from_secs(2) {
            return false;
        }
        self.places_refresh = Instant::now();
        let next = self.ordered_places();
        let changed = next.len() != self.places.len()
            || next
                .iter()
                .zip(&self.places)
                .any(|(a, b)| {
                    a.name != b.name || a.path != b.path || a.bookmark != b.bookmark
                        || a.mount != b.mount || a.stale != b.stale
                });
        if changed {
            self.places = next;
        }
        changed
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
            cx.notify();
            return;
        };

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

    /// Kick off a background decode for a thumbable file; the result
    /// arrives with a notify. Safe to call every render: cached,
    /// inflight, and queued paths are no-ops. The kick marks the path
    /// wanted (this paint is asking) and queues it; the pool pops the
    /// queue up to THUMB_POOL concurrent decodes, and a queued path
    /// the paints stopped asking about dies at pop time instead of
    /// spending a decode. Results go through the disk cache keyed
    /// (path, mtime, size), so a restart or a re-scroll past an
    /// evicted entry reads the PNG instead of the source.
    fn request_thumb(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.thumbs.contains_key(&path)
            || self.thumbs_inflight.contains(&path)
            || self.thumbs_queue.contains(&path)
        {
            return;
        }
        self.thumb_wanted.insert(path.clone());
        let Ok(meta) = fs::symlink_metadata(&path) else {
            return;
        };
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        // decoding cost is in the file read, not the resize; cap it.
        // epubs read selectively (zip directory, manifest, cover
        // bytes) and videos stream through libav's seek, so the
        // file-size cap does not apply to them
        if !icons::is_epub(&name) && !icons::is_video(&name) && meta.len() > 32 * 1024 * 1024 {
            return;
        }
        self.thumbs_queue.push_back(path);
        self.pump_thumbs(cx);
    }

    /// Start queued thumb decodes while the pool has room. Called
    /// from kicks and from landings, whichever comes first.
    fn pump_thumbs(&mut self, cx: &mut Context<Self>) {
        while self.thumbs_inflight.len() < THUMB_POOL {
            let Some(path) = self.thumbs_queue.pop_front() else {
                return;
            };
            if !self.thumb_wanted.contains(&path) {
                continue; // off-screen since it was queued: dies here
            }
            let cache_path = self.cache_path_for(&path);
            self.thumbs_inflight.insert(path.clone());
            cx.spawn(async move |this, cx| {
                let bg_path = path.clone();
                let render = cx
                    .background_spawn(async move {
                        std::panic::catch_unwind(|| {
                            thumb_job(&bg_path, &cache_path)
                        })
                        .unwrap_or(None)
                    })
                    .await;
                let update = this.update(cx, |this, cx| {
                    this.thumbs_inflight.remove(&path);
                    if let Some(render) = render {
                        // evicted thumbs release their atlas tiles: the img
                        // element never drops the tile it paints, so a
                        // heap-only eviction would leave every thumbnail
                        // ever scrolled past painted until the window
                        // closed (ADR-0016)
                        for image in trim_thumbs(&mut this.thumbs, &mut this.thumb_order) {
                            cx.drop_image(image, None);
                        }
                        this.thumb_order.push_back(path.clone());
                        this.thumbs.insert(path, Arc::new(render));
                        cx.notify();
                    }
                    this.pump_thumbs(cx);
                });
                if let Err(err) = update {
                    log::error!("thumbnail update failed: {err:#}");
                }
            })
            .detach();
        }
    }

    /// The disk cache entry for a thumbable path, keyed by the
    /// (path, mtime, size) triple. The metadata is read fresh here:
    /// the kick already stat'ed the file, but this stays correct for
    /// any path handed in later.
    fn cache_path_for(&self, path: &Path) -> PathBuf {
        let key = match fs::symlink_metadata(path) {
            Ok(meta) => {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                thumb_cache_key(path, mtime, meta.len())
            }
            Err(_) => thumb_cache_key(path, 0, 0),
        };
        self.thumb_cache.join(format!("{key}.png"))
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
    /// regular two second tick (the idle tick task, not render:
    /// an idle window draws no frames). Reports whether the theme
    /// changed.
    fn refresh_palette(&mut self, cx: &mut Context<Self>) -> bool {
        if self.theme_refresh.elapsed() < Duration::from_secs(2) {
            return false;
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
            return true;
        }
        false
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
        write_recent(path);
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

    /// The filter changed: while the filter is non-empty, kick a
    /// recursive search of the tab's directory subtree (debounced, so
    /// fast typing walks once). Deep rows from the previous filter
    /// stay put until fresh results replace them in one update; the
    /// visible list re-scores names itself, so stale rows still
    /// respect the new filter and nothing flashes.
    fn filter_changed(&mut self, cx: &mut Context<Self>) {
        self.search_gen += 1;
        if self.filter.is_empty() {
            self.searching = false;
            self.prune_deep();
            self.snap_cursor_visible();
            cx.notify();
            return;
        }
        self.searching = true;
        let Source::Dir(root) = self.tab().source.clone() else {
            return;
        };
        let generation = self.search_gen;
        let filter = self.filter.clone();
        let show_hidden = self.show_hidden;
        cx.spawn(async move |this, cx| {
            // coalesce fast typing into one walk
            cx.background_executor()
                .timer(std::time::Duration::from_millis(150))
                .await;
            let still_current = this
                .update(cx, |this, _| this.search_gen == generation)
                .unwrap_or(false);
            if !still_current {
                return;
            }
            let matches =
                cx.background_spawn(async move { search_subtree(&root, &filter, show_hidden) })
                    .await;
            let update = this.update(cx, |this, cx| {
                if this.search_gen != generation {
                    return; // the listing moved on; results are stale
                }
                this.searching = false;
                // replace the deep rows only when the walk brought
                // something new, or the watcher's re-runs flicker
                let fresh: Vec<PathBuf> = matches.iter().map(|e| e.key.clone()).collect();
                let stale: Vec<PathBuf> = this
                    .tab()
                    .entries
                    .iter()
                    .filter(|e| e.rel.is_some())
                    .map(|e| e.key.clone())
                    .collect();
                if fresh == stale {
                    return;
                }
                this.prune_deep();
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
                    // notify's mask includes OPEN: every readdir of a
                    // child (ours and the search walker's own) fires
                    // an Access event that would feed the watcher
                    // itself. Only content changes matter.
                    let meaningful = |event: &&notify::Event| {
                        !matches!(event.kind, notify::EventKind::Access(_))
                    };
                    let relevant = batch
                        .iter()
                        .filter(meaningful)
                        .flat_map(|event| event.paths.iter())
                        .any(|p| {
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
        self.path_field = input::Field::new(dir.display().to_string());
        cx.notify();
    }

    fn cancel_path_edit(&mut self, cx: &mut Context<Self>) {
        self.path_editing = false;
        cx.notify();
    }

    /// Enter in the path bar: ~ expands home, existing dirs navigate.
    fn commit_path_edit(&mut self, cx: &mut Context<Self>) {
        self.path_editing = false;
        let mut target = self.path_field.text().trim().to_string();
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
                        // a session of big pastes would grow this
                        // forever; past the cap the oldest steps
                        // stop being undoable
                        if this.undo.len() > UNDO_MAX_OPS {
                            let drop = this.undo.len() - UNDO_MAX_OPS;
                            this.undo.drain(..drop);
                        }
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
    /// The sidebar's Connect to Server entry: a blank dialog whose
    /// host field carries the caret, the pump starts on Enter.
    fn open_connect_dialog(&mut self, cx: &mut Context<Self>) {
        self.open_connect_dialog_with(String::new(), cx);
    }

    /// The same dialog, blank or prefilled: reconnecting a stale
    /// network place hands over the URI parsed from the gvfs dir
    /// name, which fills the protocol and its fields and parks the
    /// caret on the last one, so Enter connects.
    fn open_connect_dialog_with(&mut self, uri: String, cx: &mut Context<Self>) {
        self.menu = None;
        let mut dialog = ConnectDialog {
            proto: ConnectProto::Sftp,
            fields: Default::default(),
            focus: FIELD_HOST,
            uri: String::new(),
            input: Default::default(),
            prompt: None,
            mask: false,
            status: String::new(),
            notes: Vec::new(),
            since: None,
            session: None,
            password_tried: false,
        };
        if !uri.is_empty() {
            match parse_server_uri(&uri) {
                Some((proto, host, user, port, share)) => {
                    dialog.proto = proto;
                    dialog.fields[FIELD_HOST] = input::Field::new(host);
                    dialog.fields[FIELD_USER] = input::Field::new(user);
                    dialog.fields[FIELD_PORT] = input::Field::new(port);
                    dialog.fields[FIELD_SHARE] = input::Field::new(share);
                    dialog.focus = dialog.last_visible();
                }
                None => {
                    dialog.status =
                        "cannot parse that server; schemes I speak: smb, sftp, ftp".into()
                }
            }
        }
        self.connect = Some(dialog);
        cx.notify();
    }

    /// Click a protocol button in the connect dialog: swap the field
    /// layout, keep the buffers, caret back to Host.
    fn set_connect_proto(&mut self, proto: ConnectProto, cx: &mut Context<Self>) {
        if let Some(dialog) = self.connect.as_mut() {
            if dialog.session.is_none() {
                dialog.proto = proto;
                dialog.focus = FIELD_HOST;
                cx.notify();
            }
        }
    }

    /// Click a field box: the caret moves there (its selection
    /// collapses; mouse-position placement needs text metrics Koguma
    /// does not have).
    fn set_connect_focus(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(dialog) = self.connect.as_mut() {
            if dialog.session.is_none() {
                dialog.fields[dialog.focus].collapse();
                dialog.focus = ix;
                cx.notify();
            }
        }
    }

    /// Click on a stale network place: the mount is gone (reboot,
    /// disconnect), so there is nothing to navigate into. Open the
    /// connect dialog prefilled with the URI parsed from the gvfs
    /// dir name instead.
    fn reconnect_place(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.menu = None;
        let raw = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match uri_from_gvfs_name(&raw) {
            Some(uri) => self.open_connect_dialog_with(uri, cx),
            None => {
                self.status = "cannot tell which server this was; use Connect to Server".into();
                cx.notify();
            }
        }
    }

    /// If the active tab sits inside the mount, step out to Home
    /// first: our own directory watch would hold the mount busy.
    fn leave_mount(&mut self, mount: &std::path::Path, cx: &mut Context<Self>) {
        if self
            .tab()
            .current_dir()
            .is_some_and(|dir| dir.starts_with(mount))
        {
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            self.load_source(Source::Dir(home), cx);
        }
    }

    /// Disconnect (gvfs network mount) or eject (udisks removable
    /// mount) a place: `gio mount -u` in the background, then a
    /// places refresh. Unmount asks no prompts; a failure surfaces
    /// gio's own error text, busy mounts included.
    fn unmount(&mut self, mount: PathBuf, kind: MountKind, cx: &mut Context<Self>) {
        self.menu = None;
        self.leave_mount(&mount, cx);
        let label = mount.display().to_string();
        cx.spawn(async move |this, cx| {
            let done = cx
                .background_spawn(async move {
                    std::process::Command::new("gio")
                        .args(["mount", "-u"])
                        .arg(&mount)
                        .output()
                })
                .await;
            let update = this.update(cx, |this, cx| {
                match done {
                    Ok(out) if out.status.success() => {
                        this.status = match kind {
                            MountKind::Network => format!("disconnected: {label}"),
                            MountKind::Removable => format!("ejected: {label}"),
                        };
                        this.refresh_places_now();
                    }
                    Ok(out) => {
                        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                        this.status = if err.is_empty() {
                            format!("unmount failed: {label}")
                        } else {
                            format!("unmount: {err}")
                        };
                    }
                    Err(err) => this.status = format!("unmount: gio: {err}"),
                }
                cx.notify();
            });
            if let Err(err) = update {
                log::error!("unmount update failed: {err:#}");
            }
        })
        .detach();
    }

    /// Enter on the last field: compose the URI from the protocol and
    /// its fields, spawn `gio mount`, and start relaying its prompts.
    /// The pump runs on the background pool; a task forwards its
    /// events into the view. The piped stdio handles belong to the
    /// pump; the child itself stays in a mutex so Cancel can kill it
    /// while the pump is blocked on its stdout.
    fn connect_start(&mut self, cx: &mut Context<Self>) {
        let Some(mut dialog) = self.connect.take() else {
            return;
        };
        let host = dialog.fields[FIELD_HOST].text().trim().to_string();
        if host.is_empty() {
            dialog.status = "enter a host, e.g. nas.local or 192.168.1.10".into();
            dialog.focus = FIELD_HOST;
            self.connect = Some(dialog);
            cx.notify();
            return;
        }
        let uri = compose_server_uri(
            dialog.proto,
            &host,
            dialog.fields[FIELD_USER].text(),
            dialog.fields[FIELD_PORT].text(),
            dialog.fields[FIELD_SHARE].text(),
        );
        let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
            dialog.status = "no session runtime dir".into();
            self.connect = Some(dialog);
            cx.notify();
            return;
        };
        let gvfs_dir = PathBuf::from(runtime).join("gvfs");
        // the FUSE bridge must be up before the mount lands, or the
        // pump's directory diff cannot see the new entry
        ensure_gvfs_fuse();
        let (ev_tx, ev_rx) = mpsc::channel::<ConnectEvent>();
        let (ans_tx, ans_rx) = mpsc::channel::<String>();
        let mut child = match std::process::Command::new("gio")
            .arg("mount")
            .arg(&uri)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => {
                log::error!("connect: could not spawn gio");
                dialog.status = "gio not found".into();
                self.connect = Some(dialog);
                cx.notify();
                return;
            }
        };
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (Some(stdin), Some(stdout), Some(stderr)) = (stdin, stdout, stderr) else {
            dialog.status = "could not pipe gio's stdio".into();
            self.connect = Some(dialog);
            cx.notify();
            return;
        };
        log::info!("connect: gio mount {uri}");
        let child_cell = Arc::new(Mutex::new(Some(child)));
        dialog.session = Some(ConnectSession {
            answers: ans_tx,
            child: child_cell.clone(),
        });
        dialog.prompt = None;
        dialog.mask = false;
        dialog.notes.clear();
        dialog.password_tried = false;
        dialog.uri = uri.clone();
        dialog.status = format!("connecting to {uri}…");
        dialog.since = Some(std::time::Instant::now());
        self.connect = Some(dialog);
        cx.notify();

        // pump: drive gio to completion in the background. Detached:
        // dropping the Task handle would cancel it before it starts
        // (the race made rides go silent with just a live counter).
        cx.background_spawn(async move {
            run_mount_process(stdin, stdout, stderr, child_cell, gvfs_dir, ev_tx, ans_rx);
        })
        .detach();

        // event relay: block a pool thread per recv, wake the view
        let events = Arc::new(Mutex::new(ev_rx));
        cx.spawn(async move |this, cx| {
            loop {
                let events = events.clone();
                let ev = cx
                    .background_spawn(async move { events.lock().unwrap().recv() })
                    .await;
                match ev {
                    Ok(ev) => {
                        if !this
                            .update(cx, |this, cx| this.connect_event(ev, cx))
                            .unwrap_or(false)
                        {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        })
        .detach();
    }

    /// Relay one pump event into the dialog. Returns false when the
    /// pump should stop (dialog closed, view gone, or Done).
    fn connect_event(&mut self, ev: ConnectEvent, cx: &mut Context<Self>) -> bool {
        match ev {
            ConnectEvent::Prompt { text, mask } => {
                let Some(dialog) = self.connect.as_mut() else {
                    return false;
                };
                // a password typed into the form answers the first
                // password prompt by itself (the URI never carries it:
                // process lists are world-readable); a second one, it
                // was wrong, surfaces for typing
                let stored = dialog.fields[FIELD_PASSWORD].text().to_string();
                if mask && !dialog.password_tried && !stored.is_empty() {
                    dialog.password_tried = true;
                    // the question just answered is gone, including
                    // any stale prompt still on screen
                    dialog.prompt = None;
                    dialog.mask = false;
                    dialog.input = input::Field::default();
                    if let Some(session) = dialog.session.as_ref() {
                        let _ = session.answers.send(stored);
                    }
                    log::info!("connect: password prompt answered from the form");
                    cx.notify();
                    return true;
                }
                log::info!("connect: prompt '{text}' (masked: {mask})");
                dialog.prompt = Some(text);
                dialog.mask = mask;
                dialog.input = input::Field::default();
                cx.notify();
                true
            }
            ConnectEvent::Note(line) => {
                // gio's context: identity text, [1]/[2] choices, errors
                if let Some(dialog) = self.connect.as_mut() {
                    dialog.notes.push(line);
                    if dialog.notes.len() > 6 {
                        dialog.notes.remove(0);
                    }
                    cx.notify();
                }
                true
            }
            ConnectEvent::Done { ok, message, mount } => {
                let Some(dialog) = self.connect.take() else {
                    return false;
                };
                log::info!(
                    "connect: done ok={ok} mount={} ({message})",
                    mount
                        .as_deref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "none".into())
                );
                if ok {
                    self.status = format!("{}: {}", message, dialog.uri.trim());
                    match mount {
                        Some(path) => self.load_source(Source::Dir(path), cx),
                        None if gvfs_fuse_bin().is_none() => {
                            // connected, but nothing can browse it: the
                            // image lacks the gvfs-fuse bridge
                            if let Some(dialog) = self.connect.as_mut() {
                                dialog.status = "connected. The gvfs-fuse package is missing \
                                    on this system, so there is no folder to browse yet; \
                                    the mount is live and will appear once the bridge is \
                                    installed."
                                    .into();
                            }
                        }
                        None => {
                            log::info!("connect: mount registered, fuse entry not found");
                            self.refresh_places_now();
                        }
                    }
                    self.refresh_places_now();
                } else {
                    // keep the dialog up: the fields are editable for
                    // a retry
                    log::info!("connect: failed ({message})");
                    self.connect = Some(ConnectDialog {
                        proto: dialog.proto,
                        fields: dialog.fields,
                        focus: FIELD_HOST,
                        uri: dialog.uri,
                        input: input::Field::default(),
                        prompt: None,
                        mask: false,
                        status: message,
                        notes: dialog.notes,
                        since: None,
                        session: None,
                        password_tried: false,
                    });
                }
                cx.notify();
                false
            }
        }
    }

    /// Enter at a prompt: ship the buffered credential to gio.
    fn connect_submit(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.connect.as_mut() else {
            return;
        };
        let (Some(session), Some(_)) = (dialog.session.as_ref(), dialog.prompt.as_ref()) else {
            return;
        };
        let answer = dialog.input.text().to_string();
        if session.answers.send(answer.clone()).is_ok() {
            log::info!("connect: answer sent ({} chars)", answer.len());
            dialog.prompt = None;
            dialog.mask = false;
            dialog.input = input::Field::default();
            dialog.status = "connecting…".into();
        }
        cx.notify();
    }

    /// Cancel: kill the gio child and close the dialog.
    fn cancel_connect(&mut self, cx: &mut Context<Self>) {
        if let Some(dialog) = self.connect.take() {
            if let Some(session) = dialog.session {
                log::info!("connect: cancelled");
                if let Ok(mut cell) = session.child.lock() {
                    if let Some(child) = cell.as_mut() {
                        let _ = child.kill();
                    }
                }
            }
        }
        cx.notify();
    }

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
                name: input::Field::new(format!("{default_name}.tar.gz")),
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
        if dialog.name.buf.to_lowercase().ends_with(other) {
            let stem_len = dialog.name.buf.len() - other.len();
            dialog.name.buf = format!("{}{want}", &dialog.name.buf[..stem_len]);
            dialog.name.cursor = dialog.name.cursor.min(dialog.name.buf.len());
            dialog.name.sel = None;
        }
        dialog.zip = zip;
    }

    /// Build the archive from the dialog: name, format, selection.
    fn compress_create(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.compress.take() else {
            return;
        };
        let name = dialog.name.text().trim().to_string();
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
        let connect_row = div()
            .id("connect-server")
            .px_3()
            .py_1()
            .rounded_sm()
            .cursor_pointer()
            .text_size(px(13.))
            .text_color(theme::text_dim())
            .hover(|this| this.bg(theme::row_hover()))
            .on_click(cx.listener(|this, _, _, cx| this.open_connect_dialog(cx)))
            .child("Connect to Server");
        let mut section = div()
            .flex()
            .flex_col()
            .gap_px()
            .child(connect_row)
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

    /// Space: open Quick Look over the cursor entry, or close it when
    /// open. Trash rows preview nothing (their key is a .trashinfo
    /// path, not a file worth showing).
    fn toggle_quicklook(&mut self, cx: &mut Context<Self>) {
        if self.quicklook.is_some() {
            self.close_quicklook(cx);
            return;
        }
        if self.tab().cursor.is_some() {
            self.quicklook = Some(QuickLook {
                path: PathBuf::new(),
                zoom: 1.0,
                pan: (0.0, 0.0),
                drag_from: None,
                page: 1,
                pages: None,
            });
            self.sync_quicklook(cx);
            // open must paint even when the preview is already
            // cached: nothing else lands afterward to notify
            cx.notify();
        }
    }

    fn close_quicklook(&mut self, cx: &mut Context<Self>) {
        if let Some((_, _, image)) = self.ql_render.take() {
            cx.drop_image(image, None);
        }
        self.ql_inflight = None;
        self.quicklook = None;
        cx.notify();
    }

    /// Re-point the open overlay at the cursor entry. Flips land
    /// here, and so does the render after a watcher reshuffle moves
    /// the listing under the overlay. A cursor with nothing to show
    /// closes it.
    fn sync_quicklook(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self
            .tab()
            .cursor
            .and_then(|ix| self.tab().entries.get(ix))
            .filter(|entry| entry.item.is_none())
            .cloned()
        else {
            self.close_quicklook(cx);
            return;
        };
        let moved = self
            .quicklook
            .as_ref()
            .is_some_and(|ql| ql.path != entry.path);
        if moved {
            if let Some(ql) = self.quicklook.as_mut() {
                ql.path = entry.path.clone();
                ql.zoom = 1.0;
                ql.pan = (0.0, 0.0);
                ql.drag_from = None;
                // a different file pages from its own start, and its
                // count (if any) gets asked fresh; an epub opens on
                // its cover (page 0), everything else on page 1
                ql.page = if icons::is_epub(&entry.name) { 0 } else { 1 };
                ql.pages = None;
                self.ql_counting = false;
                // the epub reading chain dies here: the next landing
                // sees a foreign book and stops
                self.ql_text_for = None;
                self.ql_text_blocks = Arc::new(Vec::new());
                self.ql_text_starts = Vec::new();
                self.ql_book_at = 0;
                self.ql_book_state.reset(0);
                self.ql_text = None;
                self.ql_text_inflight = None;
                self.ql_text_failed = None;
                self.ql_md_state.reset(0);
                self.ql_text_scroll
                    .0
                    .borrow_mut()
                    .base_handle
                    .set_offset(point(px(0.), px(0.)));
            }
            if let Some((_, _, image)) = self.ql_render.take() {
                cx.drop_image(image, None);
            }
            self.ql_book = None;
            self.ql_video_len = None;
            self.video_meta_asked = false;
            self.ql_scroll.set_offset(point(px(0.), px(0.)));
        }
        self.feed_quicklook(&entry, cx);
    }

    /// Flip inside Quick Look: one step of cursor movement through the
    /// visible listing, then the overlay follows.
    fn quicklook_flip(&mut self, step: isize, cx: &mut Context<Self>) {
        self.move_cursor(step, cx);
        self.sync_quicklook(cx);
        cx.notify();
    }

    /// +/- keys: multiplicative zoom steps; back at fit, the pan
    /// resets.
    fn quicklook_zoom_step(&mut self, step: f32, cx: &mut Context<Self>) {
        let Some(ql) = self.quicklook.as_mut() else {
            return;
        };
        ql.zoom = (ql.zoom * step).clamp(1.0, QL_ZOOM_MAX);
        if ql.zoom <= 1.0 {
            ql.pan = (0.0, 0.0);
        }
        cx.notify();
    }

    /// Kick the right pipeline for the entry under Quick Look: a
    /// pane-scale decode for page-style files, the text snippet for
    /// the rest. While Quick Look is open it owns the preview; the
    /// render's rail feed re-syncs the same keys, so double requests
    /// collapse in the guards.
    fn feed_quicklook(&mut self, entry: &Entry, cx: &mut Context<Self>) {
        if entry.is_dir {
            return;
        }
        if icons::is_thumbable(&entry.name) || icons::is_epub(&entry.name) {
            // an epub's cover is page 0; everything else starts at 1
            let opening = if icons::is_epub(&entry.name) { 0 } else { 1 };
            self.request_quicklook_render(entry.path.clone(), opening, cx);
            // a multi-page PDF or an epub wants its count so paging
            // and the indicator can wake up (one ask per open path)
            if icons::is_pdf(&entry.name) || icons::is_epub(&entry.name) {
                self.request_page_count(entry.path.clone(), cx);
            }
            // a video's poster rides the image arm, so zoom and pan
            // come free; its header facts land once into the shared
            // label slot (playback itself stays with the default
            // handler; gpui has no decoder)
            if icons::is_video(&entry.name) {
                self.request_video_duration(entry.path.clone(), cx);
            }
        } else {
            let key = entry.key.clone();
            if self.preview_key.as_ref() != Some(&key) {
                self.preview_key = Some(key);
                self.text_preview = None;
                self.request_text_preview(entry.path.clone(), cx);
            }
            // and the whole-file read for the reading pane; the
            // snippet above stays the loading state until it lands
            self.request_text_read(entry.path.clone(), cx);
        }
    }

    /// PageUp/PageDown/Home/End: turn pages of the document under
    /// view. A PDF pages through its pages. An epub has no pages, so
    /// the pager jumps spine chapters: position 0 is the cover
    /// (PageUp from chapter 1 settles back onto it), k >= 1 is
    /// chapter k, and the reading pane scrolls to that chapter's
    /// first block. No-ops while the count is unknown.
    fn quicklook_page(&mut self, target: usize, cx: &mut Context<Self>) {
        let Some(ql) = self.quicklook.as_mut() else {
            return;
        };
        let Some(pages) = ql.pages else {
            return;
        };
        let is_epub = icons::is_epub(&ql.path.file_name().unwrap_or_default().to_string_lossy());
        if is_epub {
            let next = target.clamp(0, pages);
            if next == self.ql_book_at {
                return;
            }
            self.ql_book_at = next;
            cx.notify();
            if next == 0 {
                // the cover: the image slot already holds it
                return;
            }
            self.jump_book_chapter(next);
            return;
        }
        let next = target.clamp(1, pages);
        if next == ql.page {
            return;
        }
        ql.page = next;
        ql.zoom = 1.0;
        ql.pan = (0.0, 0.0);
        ql.drag_from = None;
        cx.notify();
        self.ql_scroll.set_offset(point(px(0.), px(0.)));
        let path = ql.path.clone();
        self.request_quicklook_render(path, next, cx);
    }

    /// One page step from the current position (negative is back),
    /// clamped by the known count. The keyboard twin of the header
    /// chevrons: arrows step by file, shift+arrows and the page keys
    /// step by page or chapter. Steps may reach 0 before the clamp:
    /// that is an epub settling back on its cover.
    fn quicklook_page_step(&mut self, step: isize, cx: &mut Context<Self>) {
        let is_epub = self.quicklook.as_ref().is_some_and(|ql| {
            icons::is_epub(&ql.path.file_name().unwrap_or_default().to_string_lossy())
        });
        let at = if is_epub {
            self.ql_book_at
        } else {
            self.quicklook.as_ref().map(|ql| ql.page).unwrap_or(1)
        };
        let target = (at as isize + step).max(0) as usize;
        self.quicklook_page(target, cx);
    }

    /// A clickable page chevron for the Quick Look header: the mouse
    /// road into paging, for people without (or without reach of)
    /// the page keys. Clamped by quicklook_page at the ends.
    fn quicklook_chevron(
        &self,
        id: &'static str,
        glyph: &'static str,
        step: isize,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .cursor_pointer()
            .px_1()
            .rounded_sm()
            .text_size(px(12.))
            .text_color(theme::text_dim())
            .hover(|this| this.text_color(theme::text()).bg(theme::row_hover()))
            .on_click(cx.listener(move |this, _, _, cx| this.quicklook_page_step(step, cx)))
            .child(glyph)
    }

    /// Ask how many pages the open document has: pdfinfo for PDFs,
    /// the spine for epubs. One ask per open path; a missing tool or
    /// a failed call lands as None and paging stays dormant.
    fn request_page_count(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.ql_counting {
            return;
        }
        self.ql_counting = true;
        cx.spawn(async move |this, cx| {
            let bg_path = path.clone();
            let pages = cx
                .background_spawn(async move {
                    if icons::is_pdf(&bg_path.file_name().unwrap_or_default().to_string_lossy()) {
                        let out = std::process::Command::new("pdfinfo")
                            .arg(&bg_path)
                            .output()
                            .ok()?;
                        parse_pdf_pages(&String::from_utf8_lossy(&out.stdout))
                    } else {
                        crate::epub::read_spine(&bg_path).map(|spine| spine.len())
                    }
                })
                .await;
            let open_path = path.clone();
            let update = this.update(cx, |this, cx| {
                if let Some(ql) = this.quicklook.as_mut()
                    && ql.path == path
                {
                    ql.pages = pages;
                    cx.notify();
                }
                // an epub's spine count wakes the reading chain: the
                // whole book decodes in order while the cover shows
                let is_epub = this
                    .quicklook
                    .as_ref()
                    .is_some_and(|ql| ql.path == open_path);
                if pages.is_some() && is_epub {
                    this.request_book_text(open_path, cx);
                }
            });
            if let Err(err) = update {
                log::error!("quick look page count failed: {err:#}");
            }
        })
        .detach();
    }

    /// The open video's header facts: duration and coded dimensions
    /// from the container, no decode. One ask per open path; the
    /// label rides the header like an epub's book label. A header
    /// that will not read just leaves the label out; the pane still
    /// shows its poster or its card.
    fn request_video_duration(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.video_meta_asked {
            return;
        }
        self.video_meta_asked = true;
        cx.spawn(async move |this, cx| {
            let bg_path = path.clone();
            let label = cx
                .background_spawn(async move {
                    crate::video::probe(&bg_path).and_then(|(secs, w, h)| {
                        crate::video::duration_label(secs).map(|len| {
                            format!("{len} · {}\u{d7}{}", w, h)
                        })
                    })
                })
                .await;
            let update = this.update(cx, |this, cx| {
                if let Some(label) = label
                    && this.quicklook.as_ref().is_some_and(|ql| ql.path == path)
                {
                    this.ql_video_len = Some((path, label));
                    cx.notify();
                }
            });
            if let Err(err) = update {
                log::error!("video duration failed: {err:#}");
            }
        })
        .detach();
    }

    /// Kick the background decode for Quick Look's own render (the
    /// listing's thumbs are 256px; a pane wants more). Keyed by path
    /// and page; a newer request supersedes an older one in flight,
    /// and a stale landing drops quietly (its pixels were never
    /// uploaded, so the Arc release is enough). Failures keep the
    /// screen honest: a held tile for the file reverts the counter
    /// to the page it shows; nothing held stands the card in via the
    /// failed slot so sync does not re-spawn every frame.
    fn request_quicklook_render(&mut self, path: PathBuf, page: usize, cx: &mut Context<Self>) {
        let nothing_held_failed = self.ql_render.as_ref().is_none_or(|(p, _, _)| *p != path)
            && self
                .ql_failed
                .as_ref()
                .is_some_and(|(p, _)| *p == path);
        // an epub whose meta already landed is read: a retry would
        // not find a cover that was not there
        let already_read = self.ql_book.is_some()
            && self.quicklook.as_ref().is_some_and(|ql| ql.path == path);
        if self
            .ql_render
            .as_ref()
            .is_some_and(|(p, pg, _)| *p == path && *pg == page)
            || self.ql_inflight.as_ref() == Some(&(path.clone(), page))
            || nothing_held_failed
            || already_read
        {
            return;
        }
        let Ok(meta) = fs::symlink_metadata(&path) else {
            self.ql_failed = Some((path, crate::video::PosterFail::Empty));
            return;
        };
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let is_epub = icons::is_epub(&name);
        let is_video = icons::is_video(&name);
        // decoding cost is in the file read, not the resize; cap it.
        // epubs read selectively (zip directory, manifest, cover
        // bytes) and videos stream through libav's seek, so the
        // file-size cap does not apply to them
        if !is_epub && !is_video && meta.len() > 32 * 1024 * 1024 {
            self.ql_failed = Some((path, crate::video::PosterFail::Empty));
            return;
        }
        self.ql_inflight = Some((path.clone(), page));
        // a fresh attempt supersedes an old failure's knowledge
        self.ql_failed = None;
        let is_pdf = icons::is_pdf(&name);
        cx.spawn(async move |this, cx| {
            let bg_path = path.clone();
            let (render, book, video_fail) = cx
                .background_spawn(async move {
                    std::panic::catch_unwind(|| {
                        if is_pdf {
                            (
                                icons::decode_pdf_thumbnail(&bg_path, page, QL_DECODE_MAX),
                                None,
                                None,
                            )
                        } else if is_epub {
                            match crate::epub::read_epub_meta(&bg_path) {
                                Some(meta) => (
                                    meta.cover
                                        .as_deref()
                                        .and_then(|bytes| icons::decode_cover(bytes, QL_DECODE_MAX)),
                                    Some(meta),
                                    None,
                                ),
                                None => (None, None, None),
                            }
                        } else if is_video {
                            match crate::video::decode_poster(&bg_path, QL_DECODE_MAX) {
                                Ok(render) => (Some(render), None, None),
                                Err(fail) => (None, None, Some(fail)),
                            }
                        } else {
                            (
                                icons::decode_thumbnail(&bg_path, QL_DECODE_MAX, QL_DECODE_MAX),
                                None,
                                None,
                            )
                        }
                    })
                    .unwrap_or((None, None, None))
                })
                .await;
            let update = this.update(cx, |this, cx| {
                if this.ql_inflight.as_ref() == Some(&(path.clone(), page)) {
                    this.ql_inflight = None;
                }
                // the book's words land regardless of the tile: an
                // epub without a cover still names itself
                if let Some(book) = &book
                    && this.quicklook.as_ref().is_some_and(|ql| ql.path == path)
                {
                    this.ql_book = Some(book.clone());
                    cx.notify();
                }
                // landed empty (corrupt file, missing pdftocairo, a
                // page beyond the end): keep the screen honest. An
                // epub with meta but no cover is a success, not a
                // failure: the enriched card stands in
                let Some(render) = render else {
                    if book.is_some() {
                        return;
                    }
                    match this.ql_render.as_ref() {
                        // paging: the held tile stays up, the counter
                        // reverts to the page it shows; the guard
                        // matches again, so no respawn loop
                        Some((p, pg, _)) if *p == path => {
                            if let Some(ql) = this.quicklook.as_mut()
                                && ql.path == path
                            {
                                ql.page = *pg;
                                cx.notify();
                            }
                        }
                        // nothing held (the opening decode failed):
                        // the card stands in and the failed slot
                        // stops the respawn loop; a missing codec is
                        // the named failure, the card says so
                        _ => {
                            let fail = video_fail.unwrap_or(crate::video::PosterFail::Empty);
                            this.ql_failed = Some((path, fail));
                            cx.notify();
                        }
                    }
                    return;
                };
                // the overlay may have closed, flipped files, or
                // turned pages mid-decode: a stale landing drops
                // quietly (never uploaded, the Arc release suffices)
                if this
                    .quicklook
                    .as_ref()
                    .is_some_and(|ql| ql.path == path && ql.page == page)
                {
                    // the previous tile goes the moment the new one
                    // lands (ADR-0016)
                    if let Some((_, _, old)) = this.ql_render.take() {
                        cx.drop_image(old, None);
                    }
                    this.ql_render = Some((path, page, Arc::new(render)));
                    cx.notify();
                }
            });
            if let Err(err) = update {
                log::error!("quick look decode failed: {err:#}");
            }
        })
        .detach();
    }

    /// Scroll the epub reading pane to a chapter's first block once
    /// the chain has decoded it. A jump onto not-yet-decoded text
    /// re-applies from each chain landing, so an early PageDown
    /// settles when the chapter arrives.
    fn jump_book_chapter(&self, chapter: usize) {
        if let Some(&ix) = self.ql_text_starts.get(chapter - 1) {
            self.ql_book_state
                .scroll_to(gpui::ListOffset {
                    item_ix: ix,
                    offset_in_item: px(0.),
                });
        }
    }

    /// Start the epub reading chain: spine chapters decode in order
    /// in the background, blocks appending as each lands, until the
    /// book ends or a chapter fails (the tail then carries a marker
    /// and the chain stops: a corrupt container rarely heals between
    /// chapters). One chain per open book; a flip clears the book
    /// slot and the next landing dies.
    fn request_book_text(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.ql_text_for.as_ref() == Some(&path) {
            return;
        }
        let Some(pages) = self.quicklook.as_ref().and_then(|ql| ql.pages) else {
            return;
        };
        self.ql_text_for = Some(path.clone());
        cx.spawn(async move |this, cx| {
            for chapter in 1..=pages {
                let bg_path = path.clone();
                let blocks = cx
                    .background_spawn(async move {
                        std::panic::catch_unwind(|| crate::epub::read_chapter(&bg_path, chapter))
                            .unwrap_or(None)
                    })
                    .await;
                let mut stop = false;
                let update = this.update(cx, |this, cx| {
                    // flipped away: the chain dies here
                    if this.ql_text_for.as_ref() != Some(&path) {
                        stop = true;
                        return;
                    }
                    let Some(text) = blocks else {
                        stop = true;
                        let marker = format!("… chapter {chapter} failed to read");
                        let len = marker.len();
                        let mut grown = (*this.ql_text_blocks).clone();
                        this.ql_text_starts.push(grown.len());
                        grown.push(epub::ChapterBlock::Para {
                            text: marker,
                            runs: vec![epub::ChapterRun {
                                len,
                                italic: true,
                                bold: false,
                            }],
                            quote: true,
                        });
                        this.ql_text_blocks = Arc::new(grown);
                        this.ql_book_state.reset(this.ql_text_blocks.len());
                        cx.notify();
                        return;
                    };
                    // append the chapter's blocks and remember where
                    // it starts so jumps can land on it
                    let mut all = (*this.ql_text_blocks).clone();
                    this.ql_text_starts.push(all.len());
                    all.extend(text);
                    this.ql_text_blocks = Arc::new(all);
                    this.ql_book_state.reset(this.ql_text_blocks.len());
                    // honor a pending jump onto not-yet-decoded text
                    if this.ql_book_at >= 1 {
                        this.jump_book_chapter(this.ql_book_at);
                    }
                    cx.notify();
                });
                if let Err(err) = update {
                    log::error!("book text chain failed: {err:#}");
                    return;
                }
                if stop {
                    return;
                }
            }
        })
        .detach();
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
                            Some(Arc::new(parse_markdown(&loaded.join("\n"))))
                        } else {
                            None
                        };
                        TextPreview {
                            kind,
                            lines: Arc::new(loaded),
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

    /// Read a text-ish file whole (capped at TEXT_READ_MAX) for the
    /// reading pane: the rail's snippet stays the loading state until
    /// this lands over it. A binary smell (a NUL in the head) or an
    /// unreadable file lands in the failed slot, the card stands in,
    /// and the respawn loop stays dead. Lines cap individually: one
    /// enormous line must not become one enormous layout. Csv rows
    /// align at decode so the pane renders, not computes.
    fn request_text_read(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.ql_text.as_ref().is_some_and(|(p, _)| *p == path)
            || self.ql_text_inflight.as_ref() == Some(&path)
            || self.ql_text_failed.as_ref() == Some(&path)
        {
            return;
        }
        let Ok(meta) = fs::symlink_metadata(&path) else {
            self.ql_text_failed = Some(path);
            return;
        };
        self.ql_text_inflight = Some(path.clone());
        let kind = text_kind(&path);
        let total = meta.len();
        cx.spawn(async move |this, cx| {
            let bg_path = path.clone();
            let read = cx
                .background_spawn(async move {
                    let mut file = fs::File::open(&bg_path).ok()?;
                    // the head doubles as the binary smell test
                    let mut head = vec![0u8; 4096];
                    let mut filled = io::Read::read(&mut file, &mut head).unwrap_or(0);
                    if head[..filled].contains(&0) {
                        return None;
                    }
                    let capped = total > TEXT_READ_MAX as u64;
                    let mut buf = vec![0u8; TEXT_READ_MAX];
                    buf[..filled].copy_from_slice(&head[..filled]);
                    while filled < buf.len() {
                        let n = io::Read::read(&mut file, &mut buf[filled..]).ok()?;
                        if n == 0 {
                            break;
                        }
                        filled += n;
                    }
                    // the read may cut mid-char; lossy conversion
                    // plus a boundary trim keeps the tail clean
                    let mut text = String::from_utf8_lossy(&buf[..filled]).into_owned();
                    while !text.is_char_boundary(text.len()) {
                        text.pop();
                    }
                    let lines: Vec<String> = text
                        .lines()
                        .map(|line| match line.char_indices().nth(TEXT_LINE_MAX) {
                            Some((cut, _)) => format!("{}\u{2026}", &line[..cut]),
                            None => line.to_string(),
                        })
                        .collect();
                    Some((lines, capped, total))
                })
                .await;
            let update = this.update(cx, |this, cx| {
                if this.ql_text_inflight.as_ref() == Some(&path) {
                    this.ql_text_inflight = None;
                }
                let Some((mut lines, capped, total)) = read else {
                    if this.quicklook.as_ref().is_some_and(|ql| ql.path == path) {
                        this.ql_text_failed = Some(path);
                        cx.notify();
                    }
                    return;
                };
                // the overlay may have flipped files mid-read: a stale
                // landing drops (the fresh file's read owns the slot)
                if !this.quicklook.as_ref().is_some_and(|ql| ql.path == path) {
                    return;
                }
                if kind == TextKind::Csv {
                    lines = csv_lines(&lines);
                }
                if capped {
                    lines.push(format!("… the file continues ({})", human_size(total)));
                }
                let blocks = if kind == TextKind::Markdown {
                    let parsed = Arc::new(parse_markdown(&lines.join("\n")));
                    this.ql_md_state.reset(parsed.len());
                    Some(parsed)
                } else {
                    None
                };
                this.ql_text_failed = None;
                this.ql_text = Some((
                    path,
                    TextPreview {
                        kind,
                        lines: Arc::new(lines),
                        blocks,
                    },
                ));
                cx.notify();
            });
            if let Err(err) = update {
                log::error!("text read update failed: {err:#}");
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
            rename_field: Default::default(),
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

        if self.connect.is_some() {
            let mut dialog = self.connect.take().unwrap();
            let at_prompt = dialog.session.is_some() && dialog.prompt.is_some();
            match keystroke.key.as_str() {
                "escape" => {
                    self.connect = Some(dialog);
                    self.cancel_connect(cx);
                    return;
                }
                "enter" => {
                    if at_prompt {
                        self.connect = Some(dialog);
                        self.connect_submit(cx);
                    } else if dialog.session.is_some() {
                        // between prompts: gio is thinking
                        self.connect = Some(dialog);
                        cx.notify();
                    } else if dialog.focus == dialog.last_visible() {
                        self.connect = Some(dialog);
                        self.connect_start(cx);
                    } else {
                        dialog.cycle(false);
                        self.connect = Some(dialog);
                        cx.notify();
                    }
                    return;
                }
                "tab" => {
                    if dialog.session.is_none() {
                        dialog.cycle(keystroke.modifiers.shift);
                    }
                    self.connect = Some(dialog);
                    cx.notify();
                    return;
                }
                "a" if keystroke.modifiers.control || keystroke.modifiers.platform => {
                    if at_prompt {
                        dialog.input.select_all();
                    } else if dialog.session.is_none() {
                        dialog.focused_mut().select_all();
                    }
                }
                "v" if keystroke.modifiers.control || keystroke.modifiers.platform => {
                    // paste into the active field: the focused field
                    // before the pump runs, the credential answer at a
                    // prompt; a pasted URI fills the whole form
                    if let Some(item) = cx.read_from_clipboard() {
                        // a URI or a credential is one line; pasted
                        // newlines would corrupt the stdin protocol
                        let text = item
                            .text()
                            .unwrap_or_default()
                            .trim_matches(|c: char| c == '\r' || c == '\n')
                            .to_string();
                        if at_prompt {
                            dialog.input.paste(&text);
                        } else if dialog.session.is_none() {
                            if text.contains("://") {
                                match parse_server_uri(&text) {
                                    Some((proto, host, user, port, share)) => {
                                        dialog.proto = proto;
                                        dialog.fields[FIELD_HOST] = input::Field::new(host);
                                        dialog.fields[FIELD_USER] = input::Field::new(user);
                                        dialog.fields[FIELD_PORT] = input::Field::new(port);
                                        dialog.fields[FIELD_SHARE] = input::Field::new(share);
                                        dialog.focus = dialog.last_visible();
                                    }
                                    None => {
                                        dialog.status =
                                            "schemes I can connect: smb, sftp, ftp".into()
                                    }
                                }
                            } else {
                                dialog.focused_mut().paste(&text);
                            }
                        }
                    }
                }
                key => {
                    // two phases: the focused field before the pump
                    // runs, the credential answer while a prompt is up
                    if at_prompt {
                        dialog.input.key(key, &keystroke);
                    } else if dialog.session.is_none() {
                        dialog.focused_mut().key(key, &keystroke);
                    }
                }
            }
            self.connect = Some(dialog);
            cx.notify();
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
                "a" if keystroke.modifiers.control || keystroke.modifiers.platform => {
                    dialog.name.select_all();
                }
                "v" if keystroke.modifiers.control || keystroke.modifiers.platform => {
                    if let Some(item) = cx.read_from_clipboard() {
                        // an archive name is one line
                        let text = item
                            .text()
                            .unwrap_or_default()
                            .trim_matches(|c: char| c == '\r' || c == '\n')
                            .to_string();
                        dialog.name.paste(&text);
                    }
                }
                key => {
                    dialog.name.key(key, &keystroke);
                }
            }
            self.compress = Some(dialog);
            cx.notify();
            return;
        }

        if self.quicklook.is_some() {
            match keystroke.key.as_str() {
                // space toggles shut, escape closes: both keep the
                // cursor where it was
                "space" | "escape" => self.close_quicklook(cx),
                // shift+arrows: same direction, bigger step, a page
                // instead of a file
                "up" | "left" if keystroke.modifiers.shift => {
                    self.quicklook_page_step(-1, cx)
                }
                "down" | "right" if keystroke.modifiers.shift => {
                    self.quicklook_page_step(1, cx)
                }
                "up" | "left" => self.quicklook_flip(-1, cx),
                "down" | "right" => self.quicklook_flip(1, cx),
                // enter hands off: a file goes to the system opener
                // (the ADR-0013 road), a folder navigates; the overlay
                // closes either way
                "enter" => {
                    self.close_quicklook(cx);
                    self.open_selection(cx);
                }
                "=" | "+" => self.quicklook_zoom_step(1.2, cx),
                "-" | "_" => self.quicklook_zoom_step(1.0 / 1.2, cx),
                // PDF paging: no-ops while the count is unknown
                "pageup" => self.quicklook_page_step(-1, cx),
                "pagedown" => self.quicklook_page_step(1, cx),
                "home" => self.quicklook_page(1, cx),
                "end" => {
                    let last = self
                        .quicklook
                        .as_ref()
                        .and_then(|ql| ql.pages)
                        .unwrap_or(1);
                    self.quicklook_page(last, cx);
                }
                "0" => {
                    if let Some(ql) = self.quicklook.as_mut() {
                        ql.zoom = 1.0;
                        ql.pan = (0.0, 0.0);
                    }
                    cx.notify();
                }
                _ => {}
            }
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
                "a" if keystroke.modifiers.control || keystroke.modifiers.platform => {
                    self.path_field.select_all();
                    cx.notify();
                }
                "v" if keystroke.modifiers.control || keystroke.modifiers.platform => {
                    if let Some(item) = cx.read_from_clipboard() {
                        // a path is one line
                        let text = item
                            .text()
                            .unwrap_or_default()
                            .trim_matches(|c: char| c == '\r' || c == '\n')
                            .to_string();
                        self.path_field.paste(&text);
                        cx.notify();
                    }
                }
                key => {
                    self.path_field.key(key, &keystroke);
                    cx.notify();
                }
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
                "a" if keystroke.modifiers.control || keystroke.modifiers.platform => {
                    tab.rename_field.select_all();
                    cx.notify();
                }
                "v" if keystroke.modifiers.control || keystroke.modifiers.platform => {
                    if let Some(item) = cx.read_from_clipboard() {
                        // a file name is one line
                        let text = item
                            .text()
                            .unwrap_or_default()
                            .trim_matches(|c: char| c == '\r' || c == '\n')
                            .to_string();
                        tab.rename_field.paste(&text);
                        cx.notify();
                    }
                }
                key => {
                    tab.rename_field.key(key, &keystroke);
                    cx.notify();
                }
            }
            return;
        }

        match keystroke.key.as_str() {
            "enter" if keystroke.modifiers.alt => self.toggle_inspector(cx),
            "enter" => self.open_selection(cx),
            // space: Quick Look over the cursor entry (route_key's
            // rung owns it once open)
            "space" => self.toggle_quicklook(cx),
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
        } else if crate::viewer::is_openable(&entry.name) {
            // the open surface: kuma-files claims these kinds as the
            // system default, so Enter opens in-process instead of
            // round-tripping xdg-open back to ourselves. The recency
            // note is the viewer's job on this road.
            crate::viewer::open_or_focus(entry.path.clone(), cx);
            self.status = format!("opened {}", entry.name);
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
        tab.rename_field = input::Field::new(entry.name.clone());
        // the name starts selected: typing replaces it, which is what
        // "rename" almost always means. The caret used to park at the
        // end, which read as the editor ignoring keystrokes
        tab.rename_field.select_all();
        cx.notify();
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let tab = self.tab_mut();
        let Some(renaming) = tab.renaming.take() else {
            return;
        };
        let target_name = tab.rename_field.text().trim().to_owned();
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
                cx.new(|_| Ghost { name, position })
            })
            .external_drag_payload::<DragEntry>(|dragged: &DragEntry, _, _| {
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
            // the focused rename editor: highlighted selection, an
            // accent bar for a caret
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .id("rename-box")
                .debug_selector(|| "rename-box".into())
                .border_1()
                .border_color(theme::accent())
                .rounded_sm()
                .px_1()
                .children(input::field_children(&tab.rename_field, false, true, ""))
        } else if let Some(rel) = &entry.rel {
            // recursive-search hit: name plus where it lives
            div()
                .id("name-deep")
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
            div().id("name-plain").flex_1().min_w_0().truncate().child(entry.name.clone())
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
        let renaming = self
            .tab()
            .renaming
            .as_ref()
            .is_some_and(|path| *path == entry.path);
        let rename_box = renaming.then(|| {
            let tab = self.tab();
            let (before, selected, after) = input::spans(&tab.rename_field, false);
            (before, selected, after)
        });

        // thumbnails only for local image files: trash entries point at
        // paths that no longer exist
        let show_thumb =
            !in_trash && !entry.is_dir && (icons::is_thumbable(&entry.name) || icons::is_epub(&entry.name));
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
                        cx.new(|_| Ghost { name, position })
                },
            )
            .external_drag_payload::<DragEntry>(|dragged: &DragEntry, _, _| {
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
            .child(if let Some((before, selected, after)) = rename_box {
                // the rename edit box, same as the list row's
                div()
                    .id("rename-box")
                    .debug_selector(|| "rename-box".into())
                    .w_full()
                    .flex()
                    .h(px(16. * s))
                    .border_1()
                    .border_color(theme::accent())
                    .rounded_sm()
                    .px_1()
                    .text_size(px(12. * s))
                    .overflow_hidden()
                    .children(input::span_children(&before, selected, &after, true, ""))
            } else if entry.rel.is_some() {
                // deep search hit: a second dim line under the name
                div()
                    .id("tile-name-deep")
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
                    .id("tile-name")
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
        let here = !place.stale && self.tab().current_dir() == Some(place.path.as_path());
        let path = place.path.clone();
        let unbookmark_path = place.path.clone();
        let unmount_path = place.path.clone();
        let reconnect_path = place.path.clone();
        let mount_kind = place.mount;
        let stale = place.stale;
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
                if stale {
                    // nothing to navigate into: offer the reconnect
                    this.reconnect_place(path.clone(), cx);
                } else {
                    this.load_source(Source::Dir(path.clone()), cx);
                }
            }))
            // pinned places can be unpinned from their row menu;
            // mounts disconnect or eject; stale network bookmarks
            // reconnect
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    if stale {
                        this.open_menu(
                            f32::from(event.position.x),
                            f32::from(event.position.y),
                            vec![
                                MenuItem::new(
                                    "Connect...",
                                    MenuAction::Reconnect(reconnect_path.clone()),
                                ),
                                MenuItem::new(
                                    "Remove Bookmark",
                                    MenuAction::Unbookmark(unbookmark_path.clone()),
                                ),
                            ],
                        );
                    } else if let Some(kind) = mount_kind {
                        this.open_menu(
                            f32::from(event.position.x),
                            f32::from(event.position.y),
                            vec![MenuItem::new(
                                match kind {
                                    MountKind::Network => "Disconnect",
                                    MountKind::Removable => "Eject",
                                },
                                MenuAction::Unmount(unmount_path.clone(), kind),
                            )],
                        );
                    } else if bookmarked {
                        this.open_menu(
                            f32::from(event.position.x),
                            f32::from(event.position.y),
                            vec![MenuItem::new(
                                "Remove Bookmark",
                                MenuAction::Unbookmark(unbookmark_path.clone()),
                            )],
                        );
                    }
                    cx.notify();
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
            .child(
                // mount labels run long (host/share strings): keep
                // them inside the rail instead of painting over the
                // file pane
                div()
                    .id("place-name")
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(if stale {
                        // say why the click does not navigate
                        format!("{} (not connected)", place.name)
                    } else {
                        place.name.clone()
                    }),
            )
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
    /// Render-time sync for the details pane's video poster: while a
    /// video sits selected with the pane open, arm a settle kick on
    /// the first paint and spawn one poster decode (256, the same fn
    /// the Quick Look pane uses) plus one header read once the
    /// selection has rested out the delay. A fast arrow-through a
    /// video folder therefore spends one or two in-process decodes,
    /// not one per entry passed. The grid and the search preview
    /// keep type icons, always.
    fn sync_sel_video(&mut self, cx: &mut Context<Self>) {
        if !self.inspector || self.quicklook.is_some() {
            // nothing feeds while the pane is closed or Quick Look
            // owns the preview; a spent kick re-arms on its return
            self.sel_video_kick = None;
            return;
        }
        let entry = self
            .tab()
            .cursor
            .and_then(|ix| self.tab().entries.get(ix).cloned());
        let entry = match entry {
            Some(entry) if !entry.is_dir && icons::is_video(&entry.name) => entry,
            _ => {
                // not a video under the cursor: nothing may paint
                // from a stale slot, and a resting timer must learn
                // it lost the race
                if let Some((_, poster)) = self.sel_video.take()
                    && let VideoPoster::Frame(image) = poster
                {
                    cx.drop_image(image, None);
                }
                self.sel_video_len = None;
                self.sel_video_kick = None;
                self.sel_video_gen += 1;
                return;
            }
        };
        let key = entry.key.clone();
        if self.sel_video.as_ref().is_some_and(|(k, _)| *k == key)
            && self.sel_video_len.as_ref().is_some_and(|(k, _)| *k == key)
        {
            return; // both landed for this selection
        }
        if self.sel_video_kick.as_ref().is_some_and(|(k, _)| *k == key) {
            return; // settle already armed (or spent) for this selection
        }
        self.sel_video_len = None;
        self.sel_video_kick = Some((key.clone(), entry.path.clone()));
        self.sel_video_gen += 1;
        let generation = self.sel_video_gen;
        let path = entry.path.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(200))
                .await;
            let still = this
                .update(cx, |this, _| this.sel_video_gen == generation)
                .unwrap_or(false);
            if !still {
                return;
            }
            let bg_path = path.clone();
            let poster = cx
                .background_spawn(async move {
                    std::panic::catch_unwind(|| crate::video::decode_poster(&bg_path, 256))
                        .unwrap_or(Err(crate::video::PosterFail::Empty))
                })
                .await;
            let bg_path = path.clone();
            let len = cx
                .background_spawn(async move {
                    crate::video::probe(&bg_path).and_then(|(secs, _, _)| {
                        crate::video::duration_label(secs)
                    })
                })
                .await;
            let update = this.update(cx, |this, cx| {
                if this.sel_video_gen != generation {
                    return; // the selection moved on; both lands are stale
                }
                match poster {
                    Ok(render) => {
                        this.replace_sel_video(
                            key.clone(),
                            VideoPoster::Frame(Arc::new(render)),
                            cx,
                        );
                    }
                    // the named state: libav parsed the container but
                    // has no decoder for the codec
                    Err(crate::video::PosterFail::CodecMissing) => {
                        this.replace_sel_video(key.clone(), VideoPoster::CodecMissing, cx);
                    }
                    // fail-soft: the icon stands in, the kick stays
                    // spent so the misses do not respawn
                    Err(crate::video::PosterFail::Empty) => {}
                }
                if let Some(len) = len {
                    this.sel_video_len = Some((key.clone(), len));
                }
                cx.notify();
            });
            if let Err(err) = update {
                log::error!("details video poster failed: {err:#}");
            }
        })
        .detach();
    }

    /// The length row's value for the selected video, when its
    /// header read landed under the entry's key.
    fn video_len_label(&self, entry: &Entry) -> Option<String> {
        self.sel_video_len
            .as_ref()
            .filter(|(k, _)| *k == entry.key)
            .map(|(_, len)| len.clone())
    }

    /// Swap the details pane's poster slot, releasing the old
    /// frame's atlas tile when the slot held one (ADR-0016: the img
    /// element never drops the tile it paints).
    fn replace_sel_video(&mut self, key: PathBuf, poster: VideoPoster, cx: &mut Context<Self>) {
        if let Some((_, old)) = self.sel_video.replace((key, poster))
            && let VideoPoster::Frame(old) = old
        {
            cx.drop_image(old, None);
        }
    }

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
        } else if let Some(poster) = entry.and_then(|e| {
            self.sel_video
                .as_ref()
                .filter(|(k, _)| *k == e.key)
                .map(|(_, p)| p.clone())
        }) {
            match poster {
                VideoPoster::Frame(render) => img(ImageSource::Render(render))
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    .into_any_element(),
                // the named state: libav parsed the container but
                // this system's libav has no decoder for the codec
                // (Fedora's stripped libavcodec-free, say). Honest
                // degradation, never a generic failure
                VideoPoster::CodecMissing => div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(
                        entry
                            .map(|e| self.entry_icon(e, px(56.)))
                            .unwrap_or_else(|| div().into_any_element()),
                    )
                    .child(
                        div()
                            .debug_selector(|| "codec-missing".into())
                            .max_w(px(180.))
                            .text_size(px(11.))
                            .text_color(theme::text_dim())
                            .child("codec unavailable in this system's libav"),
                    )
                    .into_any_element(),
            }
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
            .children(entry.and_then(|e| self.video_len_label(&e)).map(|len| {
                prop_row("length", len)
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

    /// The Quick Look overlay: a full-pane takeover above the
    /// listing. Nothing renders when closed. Images decode at pane
    /// scale and zoom/pan; text renders the rail's snippet at read
    /// size, scrollable; dirs and unknown files render a card.
    fn quicklook_overlay(&self, window: &Window, cx: &mut Context<Self>) -> Option<Div> {
        self.quicklook.as_ref()?;
        let cursor = self.tab().cursor;
        let entry = cursor
            .and_then(|ix| self.tab().entries.get(ix))
            .cloned()
            .filter(|entry| entry.item.is_none());
        let Some(entry) = entry else {
            // the listing moved out from under the overlay: a bare
            // catcher so nothing beneath is clickable; the next sync
            // (or this click) closes it
            return Some(
                div()
                    .absolute()
                    .inset_0()
                    .occlude()
                    .bg(theme::sidebar())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.close_quicklook(cx)),
                    ),
            );
        };
        let visible = self.visible_indices();
        let pos = cursor.and_then(|ix| visible.iter().position(|&v| v == ix));

        // the paging indicator: PDFs count pages from 1; epubs count
        // spine chapters from 1 with the cover at 0, so the cover
        // carries no label, though the hint and the forward chevron
        // still wake as soon as the count lands
        let is_pdf = !entry.is_dir && icons::is_pdf(&entry.name);
        let is_epub = !entry.is_dir && icons::is_epub(&entry.name);
        let paging = self
            .quicklook
            .as_ref()
            .filter(|_| is_pdf || is_epub)
            .and_then(|ql| ql.pages.map(|pages| (ql.page, pages)));
        let book_at = self.ql_book_at;
        let page_label = paging.and_then(|(page, pages)| {
            if is_pdf {
                Some(format!("page {page} / {pages}"))
            } else if book_at >= 1 {
                Some(format!("chapter {book_at} / {pages}"))
            } else {
                None
            }
        });

        // the book glance: dc title and author when the epub told us
        let book_label = self.ql_book.as_ref().and_then(|book| {
            if entry.is_dir || !icons::is_epub(&entry.name) {
                return None;
            }
            let mut label = book.title.clone().unwrap_or_default();
            if let Some(author) = &book.author {
                if !label.is_empty() {
                    label.push_str(" · ");
                }
                label.push_str(author);
            }
            if label.is_empty() { None } else { Some(label) }
        });
        // a video's glance: duration and dimensions straight from
        // the container header ("3:25 · 1920×1080"), no decode
        let video_label = self.ql_video_len.as_ref().and_then(|(p, label)| {
            (*p == entry.path && !entry.is_dir && icons::is_video(&entry.name))
                .then(|| label.clone())
        });

        // the top row: name, position in the listing, the close road
        let header = div()
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .flex_none()
            .h(px(40.))
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text())
                    .truncate()
                    .child(entry.name.clone()),
            )
            .children(book_label.or(video_label).map(|label| {
                div()
                    .text_size(px(12.))
                    .text_color(theme::text_dim())
                    .truncate()
                    .child(label)
            }))
            .children(paging.is_some().then(|| {
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(self.quicklook_chevron("quicklook-page-back", "‹", -1, cx))
                    .children(page_label.clone().map(|label| {
                        div()
                            .text_size(px(12.))
                            .text_color(theme::text_dim())
                            .child(label)
                    }))
                    .child(self.quicklook_chevron("quicklook-page-fwd", "›", 1, cx))
            }))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::text_dim())
                    .child(match pos {
                        Some(pos) => format!("{} of {}", pos + 1, visible.len()),
                        None => String::new(),
                    }),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::text_dim())
                    .child(if paging.is_some() {
                        let word = if is_epub { "chapters" } else { "pages" };
                        format!("PgUp/PgDn or Shift+arrows {word} · Esc closes")
                    } else {
                        "Esc closes".to_string()
                    }),
            )
            .child(
                div()
                    .id("quicklook-close")
                    .cursor_pointer()
                    .px_2()
                    .rounded_sm()
                    .text_size(px(16.))
                    .text_color(theme::text_dim())
                    .hover(|this| this.text_color(theme::text()).bg(theme::row_hover()))
                    .on_click(cx.listener(|this, _, _, cx| this.close_quicklook(cx)))
                    .child("×"),
            );

        // the pane: what renders depends on what the entry is.
        // An epub's reading pane wins once the reader has left the
        // cover; the cover lingers until the first chapter's blocks
        // land (same book, close content: the stale-tile rule
        // applied to text). A held tile for this file stays up while
        // the next page decodes; file flips drop the tile in sync,
        // so a foreign file never shows here
        let ql_page = self.quicklook.as_ref().map(|ql| ql.page).unwrap_or(0);
        let reading = is_epub && self.ql_book_at >= 1 && !self.ql_text_blocks.is_empty();
        let image = self.ql_render.as_ref().and_then(|(path, _, render)| {
            // on an epub only the cover (page 0) shows as an image
            (path == &entry.path && !entry.is_dir && (!is_epub || ql_page == 0))
                .then(|| render.clone())
        });
        // the whole-file read lands over the snippet once here
        let full_text = self.ql_text.as_ref().and_then(|(p, preview)| {
            (p == &entry.path && !entry.is_dir).then(|| preview.clone())
        });
        let pane: Div = if reading {
            // the whole spine as one continuous scroll: deferred
            // blocks (list, variable heights), chapters decoded in
            // order by the background chain. The wrapper flexes the
            // list into a definite height: Auto-sized deferred
            // elements are zero-content divs to taffy
            let blocks = self.ql_text_blocks.clone();
            div()
                .flex_1()
                .overflow_hidden()
                .m_3()
                .flex()
                .flex_col()
                .child(list(self.ql_book_state.clone(), move |ix, _, _| {
                    chapter_block_view(&blocks[ix])
                        .debug_selector(|| "ql-chapter-block".into())
                        .into_any_element()
                })
                .flex_1())
        } else if let Some(render) = image {
            let viewport = window.viewport_size();
            // the pane's own budget: this overlay's top row and the
            // pane's margin, which the layout below owns
            let avail_w = (viewport.width - px(48.)).max(px(1.)).into();
            let avail_h = (viewport.height - px(76.)).max(px(1.)).into();
            let natural = render.size(0);
            let natural = (
                u32::from(natural.width) as f32,
                u32::from(natural.height) as f32,
            );
            let fit = ql_fit_scale(natural, (avail_w, avail_h));
            let zoom = self
                .quicklook
                .as_ref()
                .map(|ql| ql.zoom.clamp(1.0, QL_ZOOM_MAX))
                .unwrap_or(1.0);
            let display = (natural.0 * fit * zoom, natural.1 * fit * zoom);
            let pan = self
                .quicklook
                .as_ref()
                .map(|ql| ql.pan)
                .unwrap_or((0.0, 0.0));
            let (pan_x, pan_y) = ql_clamp_pan(pan, display, (avail_w, avail_h));
            // symmetric opposed margins: in a centered flex the net
            // shift is (ml - mr) / 2, so this is the pan, plain
            // flexbox, no positioning semantics
            div()
                .flex_1()
                .overflow_hidden()
                .flex()
                .items_center()
                .justify_center()
                .m_3()
                .child(
                    img(ImageSource::Render(render))
                        .w(px(display.0))
                        .h(px(display.1))
                        .ml(px(pan_x))
                        .mr(px(-pan_x))
                        .mt(px(pan_y))
                        .mb(px(-pan_y)),
                )
        } else if let Some(preview) = full_text {
            // the whole-file reading pane: deferred rows (uniform
            // list for lines, list for markdown blocks) so a 2 MB
            // file renders only what is on screen
            match preview.kind {
                TextKind::Markdown => match preview.blocks.filter(|b| !b.is_empty()) {
                    Some(blocks) => div()
                        .flex_1()
                        .overflow_hidden()
                        .m_3()
                        .flex()
                        .flex_col()
                        .child(list(self.ql_md_state.clone(), move |ix, _, _| {
                            div()
                                .text_size(px(13.))
                                .pb_2()
                                .text_color(theme::text())
                                .debug_selector(|| "ql-md-block".into())
                                .child(md_block_view(&blocks[ix]))
                                .into_any_element()
                        })
                        .flex_1()),
                    None => line_list_pane(preview.lines.clone(), &self.ql_text_scroll),
                },
                TextKind::Csv | TextKind::Code | TextKind::Plain => {
                    line_list_pane(preview.lines.clone(), &self.ql_text_scroll)
                }
            }
        } else if !entry.is_dir && self.preview_key.as_ref() == Some(&entry.key)
            && self.text_preview.is_some()
        {
            let text = self.text_preview.as_ref().unwrap();
            let mono = div()
                .w_full()
                .text_size(px(13.))
                .text_color(theme::text())
                .font_family("monospace");
            let lines: Div = match text.kind {
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
                    .flex()
                    .flex_col()
                    .gap_2()
                    .text_size(px(13.))
                    .text_color(theme::text())
                    .children(md_blocks(text)),
                TextKind::Plain => mono.children(
                    text.lines
                        .iter()
                        .map(|line| preview_line(line.clone()).text_color(theme::text())),
                ),
            };
            div()
                .flex_1()
                .overflow_hidden()
                .m_3()
                .child(
                    div()
                        .id("quicklook-text")
                        .overflow_y_scroll()
                        .track_scroll(&self.ql_scroll)
                        .w_full()
                        .child(lines),
                )
        } else {
            // dirs, undecoded/failed page files, unknown types: the
            // card. Enter still hands the file to the system.
            let kind = if entry.is_dir {
                "Folder".to_string()
            } else {
                let ext = Path::new(&entry.name)
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_uppercase())
                    .unwrap_or_else(|| "file".into());
                let size = entry
                    .size
                    .map(|size| format!(" · {}", human_size(size)))
                    .unwrap_or_default();
                format!("{ext} file{size}")
            };
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .m_3()
                .child(self.entry_icon(&entry, px(96.)))
                .child(
                    div()
                        .text_size(px(14.))
                        .text_color(theme::text())
                        .child(entry.name.clone()),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::text_dim())
                        .child(kind),
                )
                // the named state: the container parses but this
                // system's libav has no decoder for the codec (a
                // stripped libavcodec-free, say). Honest, specific
                .children(
                    self.ql_failed
                        .as_ref()
                        .is_some_and(|(p, fail)| {
                            *p == entry.path && *fail == crate::video::PosterFail::CodecMissing
                        })
                        .then(|| {
                            div()
                                .debug_selector(|| "ql-codec-missing".into())
                                .max_w(px(320.))
                                .text_size(px(12.))
                                .text_color(theme::text_dim())
                                .child("codec unavailable in this system's libav")
                        }),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::text_dim())
                        .child("Enter opens with the system"),
                )
        };

        // the image pane's mouse: wheel zooms, a press+move pans (the
        // clamps live in ql_clamp_pan at render), release drops the
        // drag. On non-image panes the handlers idle.
        let drag_pane = pane
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                let Some(entry) = this
                    .tab()
                    .cursor
                    .and_then(|ix| this.tab().entries.get(ix))
                    .filter(|entry| entry.item.is_none())
                    .cloned()
                else {
                    return;
                };
                if entry.is_dir || !icons::is_thumbable(&entry.name) || this.ql_render.is_none() {
                    return;
                }
                let Some(ql) = this.quicklook.as_mut() else {
                    return;
                };
                // one wheel notch (3 lines) is about 1/1.2, same as
                // the +/- keys; trackpads land in between smoothly
                let dy: f32 = event.delta.pixel_delta(px(20.)).y.into();
                ql.zoom = (ql.zoom * (-dy * 0.003f32).exp()).clamp(1.0, QL_ZOOM_MAX);
                if ql.zoom <= 1.0 {
                    ql.pan = (0.0, 0.0);
                }
                cx.notify();
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, event: &MouseDownEvent, _, _| {
                // any press arms a pan; at fit the clamps hold the
                // pan at zero, so only a zoomed image moves
                if let Some(ql) = this.quicklook.as_mut() {
                    ql.drag_from = Some((event.position, ql.pan));
                }
            }))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                let Some(ql) = this.quicklook.as_mut() else {
                    return;
                };
                let Some((from, from_pan)) = ql.drag_from else {
                    return;
                };
                if event.pressed_button != Some(MouseButton::Left) {
                    ql.drag_from = None;
                    cx.notify();
                    return;
                }
                let dx: f32 = (event.position.x - from.x).into();
                let dy: f32 = (event.position.y - from.y).into();
                ql.pan = (from_pan.0 + dx, from_pan.1 + dy);
                cx.notify();
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| {
                if let Some(ql) = this.quicklook.as_mut()
                    && ql.drag_from.take().is_some()
                {
                    cx.notify();
                }
            }));

        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .flex_col()
                .bg(theme::sidebar())
                .child(header)
                .child(drag_pane),
        )
    }

    /// The Connect to Server dialog. Before the pump runs: the URI
    /// field. At a prompt: the gio prompt text and a masked (or plain)
    /// answer field. Between prompts: the status line alone.
    fn connect_overlay(&self, cx: &mut Context<Self>) -> Option<Div> {        let dialog = self.connect.as_ref()?;
        let at_prompt = dialog.session.is_some();
        let asking = dialog.prompt.is_some();
        // the protocol picker and its field rows, before the pump
        // runs; then the locked server line and the one question at a
        // time answer field
        let picker = (!at_prompt).then(|| {
            let buttons: Vec<AnyElement> = ConnectProto::ALL
                .into_iter()
                .map(|proto| {
                    let active = proto == dialog.proto;
                    div()
                        .id(format!("proto-{}", proto.scheme()))
                        .px_2()
                        .py_0p5()
                        .rounded_sm()
                        .text_size(px(12.))
                        .cursor_pointer()
                        .text_color(if active {
                            theme::accent()
                        } else {
                            theme::text_dim()
                        })
                        .bg(if active {
                            theme::row_selected()
                        } else {
                            theme::clear()
                        })
                        .hover(|this| this.bg(theme::row_hover()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_connect_proto(proto, cx);
                        }))
                        .child(proto.label())
                        .into_any_element()
                })
                .collect();
            div().flex().gap_1().children(buttons)
        });
        let field_rows = (!at_prompt).then(|| {
            let labels: [(&str, &str); 5] = [
                ("Host", "host or IP"),
                ("User", "user, optional"),
                ("Port", "port, optional"),
                ("Share", "share name"),
                ("Password", "optional, asked if empty"),
            ];
            let rows: Vec<AnyElement> = dialog
                .layout()
                .into_iter()
                .map(|ix| {
                    let (label, hint) = labels[ix];
                    let focused = dialog.focus == ix;
                    let masked = ix == FIELD_PASSWORD;
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .w(px(64.))
                                .text_size(px(12.))
                                .text_color(theme::text_dim())
                                .child(label),
                        )
                        .child(
                            div()
                                .id(format!("connect-box-{ix}"))
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .items_center()
                                .border_1()
                                .border_color(if focused {
                                    theme::accent()
                                } else {
                                    theme::border()
                                })
                                .rounded_sm()
                                .px_1()
                                .py_0p5()
                                .text_color(theme::text())
                                .cursor(gpui::CursorStyle::IBeam)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_connect_focus(ix, cx);
                                }))
                                .children(input::field_children(
                                    &dialog.fields[ix],
                                    masked,
                                    focused,
                                    hint,
                                )),
                        )
                        .into_any_element()
                })
                .collect();
            div().flex().flex_col().gap_1().children(rows)
        });
        let label = div()
            .text_size(px(14.))
            .text_color(theme::text())
            .child(match dialog.prompt.as_ref() {
                Some(prompt) => format!("{prompt}:"),
                None if at_prompt => "Connecting".to_string(),
                None => "Connect to Server".to_string(),
            });
        // while connecting, the server rides along (dim, locked) so
        // the one question at a time field has visible context
        let server_row = at_prompt.then(|| {
            div()
                .flex()
                .gap_2()
                .text_size(px(12.))
                .text_color(theme::text_dim())
                .child("server")
                .child(
                    div()
                        .text_color(theme::text())
                        .child(dialog.uri.trim().to_string()),
                )
        });
        let status = (!dialog.status.is_empty()).then(|| {
            div()
                .text_size(px(12.))
                .text_color(theme::text_dim())
                .child(dialog.status.clone())
        });
        let notes = dialog
            .notes
            .iter()
            .rev()
            .take(4)
            .rev()
            .map(|line| {
                div()
                    .text_size(px(11.))
                    .text_color(theme::text_dim())
                    .child(line.clone())
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
                        .child(label)
                        .children(server_row)
                        .children(picker)
                        .children(field_rows)
                        .children(notes)
                        .children(at_prompt.then(|| {
                            div()
                                .flex()
                                .items_center()
                                .border_1()
                                .border_color(theme::accent())
                                .rounded_sm()
                                .px_1()
                                .py_0p5()
                                .text_color(theme::text())
                                .children(input::field_children(
                                    &dialog.input,
                                    dialog.mask,
                                    asking,
                                    "",
                                ))
                        }))
                        .children(status)
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .text_size(px(12.))
                                .text_color(theme::text_dim())
                                .child(if at_prompt {
                                    "Enter submits, Esc cancels"
                                } else if dialog.session.is_some() {
                                    "Connecting, Esc cancels"
                                } else {
                                    "Tab moves, Enter connects, Esc closes"
                                }),
                        ),
                ),
        )
    }

    fn compress_overlay(&self, cx: &mut Context<Self>) -> Option<Div> {        let dialog = self.compress.as_ref()?;
        let count = self.tab().selection.len();
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
                                .children(input::field_children(
                                    &dialog.name,
                                    false,
                                    true,
                                    "archive name",
                                )),
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
            MenuAction::Unmount(path, kind) => self.unmount(path, kind, cx),
            MenuAction::Reconnect(path) => self.reconnect_place(path, cx),
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
        self.arm_watcher();
        self.auto_dock(window);
        self.sync_sel_video(cx);
        // this paint's thumb kicks define the wanted set; the pool
        // consults it at pop time, so off-screen queue entries die
        self.thumb_wanted.clear();
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
                            if !entry.is_dir
                                && (icons::is_thumbable(&entry.name)
                                    || icons::is_epub(&entry.name))
                            {
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

        // Quick Look owns the preview while open: it follows the
        // cursor entry (flips land here, and so do watcher
        // reshuffles). The rail's feed below keeps running on the
        // same keys and guards, so nothing double-fetches.
        if self.quicklook.is_some() {
            self.sync_quicklook(cx);
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

        // the sidebar groups into sections: Home with Recent and
        // Trash at the top, then Places (XDG dirs and bookmarks),
        // Network (network mounts and stale network bookmarks), and
        // Removable (USB drives and local devices). A header renders
        // only when its section is non-empty.
        let mut top_items: Vec<AnyElement> = Vec::new();
        let mut place_items: Vec<AnyElement> = Vec::new();
        let mut network_items: Vec<AnyElement> = Vec::new();
        let mut removable_items: Vec<AnyElement> = Vec::new();
        for (ix, place) in self.places.iter().enumerate() {
            let row = self.place_row(ix, place, cx).into_any_element();
            match place_section(place) {
                PlaceSection::Top => top_items.push(row),
                PlaceSection::Places => place_items.push(row),
                PlaceSection::Network => network_items.push(row),
                PlaceSection::Removable => removable_items.push(row),
            }
        }

        let in_trash = self.tab().source == Source::Trash;
        let in_recent = self.tab().source == Source::Recent;
        let recent_row = div()
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
            .child("Recent");
        let trash_row = div()
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
            .child("Trash");

        let section_header = |label: &'static str| {
            div()
                .px_3()
                .pt_2()
                .pb_1()
                .text_size(px(11.))
                .text_color(theme::text_dim())
                .child(label)
                .into_any_element()
        };
        let mut sidebar: Vec<AnyElement> = top_items;
        sidebar.push(recent_row.into_any_element());
        sidebar.push(trash_row.into_any_element());
        if !place_items.is_empty() {
            sidebar.push(section_header("Places"));
            sidebar.extend(place_items);
        }
        if !network_items.is_empty() {
            sidebar.push(section_header("Network"));
            sidebar.extend(network_items);
        }
        if !removable_items.is_empty() {
            sidebar.push(section_header("Removable"));
            sidebar.extend(removable_items);
        }

        let mut tabs: Vec<Stateful<Div>> = Vec::new();
        for ix in 0..self.tabs.len() {
            tabs.push(self.tab_bar_row(ix, cx));
        }

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
                    .overflow_hidden()
                    .children(sidebar)
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
                                div()
                                    .id("path-edit")
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .items_center()
                                    .border_1()
                                    .border_color(theme::accent())
                                    .rounded_sm()
                                    .px_1()
                                    .text_color(theme::text())
                                    .children(input::field_children(
                                        &self.path_field,
                                        false,
                                        true,
                                        "path",
                                    ))
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
                    .children((!self.filter.is_empty()).then(|| {
                        // the search banner: only while a filter is
                        // live, so the odd two-line rows below it
                        // explain themselves
                        let visible = self.visible_indices();
                            let entries = &self.tab().entries;
                            let here = visible
                                .iter()
                                .filter(|&&ix| entries[ix].rel.is_none())
                                .count();
                            let deep = entries.iter().filter(|e| e.rel.is_some()).count();
                            let dir_label = self
                                .tab()
                                .current_dir()
                                .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
                                .unwrap_or_default();
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_3()
                                .py_1()
                                .border_b_1()
                                .border_color(theme::border())
                                .bg(theme::row())
                                .text_size(px(12.))
                                .text_color(theme::text_dim())
                                .child(div().child("Search"))
                                .child(
                                    div()
                                        .text_color(theme::text())
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(self.filter.clone()),
                                )
                                .child(div().truncate().child(format!(
                                    "· {here} here, {deep} in subfolders of {dir_label}"
                                )))
                                .children(self.searching.then(|| {
                                    div().child("searching subfolders…")
                                }))
                                .child(div().flex_1())
                                .child(div().child("Esc clears"))
                                .child(
                                    div()
                                        .id("search-clear")
                                        .px_1()
                                        .cursor_pointer()
                                        .hover(|this| this.text_color(theme::text()))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.filter.clear();
                                            this.filter_changed(cx);
                                        }))
                                        .child("×"),
                                )
                        }),
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
                                                this.rubber_origin = Some(event.position);
                                                this.rubber_current = None;
                                                cx.notify();
                                            },
                                        ),
                                    )
                                    .on_drag(RubberSelect, |_, _, _, cx| {
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
                                            }
                                            this.rubber_current = Some(event.event.position);
                                            this.rubber_bounds = Some(event.bounds);
                                            this.rubber_ctrl =
                                                event.event.modifiers.control;
                                            cx.notify();
                                        },
                                    ))
                                    .on_drop(cx.listener(|this, _: &RubberSelect, _, cx| {
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
            .children(self.connect_overlay(cx))
            .children(self.quicklook_overlay(window, cx))
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
    /// Arc so the reading pane's deferred list closures can capture
    /// it: a whole-file read is up to tens of thousands of lines and
    /// the pane repaints per frame.
    lines: Arc<Vec<String>>,
    /// Parsed markdown, when kind is Markdown; None or empty falls
    /// back to plain lines.
    blocks: Option<Arc<Vec<MdBlock>>>,
}

/// Quick Look's own state: which entry it shows (follows the cursor),
/// the image zoom and pan, an active drag's anchor (the pointer
/// position the press started at, plus the pan it started from), and
/// the PDF page under view (`pages` is the count from pdfinfo, `None`
/// while unknown: paging hides itself). For an epub `page` stays 0,
/// the cover; the reader's position lives in `ql_book_at`.
#[derive(Clone, Debug, PartialEq)]
struct QuickLook {
    path: PathBuf,
    zoom: f32,
    pan: (f32, f32),
    drag_from: Option<(Point<Pixels>, (f32, f32))>,
    page: usize,
    pages: Option<usize>,
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

/// The line-rows reading pane: a uniform list of truncated mono
/// rows, deferred so only the visible window lays out (a 2 MB read
/// is tens of thousands of lines). The wrapper is a flex column and
/// the list grows into it: an Auto-sized deferred element is a
/// zero-content div to taffy and paints nothing without a height.
fn line_list_pane(lines: Arc<Vec<String>>, scroll: &gpui::UniformListScrollHandle) -> Div {
    let count = lines.len();
    div().flex_1().overflow_hidden().m_3().flex().flex_col().child(
        uniform_list("quicklook-text-list", count, move |range, _, _| {
            lines[range.clone()]
                .iter()
                .map(|line| {
                    div()
                        .h(px(20.))
                        .w_full()
                        .debug_selector(|| "ql-row".into())
                        .truncate()
                        .text_size(px(13.))
                        .font_family("monospace")
                        .text_color(theme::text())
                        .child(line.clone())
                })
                .collect()
        })
        .flex_1()
        .track_scroll(scroll),
    )
}

/// One chapter block for the glance pane: tag-carried typography
/// only, no CSS. Headings size by level and render bold; quotes
/// indent and dim; rules are hairlines.
fn chapter_block_view(block: &epub::ChapterBlock) -> Div {
    match block {
        epub::ChapterBlock::Rule => div().w_full().h(px(1.)).bg(theme::row_hover()),
        epub::ChapterBlock::Heading { level, text, runs } => {
            let size = match level {
                1 => 20.0,
                2 => 18.0,
                3 => 17.0,
                _ => 16.0,
            };
            div()
                .w_full()
                .pb_2()
                .text_size(px(size))
                .child(chapter_runs_view(text, runs, theme::text(), true))
        }
        epub::ChapterBlock::Para { text, runs, quote } => {
            let color = if *quote { theme::text_dim() } else { theme::text() };
            let mut pane = div().w_full().text_size(px(13.)).pb_2();
            if *quote {
                pane = pane.ml_4();
            }
            pane.child(chapter_runs_view(text, runs, color, false))
        }
    }
}

/// A block's runs as one StyledText: the family is the system UI
/// font, size and line height come from the wrapping div's text
/// style, and the runs carry slant, weight, and color. Run lengths
/// are byte counts summing to the text; the scanner guarantees it.
fn chapter_runs_view(
    text: &str,
    runs: &[epub::ChapterRun],
    color: Rgba,
    heading: bool,
) -> StyledText {
    let runs = runs
        .iter()
        .map(|run| {
            let mut font = Font::default();
            if run.italic {
                font.style = FontStyle::Italic;
            }
            if run.bold || heading {
                font.weight = FontWeight::BOLD;
            }
            TextRun {
                len: run.len,
                font,
                color: color.into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }
        })
        .collect();
    StyledText::new(text.to_string()).with_runs(runs)
}

/// The freedesktop recency write: every open road notes here, the
/// manager's Enter and the viewer's launches alike, so the Recent
/// list and other apps' stores stay one truth.
pub(crate) fn write_recent(path: &Path) {
    let Some(store) = recent_xbel_path() else {
        return;
    };
    let entries = Browser::read_recents();
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
}

/// Page count out of pdfinfo's stdout: the "Pages:" line. None when
/// the tool said nothing usable (or said zero).
pub(crate) fn parse_pdf_pages(info: &str) -> Option<usize> {
    info.lines()
        .find_map(|line| line.strip_prefix("Pages:"))
        .and_then(|rest| rest.trim().parse::<usize>().ok())
        .filter(|pages| *pages > 0)
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
    blocks.iter().map(md_block_view).collect()
}

/// One markdown block as an element; the reading pane's deferred
/// list renders it per visible index, so this is the shared shape.
fn md_block_view(block: &MdBlock) -> AnyElement {
    match block {
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
    }
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
    ("Space", "quick look"),
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

pub(crate) fn human_size(size: u64) -> String {
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
    fn thumb_eviction_drops_the_oldest_and_reports_the_droplets() {
        let mut thumbs: HashMap<PathBuf, Arc<RenderImage>> = HashMap::new();
        let mut order: VecDeque<PathBuf> = VecDeque::new();
        for i in 0..THUMB_CACHE_MAX {
            let path = PathBuf::from(format!("/tmp/thumb-{i}.png"));
            thumbs.insert(path.clone(), Arc::new(render_image()));
            order.push_back(path);
        }
        let oldest_key = order.front().unwrap().clone();

        // one over the cap: exactly one eviction, the oldest, with a
        // droplet whose tile the caller must release
        let new_key = PathBuf::from("/tmp/thumb-new.png");
        let droplets = trim_thumbs(&mut thumbs, &mut order);
        thumbs.insert(new_key.clone(), Arc::new(render_image()));
        order.push_back(new_key);

        assert_eq!(droplets.len(), 1);
        assert!(!thumbs.contains_key(&oldest_key), "the oldest left the cache");
        assert_eq!(thumbs.len(), THUMB_CACHE_MAX);
        assert!(!order.contains(&oldest_key));
    }

    #[test]
    fn thumb_trim_drains_stray_entries_when_the_order_lags() {
        // the defensive path: thumbs populated without order entries
        // (should not happen) must still come back under the cap
        let mut thumbs: HashMap<PathBuf, Arc<RenderImage>> = HashMap::new();
        let mut order: VecDeque<PathBuf> = VecDeque::new();
        for i in 0..THUMB_CACHE_MAX {
            thumbs.insert(PathBuf::from(format!("/tmp/stray-{i}.png")), Arc::new(render_image()));
        }
        let droplets = trim_thumbs(&mut thumbs, &mut order);
        assert_eq!(droplets.len(), THUMB_CACHE_MAX);
        assert!(thumbs.is_empty());
    }

    fn render_image() -> RenderImage {
        RenderImage::new(smallvec::smallvec![image::Frame::new(image::RgbaImage::new(1, 1))])
    }

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
            name: input::Field::new("crew.tar.gz"),
            zip: false,
            zip_available: true,
        };
        Browser::set_zip_format(&mut dialog, true);
        assert_eq!(dialog.name.text(), "crew.zip");
        assert!(dialog.zip);
        Browser::set_zip_format(&mut dialog, false);
        assert_eq!(dialog.name.text(), "crew.tar.gz");

        // custom names without a known suffix stay as typed; Create
        // appends the chosen suffix at build time
        dialog.name = input::Field::new("backup 2026");
        Browser::set_zip_format(&mut dialog, true);
        assert_eq!(dialog.name.text(), "backup 2026");
        assert!(dialog.zip);
        Browser::set_zip_format(&mut dialog, false);
        assert_eq!(dialog.name.text(), "backup 2026");
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

    pub(crate) struct Lab {
        pub(crate) dir: PathBuf,
        src: PathBuf,
    }

    impl Lab {
        /// dir holds the browser's listing (a.txt, b.txt, c.txt, sub),
        /// src holds the clipboard payloads of the same names. The
        /// name keeps parallel tests out of each other's dirs.
        pub(crate) fn new(name: &str) -> Self {
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

    pub(crate) fn key(k: &str) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke::parse(k).unwrap(),
            is_held: false,
            prefer_character_input: false,
        }
    }

    pub(crate) fn open_browser(
        app: &mut gpui::TestApp,
        dir: &Path,
    ) -> gpui::TestAppWindow<Browser> {
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
    fn space_opens_quick_look_over_the_cursor_and_toggles_shut() {
        let lab = Lab::new("quicklook-toggle");
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            // dirs sort first: sub, a.txt, b.txt, c.txt
            browser.jump_cursor(1, cx);
            browser.route_key(&key("space"), cx);
            assert!(browser
                .quicklook
                .as_ref()
                .is_some_and(|ql| ql.path == lab.dir.join("a.txt")));
            // the text pipeline fed for the entry under the overlay
            assert_eq!(browser.preview_key.as_ref(), Some(&lab.dir.join("a.txt")));
            // space again closes and keeps the cursor
            browser.route_key(&key("space"), cx);
            assert!(browser.quicklook.is_none());
            assert_eq!(browser.tab().cursor, Some(1));
            // escape closes too
            browser.route_key(&key("space"), cx);
            browser.route_key(&key("escape"), cx);
            assert!(browser.quicklook.is_none());
        });
        app.run_until_parked();
    }

    #[test]
    fn quick_look_flip_follows_the_listing_and_resets_the_image() {
        let lab = Lab::new("quicklook-flip");
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            browser.jump_cursor(1, cx); // a.txt
            browser.route_key(&key("space"), cx);
            if let Some(ql) = browser.quicklook.as_mut() {
                ql.zoom = 4.0;
                ql.pan = (100.0, -50.0);
            }
            // down: to b.txt, and the image state resets
            browser.route_key(&key("down"), cx);
            assert!(browser
                .quicklook
                .as_ref()
                .is_some_and(|ql| ql.path == lab.dir.join("b.txt")));
            assert_eq!(browser.quicklook.as_ref().unwrap().zoom, 1.0);
            assert_eq!(browser.quicklook.as_ref().unwrap().pan, (0.0, 0.0));
            // up onto the dir card, where a further up clamps
            browser.route_key(&key("up"), cx);
            browser.route_key(&key("up"), cx);
            assert!(browser
                .quicklook
                .as_ref()
                .is_some_and(|ql| ql.path == lab.dir.join("sub")));
            // the listing's cursor followed, so a later close leaves
            // the browser parked on the same entry
            browser.route_key(&key("space"), cx);
            assert_eq!(browser.tab().cursor, Some(0));
        });
        app.run_until_parked();
    }

    #[test]
    fn quick_look_flip_respects_the_filter() {
        let lab = Lab::new("quicklook-filter");
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            // set directly: the flip path reads visible_indices, no
            // deep search involved
            browser.filter = "b.txt".into();
            browser.snap_cursor_visible();
            browser.route_key(&key("space"), cx);
            assert!(browser
                .quicklook
                .as_ref()
                .is_some_and(|ql| ql.path == lab.dir.join("b.txt")));
            // the only visible entry: flipping clamps in place
            browser.route_key(&key("down"), cx);
            browser.route_key(&key("down"), cx);
            assert!(browser
                .quicklook
                .as_ref()
                .is_some_and(|ql| ql.path == lab.dir.join("b.txt")));
        });
        app.run_until_parked();
    }

    #[test]
    fn quick_look_enter_hands_off_or_navigates() {
        let lab = Lab::new("quicklook-enter");
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            // enter on a folder card navigates (and closes first)
            browser.jump_cursor(0, cx); // sub
            browser.route_key(&key("space"), cx);
            assert!(browser.quicklook.is_some());
            browser.route_key(&key("enter"), cx);
            assert!(browser.quicklook.is_none());
            assert_eq!(
                browser.tab().current_dir().map(Path::to_path_buf),
                Some(lab.dir.join("sub"))
            );
        });
        app.run_until_parked();
    }

    #[test]
    fn quick_look_image_decode_lands_and_flips_drop_it() {
        let lab = Lab::new("quicklook-image");
        // a tiny real png, written before the browser loads so the
        // listing sees it; dirs first, so pic sorts last
        let pic = lab.dir.join("pic.png");
        image::DynamicImage::new_rgb8(8, 8)
            .save_with_format(&pic, image::ImageFormat::Png)
            .unwrap();
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            browser.jump_cursor(4, cx); // pic.png
            browser.route_key(&key("space"), cx);
            assert!(browser.quicklook.is_some());
        });
        app.run_until_parked(); // the background decode lands
        window.update(|browser, _, cx| {
            assert!(browser
                .ql_render
                .as_ref()
                .is_some_and(|(p, _, _)| *p == pic));
            // flip to a text file: the held tile drops (ADR-0016)
            browser.route_key(&key("up"), cx);
            assert!(browser.ql_render.is_none());
        });
        app.run_until_parked();
    }

    #[test]
    fn quick_look_zoom_and_pan_clamp() {
        // contain: the tighter ratio wins
        assert_eq!(ql_fit_scale((2000.0, 1000.0), (100.0, 100.0)), 0.05);
        // degenerate sizes read as fit instead of dividing by zero
        assert_eq!(ql_fit_scale((0.0, 1000.0), (100.0, 100.0)), 1.0);
        assert_eq!(ql_fit_scale((100.0, 100.0), (0.0, 0.0)), 1.0);
        // pan clamps to half the overflow each way, zero at fit
        assert_eq!(
            ql_clamp_pan((5000.0, 0.0), (2000.0, 1000.0), (1000.0, 1000.0)),
            (500.0, 0.0)
        );
        assert_eq!(
            ql_clamp_pan((5000.0, 0.0), (1000.0, 1000.0), (1000.0, 1000.0)),
            (0.0, 0.0)
        );
    }

    #[test]
    fn pdf_pages_parse_from_pdfinfo_output() {
        assert_eq!(parse_pdf_pages("Pages:          12\n"), Some(12));
        assert_eq!(
            parse_pdf_pages("Title: report\nCreator: x\nPages: 3\n"),
            Some(3)
        );
        // nothing usable: no line, zero, or garbage
        assert_eq!(parse_pdf_pages("Title: report\n"), None);
        assert_eq!(parse_pdf_pages("Pages: 0\n"), None);
        assert_eq!(parse_pdf_pages("Pages: many\n"), None);
    }

    #[test]
    fn quick_look_page_keys_clamp_and_noop_without_a_count() {
        let lab = Lab::new("quicklook-pages");
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            browser.jump_cursor(1, cx); // a.txt
            browser.route_key(&key("space"), cx);
            // count unknown: paging hides itself entirely
            browser.route_key(&key("pagedown"), cx);
            assert_eq!(browser.quicklook.as_ref().unwrap().page, 1);
            // simulate pdfinfo landing
            browser.quicklook.as_mut().unwrap().pages = Some(3);
            browser.route_key(&key("pagedown"), cx);
            browser.route_key(&key("pagedown"), cx);
            assert_eq!(browser.quicklook.as_ref().unwrap().page, 3);
            browser.route_key(&key("pagedown"), cx);
            assert_eq!(browser.quicklook.as_ref().unwrap().page, 3);
            browser.route_key(&key("pageup"), cx);
            assert_eq!(browser.quicklook.as_ref().unwrap().page, 2);
            browser.route_key(&key("home"), cx);
            assert_eq!(browser.quicklook.as_ref().unwrap().page, 1);
            browser.route_key(&key("end"), cx);
            assert_eq!(browser.quicklook.as_ref().unwrap().page, 3);
            // a page is a new picture: zoom and pan reset with it
            if let Some(ql) = browser.quicklook.as_mut() {
                ql.zoom = 4.0;
                ql.pan = (100.0, 100.0);
            }
            browser.route_key(&key("pageup"), cx);
            let ql = browser.quicklook.as_ref().unwrap();
            assert_eq!(ql.page, 2);
            assert_eq!(ql.zoom, 1.0);
            assert_eq!(ql.pan, (0.0, 0.0));
        });
        app.run_until_parked();
    }

    #[test]
    fn quick_look_shift_arrows_page_without_moving_the_file() {
        let lab = Lab::new("quicklook-shift-pages");
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            browser.jump_cursor(1, cx); // a.txt
            browser.route_key(&key("space"), cx);
            browser.quicklook.as_mut().unwrap().pages = Some(3);
            // shift+down: a page turn, not a file flip
            let mut shift_down = key("down");
            shift_down.keystroke.modifiers.shift = true;
            browser.route_key(&shift_down, cx);
            let ql = browser.quicklook.as_ref().unwrap();
            assert_eq!(ql.page, 2);
            assert_eq!(ql.path, lab.dir.join("a.txt"));
            // plain down still flips the file (and resets the page)
            browser.route_key(&key("down"), cx);
            let ql = browser.quicklook.as_ref().unwrap();
            assert_eq!(ql.path, lab.dir.join("b.txt"));
            assert_eq!(ql.page, 1);
            // shift+up at the first page: clamped no-op
            browser.quicklook.as_mut().unwrap().pages = Some(3);
            let mut shift_up = key("up");
            shift_up.keystroke.modifiers.shift = true;
            browser.route_key(&shift_up, cx);
            assert_eq!(browser.quicklook.as_ref().unwrap().page, 1);
            // count unknown: shift+arrows idle too
            browser.quicklook.as_mut().unwrap().pages = None;
            browser.route_key(&shift_down, cx);
            assert_eq!(browser.quicklook.as_ref().unwrap().page, 1);
        });
        app.run_until_parked();
    }

    #[test]
    fn rename_opens_with_the_name_selected_so_typing_replaces() {
        let lab = Lab::new("rename-select-all");
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            browser.jump_cursor(1, cx); // a.txt
            browser.start_rename(cx);
            assert_eq!(browser.tab().rename_field.sel, Some((0, 5)));
            // a typed character replaces the whole selection
            let mut typed = key("x");
            typed.keystroke.key_char = Some("X".into());
            browser.route_key(&typed, cx);
            assert_eq!(browser.tab().rename_field.text(), "X");
            browser.route_key(&key("enter"), cx);
        });
        app.run_until_parked();
        assert!(lab.dir.join("X").exists());
        assert!(!lab.dir.join("a.txt").exists());
    }

    #[test]
    fn quick_look_epub_lands_cover_and_metadata() {
        let lab = Lab::new("quicklook-epub");
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let opf = br#"<?xml version="1.0"?>
            <package xmlns="http://www.idpf.org/2007/opf" version="3.0">
              <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
                <dc:title>Neuromancer</dc:title>
                <dc:creator>William Gibson</dc:creator>
              </metadata>
              <manifest>
                <item id="cover" href="cover.png" properties="cover-image"/>
              </manifest>
              <spine/>
            </package>"#;
        let container = br#"<?xml version="1.0"?>
            <container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
              <rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles>
            </container>"#;
        let bytes = crate::epub::tests::write_epub(&[
            ("META-INF/container.xml", container),
            ("OEBPS/content.opf", opf),
            ("OEBPS/cover.png", &png),
        ]);
        fs::write(lab.dir.join("zz.epub"), bytes).unwrap();
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            // dirs first: sub, a.txt, b.txt, c.txt, zz.epub
            browser.jump_cursor(4, cx);
            browser.route_key(&key("space"), cx);
            assert!(browser.quicklook.is_some());
        });
        app.run_until_parked(); // the background container read lands
        window.update(|browser, _, _| {
            let book = browser.ql_book.as_ref().unwrap();
            assert_eq!(book.title.as_deref(), Some("Neuromancer"));
            assert_eq!(book.author.as_deref(), Some("William Gibson"));
            assert!(browser
                .ql_render
                .as_ref()
                .is_some_and(|(p, pg, _)| *p == lab.dir.join("zz.epub") && *pg == 0));
        });
        app.run_until_parked();
    }

    #[test]
    fn quick_look_text_reads_the_whole_file() {
        let lab = Lab::new("quicklook-fulltext");
        // dirs first: sub, then a.txt, big.txt, blob.bin, c.txt, note.md
        let body: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
        fs::write(lab.dir.join("big.txt"), &body).unwrap();
        fs::write(lab.dir.join("blob.bin"), [0u8; 4096]).unwrap();
        fs::write(lab.dir.join("note.md"), "# Title\n\nhello world\n".repeat(10)).unwrap();
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            // entries: sub, a.txt, b.txt, big.txt, blob.bin, c.txt, note.md
            browser.jump_cursor(4, cx); // blob.bin
            browser.route_key(&key("space"), cx);
            assert!(browser.quicklook.is_some());
        });
        app.run_until_parked(); // the binary read fails into the slot
        window.update(|browser, _, cx| {
            assert!(browser.ql_text.is_none());
            assert_eq!(
                browser.ql_text_failed.as_deref(),
                Some(lab.dir.join("blob.bin").as_path())
            );
            // flip to c.txt: a plain one-line read
            browser.quicklook_flip(1, cx);
        });
        app.run_until_parked();
        window.update(|browser, _, cx| {
            let (p, preview) = browser.ql_text.as_ref().unwrap();
            assert_eq!(*p, lab.dir.join("c.txt"));
            assert_eq!(preview.lines.last().unwrap(), "old-c");
            // flip on to note.md: blocks parse
            browser.quicklook_flip(2, cx);
        });
        app.run_until_parked();
        window.update(|browser, window, _| {
            let (p, preview) = browser.ql_text.as_ref().unwrap();
            assert_eq!(*p, lab.dir.join("note.md"));
            let blocks = preview.blocks.as_ref().unwrap();
            assert!(blocks.len() >= 10, "markdown blocks unparsed");
            // paint-level guard for the other deferred list too
            let block = window
                .debug_element_bounds("ql-md-block")
                .expect("markdown pane never painted a block");
            assert!(block.size.height > px(0.));
        });
        // the whole-file read of big.txt checked apart
        window.update(|browser, _, cx| {
            browser.close_quicklook(cx);
            browser.jump_cursor(3, cx); // big.txt
            browser.route_key(&key("space"), cx);
        });
        app.run_until_parked();
        window.update(|browser, window, _| {
            let (p, preview) = browser.ql_text.as_ref().unwrap();
            assert_eq!(*p, lab.dir.join("big.txt"));
            assert_eq!(preview.lines.len(), 20_000);
            assert_eq!(preview.lines.first().unwrap(), "line 0");
            assert_eq!(preview.lines.last().unwrap(), "line 19999");
            // paint-level guard: a row really painted, with height.
            // An Auto-sized deferred list without a flexed height is
            // a zero-content div to taffy: a blank pane paints
            // nothing
            let row = window
                .debug_element_bounds("ql-row")
                .expect("reading pane never painted a row");
            assert!(row.size.height > px(0.));
        });
    }

    #[test]
    fn quick_look_text_caps_the_read_and_each_line() {
        let lab = Lab::new("quicklook-fulltext-cap");
        // past the read cap: head plus a tail marker ("abc\n" is 4
        // bytes, so the cap cuts on a whole line)
        let body = "abc\n".repeat(900_000); // 3.6 MB
        fs::write(lab.dir.join("huge.txt"), &body).unwrap();
        // one enormous line caps per line instead of one huge layout
        let wide = format!("w{}x\n", "o".repeat(TEXT_LINE_MAX * 2));
        fs::write(lab.dir.join("a.txt"), &wide).unwrap();
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            browser.jump_cursor(1, cx); // a.txt: dirs first, then a.txt
            browser.route_key(&key("space"), cx);
        });
        app.run_until_parked();
        window.update(|browser, _, cx| {
            let (_, preview) = browser.ql_text.as_ref().unwrap();
            // one line, capped at TEXT_LINE_MAX chars plus the mark
            assert_eq!(preview.lines.len(), 1);
            assert_eq!(preview.lines[0].len(), TEXT_LINE_MAX + "\u{2026}".len());
            // over b.txt and c.txt to the huge file
            browser.quicklook_flip(3, cx);
        });
        app.run_until_parked();
        window.update(|browser, _, _| {
            let (_, preview) = browser.ql_text.as_ref().unwrap();
            assert_eq!(
                preview.lines.len(),
                TEXT_READ_MAX / 4 + 1,
                "head of the file plus the tail marker"
            );
            assert!(preview.lines.last().unwrap().contains("continues"));
        });
    }

    #[test]
    fn quick_look_epub_reads_continuously_and_the_pager_jumps_chapters() {
        let lab = Lab::new("quicklook-epub-continuous");
        let opf = br#"<?xml version="1.0"?>
            <package xmlns="http://www.idpf.org/2007/opf" version="3.0">
              <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
                <dc:title>Neuromancer</dc:title>
                <dc:creator>William Gibson</dc:creator>
              </metadata>
              <manifest>
                <item id="c1" href="ch1.xhtml"/>
                <item id="c2" href="ch2.xhtml"/>
              </manifest>
              <spine><itemref idref="c1"/><itemref idref="c2"/></spine>
            </package>"#;
        let container = br#"<?xml version="1.0"?>
            <container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
              <rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles>
            </container>"#;
        let ch1: &[u8] = b"<html><body><h1>First</h1><p>alpha body text</p></body></html>";
        let ch2: &[u8] = b"<html><body><p>beta body text</p></body></html>";
        let bytes = crate::epub::tests::write_epub(&[
            ("META-INF/container.xml", container),
            ("OEBPS/content.opf", opf),
            ("OEBPS/ch1.xhtml", ch1),
            ("OEBPS/ch2.xhtml", ch2),
        ]);
        fs::write(lab.dir.join("zz.epub"), bytes).unwrap();
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            // dirs first: sub, a.txt, b.txt, c.txt, zz.epub
            browser.jump_cursor(4, cx);
            browser.route_key(&key("space"), cx);
            assert!(browser.quicklook.is_some());
        });
        app.run_until_parked(); // count, cover, and the whole spine land
        window.update(|browser, _, cx| {
            let ql = browser.quicklook.as_ref().unwrap();
            // an epub opens on its cover: position 0, count known
            assert_eq!(ql.page, 0);
            assert_eq!(ql.pages, Some(2));
            assert_eq!(browser.ql_book_at, 0);
            // the chain decoded the whole spine while the cover showed
            let blocks = browser.ql_text_blocks.clone();
            assert_eq!(blocks.len(), 3, "h1 + para, then the second para");
            assert_eq!(browser.ql_text_starts, vec![0, 2]);
            let text = crate::epub::tests::blocks_text(&blocks);
            assert!(text.contains("alpha body text"));
            assert!(text.contains("beta body text"));
            // pagedown leaves the cover for chapter 1
            browser.route_key(&key("pagedown"), cx);
            assert_eq!(browser.ql_book_at, 1);
        });
        app.run_until_parked(); // the reading pane paints
        window.update(|_, window, _| {
            // paint-level guard: the deferred list really painted a
            // block, with height
            let block = window
                .debug_element_bounds("ql-chapter-block")
                .expect("the epub reading pane never painted a block");
            assert!(block.size.height > px(0.));
        });window.update(|browser, _, cx| {
            // pagedown: chapter 2
            browser.route_key(&key("pagedown"), cx);
            assert_eq!(browser.ql_book_at, 2);
            // pagedown at the last chapter: clamped no-op
            browser.route_key(&key("pagedown"), cx);
            assert_eq!(browser.ql_book_at, 2);
            // pageup: chapter 1, then the cover, floored there
            browser.route_key(&key("pageup"), cx);
            assert_eq!(browser.ql_book_at, 1);
            browser.route_key(&key("pageup"), cx);
            assert_eq!(browser.ql_book_at, 0);
            browser.route_key(&key("pageup"), cx);
            assert_eq!(browser.ql_book_at, 0);
            // end and home: the last chapter, then the first
            browser.route_key(&key("end"), cx);
            assert_eq!(browser.ql_book_at, 2);
            browser.route_key(&key("home"), cx);
            assert_eq!(browser.ql_book_at, 1);
            // back to the cover: the text pane stays; the cover shows
            browser.route_key(&key("pageup"), cx);
            assert_eq!(browser.ql_book_at, 0);
            // flipping files resets the reader: chain killed, panes empty
            browser.quicklook_flip(-1, cx);
        });
        app.run_until_parked();
        window.update(|browser, _, _| {
            assert!(browser.ql_text_blocks.is_empty());
            assert!(browser.ql_text_starts.is_empty());
            assert_eq!(browser.ql_text_for, None);
            assert_eq!(browser.ql_book_at, 0);
        });
    }

    #[test]
    fn quick_look_epub_chain_marks_a_failed_chapter_and_stops() {
        let lab = Lab::new("quicklook-epub-gap");
        let opf = br#"<?xml version="1.0"?>
            <package xmlns="http://www.idpf.org/2007/opf" version="3.0">
              <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"/>
              <manifest>
                <item id="c1" href="ch1.xhtml"/>
                <item id="c2" href="missing.xhtml"/>
                <item id="c3" href="ch3.xhtml"/>
              </manifest>
              <spine><itemref idref="c1"/><itemref idref="c2"/><itemref idref="c3"/></spine>
            </package>"#;
        let container = br#"<?xml version="1.0"?>
            <container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
              <rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles>
            </container>"#;
        let ch1: &[u8] = b"<html><body><p>alpha body text</p></body></html>";
        let ch3: &[u8] = b"<html><body><p>gamma body text</p></body></html>";
        let bytes = crate::epub::tests::write_epub(&[
            ("META-INF/container.xml", container),
            ("OEBPS/content.opf", opf),
            ("OEBPS/ch1.xhtml", ch1),
            ("OEBPS/ch3.xhtml", ch3),
        ]);
        fs::write(lab.dir.join("zz.epub"), bytes).unwrap();
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            browser.jump_cursor(4, cx); // zz.epub
            browser.route_key(&key("space"), cx);
        });
        app.run_until_parked(); // the chain runs and hits the gap
        window.update(|browser, _, _| {
            let blocks = browser.ql_text_blocks.clone();
            let text = crate::epub::tests::blocks_text(&blocks);
            assert!(text.contains("alpha body text"));
            assert!(text.contains("chapter 2 failed to read"));
            // the chain stopped: chapter 3 never renders behind the gap
            assert!(!text.contains("gamma body text"));
            // the marker is a chapter for the pager too
            assert_eq!(browser.ql_text_starts.len(), 2);
            assert_eq!(browser.quicklook.as_ref().unwrap().pages, Some(3));
        });
    }
    #[test]
    fn quick_look_video_fails_soft_into_the_card() {
        let lab = Lab::new("quicklook-video");
        // a junk mp4 decodes to nothing in-process: the poster
        // fails soft, the card stands in, and the header ask is
        // spent quietly
        fs::write(lab.dir.join("clip.mp4"), b"not a video").unwrap();
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            // dirs first: sub, a.txt, b.txt, c.txt, clip.mp4
            browser.jump_cursor(4, cx);
            browser.route_key(&key("space"), cx);
            assert!(browser.quicklook.is_some());
        });
        app.run_until_parked(); // the failed decode lands
        window.update(|browser, _, _| {
            assert_eq!(
                browser
                    .ql_failed
                    .as_ref()
                    .map(|(p, _)| p.as_path()),
                Some(lab.dir.join("clip.mp4").as_path())
            );
            // one ask, no label: nothing respawns per frame
            assert!(browser.video_meta_asked);
            assert!(browser.ql_video_len.is_none());
        });
    }

    #[test]
    fn details_video_codec_missing_is_named() {
        let lab = Lab::new("video-codec-missing");
        // whether libav lacks a codec's decoder is a runtime fact
        // of the system (the free build ships the common decoders,
        // so no file in this container can trigger it end to end):
        // inject the named state and assert the pane says so
        fs::write(lab.dir.join("clip.mp4"), b"not a video").unwrap();
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, window, cx| {
            // dirs first: sub, a.txt, b.txt, c.txt, clip.mp4
            browser.jump_cursor(4, cx);
            browser.sel_video = Some((lab.dir.join("clip.mp4"), VideoPoster::CodecMissing));
            cx.notify();
            window.refresh();
        });
        window.update(|browser, window, _| {
            let bounds = window
                .debug_element_bounds("codec-missing")
                .expect("the codec-missing caption never painted");
            assert!(bounds.size.width > px(0.) && bounds.size.height > px(0.));
            // the slot survives the pane's sync: the matching key
            // is not cleared, not overwritten by the fail-soft
            assert!(matches!(
                browser.sel_video.as_ref().map(|(_, poster)| poster),
                Some(VideoPoster::CodecMissing)
            ));
        });
    }

    #[test]
    fn quick_look_video_codec_missing_says_so() {
        let lab = Lab::new("quicklook-codec-missing");
        fs::write(lab.dir.join("clip.mp4"), b"not a video").unwrap();
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            // dirs first: sub, a.txt, b.txt, c.txt, clip.mp4
            browser.jump_cursor(4, cx);
            browser.route_key(&key("space"), cx);
            assert!(browser.quicklook.is_some());
        });
        app.run_until_parked(); // the junk mp4's decode fails fast
        window.update(|browser, window, cx| {
            // the generic card landed; now the named state (a
            // runtime fact of the system's libav, injected here)
            browser.ql_failed = Some((
                lab.dir.join("clip.mp4"),
                crate::video::PosterFail::CodecMissing,
            ));
            cx.notify();
            window.refresh();
        });
        window.update(|browser, window, _| {
            let bounds = window
                .debug_element_bounds("ql-codec-missing")
                .expect("the codec-missing line never painted");
            assert!(bounds.size.width > px(0.) && bounds.size.height > px(0.));
            // the failed slot stops the respawn loop: the card
            // stands in, nothing re-decodes
            assert!(browser.ql_render.is_none());
            assert!(browser.quicklook.is_some());
        });
    }

    #[test]
    fn quick_look_corrupt_epub_fails_into_the_card() {
        let lab = Lab::new("quicklook-epub-bad");
        fs::write(lab.dir.join("zz.epub"), b"not a zip").unwrap();
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            browser.jump_cursor(4, cx); // zz.epub
            browser.route_key(&key("space"), cx);
            assert!(browser.quicklook.is_some());
        });
        app.run_until_parked(); // the failed decode lands
        window.update(|browser, _, _| {
            assert_eq!(
                browser.ql_failed.as_ref().map(|(p, _)| p.as_path()),
                Some(lab.dir.join("zz.epub").as_path())
            );
            assert!(browser.ql_render.is_none());
            // still open, showing the card
            assert!(browser.quicklook.is_some());
        });
        app.run_until_parked();
    }

    #[test]
    fn details_pane_shows_epub_covers_and_skips_corrupt_ones() {
        let lab = Lab::new("rail-epub-thumb");
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let opf = br#"<?xml version="1.0"?>
            <package xmlns="http://www.idpf.org/2007/opf" version="3.0">
              <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
                <dc:title>Neuromancer</dc:title>
              </metadata>
              <manifest>
                <item id="cover" href="cover.png" properties="cover-image"/>
              </manifest>
              <spine/>
            </package>"#;
        let container = br#"<?xml version="1.0"?>
            <container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
              <rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles>
            </container>"#;
        fs::write(
            lab.dir.join("zz.epub"),
            crate::epub::tests::write_epub(&[
                ("META-INF/container.xml", container),
                ("OEBPS/content.opf", opf),
                ("OEBPS/cover.png", &png),
            ]),
        )
        .unwrap();
        // a second, corrupt epub: decode lands empty, no thumb
        fs::write(lab.dir.join("bad.epub"), b"not a zip").unwrap();
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            browser.inspector = true;
            // dirs first: sub, a.txt, b.txt, c.txt, bad.epub, zz.epub
            browser.jump_cursor(5, cx);
        });
        app.run_until_parked(); // the rail feed + cover decode land
        window.update(|browser, _, _| {
            assert!(browser.thumbs.contains_key(&lab.dir.join("zz.epub")));
            assert!(!browser.thumbs.contains_key(&lab.dir.join("bad.epub")));
        });
        app.run_until_parked();
    }

    #[test]
    fn details_video_settles_then_fails_soft() {
        let lab = Lab::new("details-video");
        // a junk mp4 decodes to nothing in-process: the poster and
        // the length both come up empty, the icon stands in, and
        // the spent kick keeps the misses from respawning
        fs::write(lab.dir.join("clip.mp4"), b"not a video").unwrap();
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            // dirs first: sub, a.txt, b.txt, c.txt, clip.mp4
            browser.jump_cursor(4, cx);
        });
        app.run_until_parked(); // the render arms the settle kick
        window.update(|browser, _, _| {
            assert!(browser.sel_video_kick.is_some());
            assert!(browser.sel_video.is_none());
        });
        app.advance_clock(std::time::Duration::from_millis(250));
        app.run_until_parked(); // the timer wakes, the decodes fail soft
        window.update(|browser, _, _| {
            // spent, not re-armed: no respawn per frame
            assert!(browser.sel_video_kick.is_some());
            assert!(browser.sel_video.is_none());
            assert!(browser.sel_video_len.is_none());
        });
        // selecting a non-video drops everything and kills the timer
        window.update(|browser, _, cx| {
            browser.jump_cursor(1, cx); // a.txt
        });
        app.run_until_parked();
        window.update(|browser, _, _| {
            assert!(browser.sel_video_kick.is_none());
            assert!(browser.sel_video_gen > 1);
        });
    }

    #[test]
    fn details_video_length_row_follows_the_key() {
        let lab = Lab::new("details-video-len");
        fs::write(lab.dir.join("clip.mp4"), b"not a video").unwrap();
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, _| {
            let key = browser.tab().entries[4].key.clone();
            browser.sel_video_len = Some((key, "3:25".to_string()));
            let entry = browser.tab().entries[4].clone();
            assert_eq!(browser.video_len_label(&entry).as_deref(), Some("3:25"));
            let other = browser.tab().entries[1].clone();
            assert_eq!(browser.video_len_label(&other), None);
        });
    }

    #[test]
    fn thumb_pool_bounds_and_cancels_queued() {
        let lab = Lab::new("thumb-pool");
        // eight decodable pngs; more kicks than pool slots
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(8, 8)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let mut paths = Vec::new();
        for i in 0..8 {
            let path = lab.dir.join(format!("img{i}.png"));
            fs::write(&path, &png).unwrap();
            paths.push(path);
        }
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            // the first draw's paint kicks may have queued, started,
            // or even landed some of these against the default cache
            // root: clear every slot so this update's own requests
            // are the whole experiment
            browser.thumbs.clear();
            browser.thumbs_inflight.clear();
            browser.thumbs_queue.clear();
            browser.thumb_wanted.clear();
            for path in &paths {
                browser.request_thumb(path.clone(), cx);
            }
            // the pool bounds concurrency no matter how many kicks,
            // whatever landed or started before these
            assert!(browser.thumbs_inflight.len() <= 3);
            assert_eq!(
                browser.thumbs.len()
                    + browser.thumbs_inflight.len()
                    + browser.thumbs_queue.len(),
                8
            );
            // every path scrolled away from: the queue dies at pop
            // time instead of spending decodes
            browser.thumb_wanted.clear();
            browser.thumbs_inflight.clear();
            browser.pump_thumbs(cx);
            assert!(browser.thumbs_queue.is_empty());
            assert!(browser.thumbs_inflight.is_empty());
            // wanted again: fresh kicks re-enqueue, and the pump
            // starts at most three
            for path in &paths {
                browser.request_thumb(path.clone(), cx);
            }
            assert_eq!(browser.thumbs_inflight.len(), 3);
        });
        app.run_until_parked();
        window.update(|browser, _, _| {
            // started jobs always land, whatever the repaints did to
            // wanted; in list mode nothing re-kicks, so the queue's
            // unstarted entries died exactly as designed
            assert!(browser.thumbs.len() >= 3);
            for path in browser.thumbs.keys() {
                assert!(paths.contains(path));
            }
        });
    }

    #[test]
    fn enter_on_a_claimed_type_opens_the_viewer() {
        let lab = Lab::new("enter-viewer");
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(8, 8)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        fs::write(lab.dir.join("a.png"), &png).unwrap();
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            let ix = browser
                .tab()
                .entries
                .iter()
                .position(|e| e.name == "a.png")
                .expect("a.png in the listing");
            browser.jump_cursor(ix, cx);
            browser.route_key(&key("enter"), cx);
            assert_eq!(browser.status, "opened a.png");
        });
        app.run_until_parked();
        // the viewer window owns the file; the grid never shelled
        // out to xdg-open on this road. The pane is probed through
        // the viewer's own window, after a refresh so a frame drew.
        app.update(|cx| {
            let handle = cx
                .try_global::<crate::viewer::Hosts>()
                .expect("viewer host global")
                .viewer
                .as_ref()
                .expect("no viewer window")
                .clone();
            let shown = handle
                .update(cx, |_, window, _| {
                    window.refresh();
                    window.debug_element_bounds("viewer-pane").is_some()
                })
                .unwrap();
            assert!(shown, "the viewer pane never painted");
        });
        app.update(|cx| {
            let handle = cx
                .try_global::<crate::viewer::Hosts>()
                .expect("viewer host global")
                .viewer
                .as_ref()
                .expect("no viewer window")
                .clone();
            handle
                .update(cx, |viewer, _, _| {
                    assert_eq!(viewer.path, lab.dir.join("a.png"));
                })
                .unwrap();
        });
    }

    #[test]
    fn enter_on_an_unclaimed_type_still_hands_off() {
        let lab = Lab::new("enter-unclaimed");
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            // dirs first: sub, a.txt, b.txt, c.txt; a.txt is no kind
            // the open surface claims
            browser.jump_cursor(1, cx);
            browser.route_key(&key("enter"), cx);
            // the handoff road ran (its status wording depends on
            // the env's xdg-open); the viewer stayed shut
            assert_ne!(browser.status, "opened a.txt");
        });
        app.update(|cx| {
            let hosts = cx.try_global::<crate::viewer::Hosts>();
            assert!(hosts.map_or(true, |hosts| hosts.viewer.is_none()));
        });
    }

    #[test]
    fn desktop_file_claims_what_the_surface_renders() {
        let text = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging/kuma-files.desktop.in"),
        )
        .unwrap();
        let exec = text
            .lines()
            .find(|line| line.starts_with("Exec="))
            .expect("Exec line");
        // glib passes the file only through a field code: without
        // one the mimetype claims are inert
        assert!(exec.ends_with(" %u"), "Exec lacks a %u field code: {exec}");
        let mime_line = text
            .lines()
            .find(|line| line.starts_with("MimeType="))
            .expect("MimeType line");
        let claimed: Vec<&str> = mime_line
            .strip_prefix("MimeType=")
            .unwrap()
            .split(';')
            .filter(|t| !t.is_empty())
            .collect();
        for mime in [
            "inode/directory",
            "x-scheme-handler/file",
            "image/png",
            "image/jpeg",
            "image/webp",
            "image/gif",
            "image/tiff",
            "application/pdf",
        ] {
            assert!(claimed.contains(&mime), "{mime} not claimed");
        }
        // the claim follows the surface: no decoder for these yet
        assert!(!claimed.contains(&"image/avif"));
        assert!(!claimed.contains(&"image/svg+xml"));
    }

    #[test]
    fn thumb_disk_cache_survives_and_shields_the_source() {
        let lab = Lab::new("thumb-cache");
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(8, 8)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let path = lab.dir.join("photo.png");
        fs::write(&path, &png).unwrap();
        let cache = std::env::temp_dir().join(format!("kuma-thumb-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&cache);
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            browser.thumb_cache = cache.clone();
            // a first-draw paint kick may have started a job against
            // the default cache root before this update ran: clear
            // the slots so this request is the one that runs, and it
            // runs against the test's own cache root
            browser.thumbs.clear();
            browser.thumbs_inflight.clear();
            browser.thumbs_queue.clear();
            browser.thumb_wanted.clear();
            browser.request_thumb(path.clone(), cx);
        });
        app.run_until_parked();
        window.update(|browser, _, _| {
            assert!(browser.thumbs.contains_key(&path));
        });
        // the cache root now holds exactly one PNG
        let mut entries = fs::read_dir(&cache).unwrap();
        let cached = entries.next().unwrap().unwrap().path();
        assert!(entries.next().is_none());
        assert_eq!(cached.extension().unwrap(), "png");

        // poison the source but restore its (mtime, size): the key
        // must not move, and the cache read must shield the decode
        // from the corrupt file
        let meta = fs::metadata(&path).unwrap();
        let mtime = meta.modified().unwrap();
        fs::write(&path, vec![0u8; png.len()]).unwrap();
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_times(
            std::fs::FileTimes::new().set_modified(mtime),
        )
        .unwrap();
        drop(file);

        // a fresh browser is the restart case: empty memory, warm
        // disk
        let mut window2 = open_browser(&mut app, &lab.dir);
        window2.update(|browser, _, cx| {
            browser.thumb_cache = cache.clone();
            // same clean slate as the first phase: the first draw's
            // paint kick must not run this test's job against the
            // default cache root (or the poisoned source)
            browser.thumbs.clear();
            browser.thumbs_inflight.clear();
            browser.thumbs_queue.clear();
            browser.thumb_wanted.clear();
            browser.request_thumb(path.clone(), cx);
        });
        app.run_until_parked();
        window2.update(|browser, _, _| {
            assert!(browser.thumbs.contains_key(&path));
            let bytes = browser.thumbs[&path].as_bytes(0).unwrap().to_vec();
            assert!(bytes.iter().any(|b| *b != 0));
        });
        let _ = fs::remove_dir_all(&cache);
    }

    #[test]
    #[cfg(feature = "video")]
    fn video_posters_decode_through_the_grid_pipeline() {
        let lab = Lab::new("thumb-video");
        // the committed fixture decodes in-process: no CLI, no GPU
        fs::copy("tests/fixtures/sample.mp4", lab.dir.join("a.mp4")).unwrap();
        fs::copy("tests/fixtures/sample.mp4", lab.dir.join("b.mp4")).unwrap();
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            browser.request_thumb(lab.dir.join("a.mp4"), cx);
            browser.request_thumb(lab.dir.join("b.mp4"), cx);
            assert!(icons::is_thumbable("a.mp4"));
        });
        app.run_until_parked();
        window.update(|browser, _, _| {
            assert!(browser.thumbs.contains_key(&lab.dir.join("a.mp4")));
            assert!(browser.thumbs.contains_key(&lab.dir.join("b.mp4")));
            let bytes = browser.thumbs[&lab.dir.join("a.mp4")]
                .as_bytes(0)
                .unwrap();
            assert!(bytes.iter().any(|b| *b != 0));
        });
    }

    #[test]
    fn quick_look_file_flip_resets_pages() {
        let lab = Lab::new("quicklook-page-flip");
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            browser.jump_cursor(1, cx); // a.txt
            browser.route_key(&key("space"), cx);
            browser.quicklook.as_mut().unwrap().page = 2;
            browser.quicklook.as_mut().unwrap().pages = Some(5);
            browser.route_key(&key("down"), cx); // b.txt
            let ql = browser.quicklook.as_ref().unwrap();
            assert_eq!(ql.path, lab.dir.join("b.txt"));
            assert_eq!(ql.page, 1);
            assert_eq!(ql.pages, None);
        });
        app.run_until_parked();
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
        // the walker debounces on a simulated 150ms timer
        app.advance_clock(std::time::Duration::from_millis(200));
        app.run_until_parked();
        window.update(|browser, _, _| {
            let deep: Vec<&Entry> =
                browser.tab().entries.iter().filter(|e| e.rel.is_some()).collect();
            assert_eq!(deep.len(), 2, "walker results never landed");
            assert!(browser.tab().entries.iter().any(|e| e.path == tree.root.join("sub").join("needle.txt")));
            assert!(!browser.searching, "walk finished but the banner still says searching");
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

/// Rename end to end: start (F2 path), type, commit, and the file
/// actually moving on disk.
#[cfg(test)]
mod browser_rename {
    use super::*;
    use gpui::{Keystroke, TestApp};

    fn key(k: &str) -> KeyDownEvent {
        let mut keystroke = Keystroke::parse(k).unwrap();
        // printable keys arrive with key_char on the real input path;
        // the rename buffer inserts from key_char
        if keystroke.key.chars().count() == 1 {
            keystroke.key_char = Some(keystroke.key.clone().into());
        }
        KeyDownEvent {
            keystroke,
            is_held: false,
            prefer_character_input: false,
        }
    }

    #[test]
    fn rename_commits_to_disk() {
        let dir =
            std::env::temp_dir().join(format!("koguma-rename-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("notes.txt"), "hello").unwrap();

        let mut app = TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = app.open_window(|window, cx| {
            Browser::new(Some(dir.clone()), window, cx)
        });
        app.run_until_parked();
        window.update(|browser, _, cx| {
            browser.tab_mut().cursor = Some(0);
            browser.start_rename(cx);
            assert!(browser.tab().renaming.is_some(), "F2 path never entered rename");
            // the name opens selected (typing replaces it); end
            // collapses to the caret, then append a 2 and commit
            assert_eq!(browser.tab().rename_field.sel, Some((0, 9)));
            browser.route_key(&key("end"), cx);
            browser.route_key(&key("2"), cx);
            assert_eq!(browser.tab().rename_field.text(), "notes.txt2");
            browser.route_key(&key("enter"), cx);
        });
        app.run_until_parked();
        assert!(
            !dir.join("notes.txt").exists(),
            "the old name is still there; rename never committed"
        );
        assert_eq!(fs::read_to_string(dir.join("notes.txt2")).unwrap(), "hello");
        window.update(|browser, _, _| {
            assert!(browser.tab().renaming.is_none());
        });
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_renders_its_box_in_icon_view() {
        let dir =
            std::env::temp_dir().join(format!("koguma-rename-icon-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("notes.txt"), "hello").unwrap();

        let mut app = TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = app.open_window(|window, cx| {
            Browser::new(Some(dir.clone()), window, cx)
        });
        app.run_until_parked();
        window.update(|browser, _, cx| {
            browser.set_view_mode(ViewMode::Icons, cx);
            browser.tab_mut().cursor = Some(0);
            browser.start_rename(cx);
        });
        app.run_until_parked();
        // the edit box must actually paint: before this fix, icon view
        // started renames with no visible box and silently ate keys
        let painted = window.update(|_, window, _| {
            window
                .debug_element_bounds("rename-box")
                .map(|b| b.size.width > gpui::px(0.))
                .unwrap_or(false)
        });
        assert!(painted, "icon view never painted the rename box");
        // and the flow still commits (end collapses the opening
        // selection to the caret, then append a 2)
        window.update(|browser, _, cx| {
            browser.route_key(&key("end"), cx);
            browser.route_key(&key("2"), cx);
            browser.route_key(&key("enter"), cx);
        });
        app.run_until_parked();
        assert!(dir.join("notes.txt2").exists(), "icon-view rename never committed");
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod connect_tests {
    use super::browser_ux_keys::{key, open_browser, Lab};
    use super::*;

    /// Paste into the connect dialog: a pasted URI fills the protocol
    /// and its fields (a pasted password is dropped), plain text
    /// inserts into the focused field.
    #[test]
    fn connect_dialog_paste_fills_the_form_from_a_uri() {
        let lab = Lab::new("connect-dialog-paste");
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            browser.open_connect_dialog(cx);
            let dialog = browser.connect.as_ref().unwrap();
            assert_eq!(dialog.proto, ConnectProto::Sftp);
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                "smb://bob:hidden@NAS/media".to_string(),
            ));
            let mut v = key("v");
            v.keystroke.modifiers.control = true;
            browser.route_key(&v, cx);
        });
        app.run_until_parked();
        window.update(|browser, _, cx| {
            let dialog = browser.connect.as_ref().unwrap();
            assert_eq!(dialog.proto, ConnectProto::Smb);
            assert_eq!(dialog.fields[FIELD_HOST].text(), "NAS");
            assert_eq!(dialog.fields[FIELD_USER].text(), "bob");
            // a pasted password never lands in a field: gio re-asks
            assert_eq!(dialog.fields[FIELD_PORT].text(), "");
            assert_eq!(dialog.fields[FIELD_SHARE].text(), "media");
            // the caret parks on the last visible field (Password),
            // so Enter connects
            assert_eq!(dialog.focus, dialog.last_visible());
            // plain paste goes to the focused field: click through to
            // Share first, the way the user would
            browser.set_connect_focus(FIELD_SHARE, cx);
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("/sub".to_string()));
            let mut v2 = key("v");
            v2.keystroke.modifiers.control = true;
            browser.route_key(&v2, cx);
        });
        app.run_until_parked();
        window.update(|browser, _, _| {
            let dialog = browser.connect.as_ref().unwrap();
            assert_eq!(dialog.fields[FIELD_SHARE].text(), "media/sub");
        });
    }

    /// Ctrl+A in a connect field selects the whole buffer; typing
    /// replaces the selection.
    #[test]
    fn connect_dialog_ctrl_a_selects_all() {
        let lab = Lab::new("connect-dialog-ctrl-a");
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            browser.open_connect_dialog(cx);
            let dialog = browser.connect.as_mut().unwrap();
            dialog.fields[FIELD_HOST] = input::Field::new("nas.local");
            let mut a = key("a");
            a.keystroke.modifiers.control = true;
            browser.route_key(&a, cx);
        });
        app.run_until_parked();
        window.update(|browser, _, cx| {
            let dialog = browser.connect.as_mut().unwrap();
            assert_eq!(dialog.fields[FIELD_HOST].sel, Some((0, 9)));
            // typing over the selection replaces it (printable keys
            // ride key_char, as the real window delivers them)
            let mut n = key("n");
            n.keystroke.key_char = Some("n".into());
            browser.route_key(&n, cx);
            let dialog = browser.connect.as_ref().unwrap();
            assert_eq!(dialog.fields[FIELD_HOST].text(), "n");
        });
    }

    /// Clicking a field box moves the caret there: the next typed
    /// characters land in that field, not the one the keyboard left.
    #[test]
    fn clicking_a_connect_field_moves_the_caret_there() {
        let lab = Lab::new("connect-dialog-click-focus");
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        window.update(|browser, _, cx| {
            browser.open_connect_dialog(cx);
            let dialog = browser.connect.as_mut().unwrap();
            dialog.fields[FIELD_HOST] = input::Field::new("nas.local");
            browser.set_connect_focus(FIELD_USER, cx);
            let mut b = key("b");
            b.keystroke.key_char = Some("b".into());
            browser.route_key(&b, cx);
        });
        app.run_until_parked();
        window.update(|browser, _, _| {
            let dialog = browser.connect.as_ref().unwrap();
            assert_eq!(dialog.focus, FIELD_USER);
            assert_eq!(dialog.fields[FIELD_USER].text(), "b");
            assert_eq!(dialog.fields[FIELD_HOST].text(), "nas.local");
        });
    }

    #[test]
    fn server_uris_compose_and_parse_round_trip() {
        let uri = compose_server_uri(ConnectProto::Smb, "NAS", "bob", "", "media");
        assert_eq!(uri, "smb://bob@NAS/media");
        let uri = compose_server_uri(ConnectProto::Sftp, "box", "ann", "2222", "");
        assert_eq!(uri, "sftp://ann@box:2222");
        // a non-digit port is not composed: it would break gio
        let uri = compose_server_uri(ConnectProto::Ftp, "box", "", "abc", "");
        assert_eq!(uri, "ftp://box");
        // a share with slashes keeps its path
        let uri = compose_server_uri(ConnectProto::Smb, "NAS", "", "", "/media/sub");
        assert_eq!(uri, "smb://NAS/media/sub");

        let (proto, host, user, port, share) =
            parse_server_uri("sftp://ann@box:2222").unwrap();
        assert_eq!(
            (proto, host.as_str(), user.as_str(), port.as_str(), share.as_str()),
            (ConnectProto::Sftp, "box", "ann", "2222", "")
        );
        let (proto, host, user, port, share) =
            parse_server_uri("smb://bob:hidden@NAS/media/sub").unwrap();
        assert_eq!(
            (proto, host.as_str(), user.as_str(), port.as_str(), share.as_str()),
            (ConnectProto::Smb, "NAS", "bob", "", "media/sub")
        );
        // bracketed IPv6
        let (proto, host, _user, port, _share) =
            parse_server_uri("sftp://[::1]:2222").unwrap();
        assert_eq!(
            (proto, host.as_str(), port.as_str()),
            (ConnectProto::Sftp, "::1", "2222")
        );
        // a scheme the dialog does not speak
        assert!(parse_server_uri("webdav://box").is_none());
        assert!(parse_server_uri("no scheme at all").is_none());
    }

    #[test]
    fn gvfs_names_become_place_labels() {
        assert_eq!(mount_label("sftp:host=localhost"), "localhost (sftp)");
        assert_eq!(mount_label("smb:host=NAS,share=media"), "NAS/media (smb)");
        // the smb backend's real dir name shape
        assert_eq!(
            mount_label("smb-share:server=NAS,share=media"),
            "NAS/media (smb)"
        );
        // no parseable host: pass the raw name through
        assert_eq!(mount_label("mtp:[usb:003,004]"), "mtp:[usb:003,004]");
        assert_eq!(mount_label("External Drive"), "External Drive");
    }

    #[test]
    fn gvfs_names_parse_back_to_reconnect_uris() {
        assert_eq!(
            uri_from_gvfs_name("smb-share:server=NAS,share=media").as_deref(),
            Some("smb://NAS/media")
        );
        assert_eq!(
            uri_from_gvfs_name("sftp:host=localhost").as_deref(),
            Some("sftp://localhost")
        );
        // user rides the URI as user@host, so gio re-asks only for
        // the password, not the name
        assert_eq!(
            uri_from_gvfs_name("sftp:host=localhost,user=bob").as_deref(),
            Some("sftp://bob@localhost")
        );
        // the backend's `;N` repeat counter strips off
        assert_eq!(
            uri_from_gvfs_name("smb-share:server=NAS,share=media;2").as_deref(),
            Some("smb://NAS/media")
        );
        // local and junk backends have nothing to reconnect to
        assert_eq!(uri_from_gvfs_name("mtp:[usb:003,004]"), None);
        assert_eq!(uri_from_gvfs_name("External Drive"), None);
        assert_eq!(uri_from_gvfs_name("burn://"), None);
    }

    #[test]
    fn gvfs_mounts_split_network_from_local_devices() {
        assert_eq!(
            gvfs_mount_kind("smb-share:server=NAS,share=media"),
            MountKind::Network
        );
        assert_eq!(gvfs_mount_kind("sftp:host=localhost"), MountKind::Network);
        assert_eq!(gvfs_mount_kind("webdav:host=box"), MountKind::Network);
        // local-device backends ride with the removable drives
        assert_eq!(gvfs_mount_kind("mtp:[usb:003,004]"), MountKind::Removable);
        assert_eq!(gvfs_mount_kind("gphoto2:[usb:002]"), MountKind::Removable);
        assert_eq!(gvfs_mount_kind("afc:host=iPod"), MountKind::Removable);
    }

    #[test]
    fn places_group_into_sections() {
        let place = |name: &str, path: PathBuf, bookmark: bool, mount: Option<MountKind>, stale: bool| {
            Place {
                name: name.into(),
                path,
                bookmark,
                mount,
                stale,
            }
        };
        let home = dirs::home_dir().expect("tests run with a home dir");
        assert_eq!(
            place_section(&place("Home", home.clone(), false, None, false)),
            PlaceSection::Top
        );
        assert_eq!(
            place_section(&place(
                "Documents",
                home.join("Documents"),
                false,
                None,
                false
            )),
            PlaceSection::Places
        );
        assert_eq!(
            place_section(&place("Notes", home.join("notes"), true, None, false)),
            PlaceSection::Places
        );
        let share = PathBuf::from("/run/user/1/gvfs/smb-share:server=NAS,share=media");
        assert_eq!(
            place_section(&place("NAS/media", share.clone(), false, Some(MountKind::Network), false)),
            PlaceSection::Network
        );
        // a stale network bookmark keeps its network seat
        assert_eq!(
            place_section(&place("NAS/media", share, true, None, true)),
            PlaceSection::Network
        );
        assert_eq!(
            place_section(&place(
                "Stick",
                PathBuf::from("/run/media/user/Stick"),
                false,
                Some(MountKind::Removable),
                false
            )),
            PlaceSection::Removable
        );
    }

    #[test]
    fn prompt_classifier_matches_bare_word_prompts_only() {
        assert!(is_prompt("User"));
        assert!(is_prompt("Password"));
        assert!(is_prompt("Password for bob@host"));
        assert!(is_prompt("Domain [WORKGROUP]"));
        assert!(is_prompt("passphrase"));
        // gvfs host-key questions ask for a numbered choice
        assert!(is_prompt("Choice"));
        // prose ending in a colon is context, not a prompt
        assert!(!is_prompt("Enter user and password for [localhost]"));
        assert!(!is_prompt("Authentication Required"));
        assert!(!is_prompt("Error mounting gvfs backend"));
    }

    #[test]
    fn password_prompts_mask_their_answer() {
        assert!(mask_prompt("Password"));
        assert!(mask_prompt("passphrase"));
        assert!(!mask_prompt("User"));
        assert!(!mask_prompt("Domain [WORKGROUP]"));
    }

    /// The dialog wiring, without a subprocess: a prompt arriving from
    /// the pump shows in the view, the typed answer routes to the pump
    /// channel on Enter, and Esc closes the dialog.
    #[test]
    fn connect_dialog_prompt_submit_routes_to_the_pump() {
        fn typed(k: &str) -> KeyDownEvent {
            let mut ev = key(k);
            ev.keystroke.key_char = Some(k.to_string());
            ev
        }
        let lab = Lab::new("connect-dialog");
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        let (ans_tx, ans_rx) = mpsc::channel::<String>();
        window.update(|browser, _, cx| {
            browser.open_connect_dialog(cx);
            let dialog = browser.connect.as_mut().unwrap();
            dialog.session = Some(ConnectSession {
                answers: ans_tx,
                // no child: Cancel's kill is a no-op on a taken cell
                child: Arc::new(Mutex::new(None)),
            });
            browser
                .connect_event(ConnectEvent::Prompt {
                    text: "User".into(),
                    mask: false,
                }, cx);
        });
        app.run_until_parked();
        window.update(|browser, _, cx| {
            for ch in ["b", "o", "b"] {
                browser.route_key(&typed(ch), cx);
            }
            browser.route_key(&key("enter"), cx);
        });
        app.run_until_parked();
        assert_eq!(ans_rx.recv_timeout(std::time::Duration::from_secs(5)).as_deref(), Ok("bob"));
        window.update(|browser, _, cx| {
            let dialog = browser.connect.as_ref().unwrap();
            assert!(dialog.prompt.is_none(), "prompt consumed");
            assert!(
                dialog.input.text().is_empty(),
                "answer buffer cleared"
            );
            // Esc at this point (no prompt up) still tears the dialog down
            browser.route_key(&key("escape"), cx);
        });
        app.run_until_parked();
        window.update(|browser, _, _| assert!(browser.connect.is_none(), "Esc closes"));
    }

    /// A password typed into the form answers gio's first password
    /// prompt by itself (user prompts and a wrong-password retry
    /// still surface for typing).
    #[test]
    fn connect_dialog_password_field_answers_the_first_prompt() {
        let lab = Lab::new("connect-dialog-password");
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(icons::Assets),
        );
        let mut window = open_browser(&mut app, &lab.dir);
        let (ans_tx, ans_rx) = mpsc::channel::<String>();
        window.update(|browser, _, cx| {
            browser.open_connect_dialog(cx);
            let dialog = browser.connect.as_mut().unwrap();
            dialog.fields[FIELD_PASSWORD] = input::Field::new("secret");
            dialog.session = Some(ConnectSession {
                answers: ans_tx,
                child: Arc::new(Mutex::new(None)),
            });
            // a user prompt is not a password prompt: it surfaces
            browser.connect_event(ConnectEvent::Prompt {
                text: "User".into(),
                mask: false,
            }, cx);
            let dialog = browser.connect.as_ref().unwrap();
            assert_eq!(dialog.prompt.as_deref(), Some("User"), "user prompt surfaces");
            // the password prompt: answered from the form, no typing
            browser.connect_event(ConnectEvent::Prompt {
                text: "Password".into(),
                mask: true,
            }, cx);
            let dialog = browser.connect.as_ref().unwrap();
            assert!(
                dialog.prompt.is_none(),
                "password prompt auto-answered, never surfaced"
            );
            assert!(dialog.password_tried);
        });
        app.run_until_parked();
        assert_eq!(
            ans_rx.recv_timeout(std::time::Duration::from_secs(5)).as_deref(),
            Ok("secret")
        );
        // the second password prompt (the stored one was wrong)
        // surfaces for typing like any other
        window.update(|browser, _, cx| {
            browser.connect_event(ConnectEvent::Prompt {
                text: "Password".into(),
                mask: true,
            }, cx);
            let dialog = browser.connect.as_ref().unwrap();
            assert_eq!(
                dialog.prompt.as_deref(),
                Some("Password"),
                "retry prompt surfaces"
            );
        });
    }

    /// The full pump loop against a scripted child that mimics gio's
    /// prompt protocol (context lines with newlines, prompts ending
    /// bare at the colon).
    #[test]
    fn mount_process_relays_prompts_and_answers() {
        let dir = std::env::temp_dir().join(format!("kuma-connect-ok-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let script = "echo Authentication Required; \
            echo 'Enter user and password for [host]:'; \
            printf 'User: '; read u; printf 'Password: '; read p; \
            if [ \"$u\" = bob ] && [ \"$p\" = s3cret ]; then \
              mkdir \"$TESTDIR/sftp:host=host,user=bob\"; exit 0; \
            else echo 'Error mounting: auth failed'; exit 1; fi";
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .env("TESTDIR", &dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let child = Arc::new(Mutex::new(Some(child)));
        let (ev_tx, ev_rx) = mpsc::channel::<ConnectEvent>();
        let (ans_tx, ans_rx) = mpsc::channel::<String>();
        let pump_dir = dir.clone();
        let handle = std::thread::spawn(move || {
            run_mount_process(stdin, stdout, stderr, child, pump_dir, ev_tx, ans_rx)
        });

        let mut notes = Vec::new();
        let mut prompts = Vec::new();
        loop {
            let ev = ev_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("pump finished in time");
            match ev {
                ConnectEvent::Prompt { text, mask } => {
                    prompts.push((text, mask));
                    if prompts.len() == 1 {
                        ans_tx.send("bob".into()).unwrap();
                    } else {
                        ans_tx.send("s3cret".into()).unwrap();
                    }
                }
                ConnectEvent::Note(line) => notes.push(line),
                ConnectEvent::Done { ok, message, mount } => {
                    assert!(ok, "scripted mount should succeed: {message}");
                    let expected = dir.join("sftp:host=host,user=bob");
                    assert_eq!(mount.as_deref(), Some(expected.as_path()));
                    assert!(expected.is_dir());
                    break;
                }
            }
        }
        handle.join().unwrap();
        assert_eq!(prompts.len(), 2, "user and password prompts: {prompts:?}");
        assert_eq!(prompts[0], ("User".into(), false));
        assert_eq!(prompts[1], ("Password".into(), true));
        assert!(notes.iter().any(|n| n.contains("Authentication Required")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mount_process_relay_host_key_question_with_choices() {
        let dir = std::env::temp_dir().join(format!("kuma-connect-key-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // gvfs's host-key flow: multi-line context, numbered choices,
        // then a bare "Choice: " prompt that waits on stdin
        let script = "echo 'Identity Verification Failed'; \\\n\
            echo '[1] Log In Anyway'; \\\n\
            echo '[2] Cancel Login'; \\\n\
            printf 'Choice: '; read c; \\\n\
            if [ \"$c\" = 1 ]; then \\\n\
              mkdir \"$TESTDIR/sftp:host=host\"; exit 0; \\\n\
            fi; echo 'Error mounting: cancelled'; exit 1";
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .env("TESTDIR", &dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let child = Arc::new(Mutex::new(Some(child)));
        let (ev_tx, ev_rx) = mpsc::channel::<ConnectEvent>();
        let (ans_tx, ans_rx) = mpsc::channel::<String>();
        let pump_dir = dir.clone();
        let handle = std::thread::spawn(move || {
            run_mount_process(stdin, stdout, stderr, child, pump_dir, ev_tx, ans_rx)
        });
        let mut notes = Vec::new();
        loop {
            match ev_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("pump finished in time")
            {
                ConnectEvent::Prompt { text, mask } => {
                    assert_eq!(text, "Choice");
                    assert!(!mask);
                    // the choices must have arrived before the prompt,
                    // or the user is answering a question they cannot see
                    assert!(notes.iter().any(|n: &String| n.contains("[1] Log In Anyway")));
                    assert!(notes.iter().any(|n: &String| n.contains("[2] Cancel Login")));
                    ans_tx.send("1".into()).unwrap();
                }
                ConnectEvent::Note(line) => notes.push(line),
                ConnectEvent::Done { ok, mount, .. } => {
                    assert!(ok);
                    assert_eq!(
                        mount.as_deref(),
                        Some(dir.join("sftp:host=host").as_path())
                    );
                    break;
                }
            }
        }
        handle.join().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mount_process_reports_auth_failure() {
        let dir = std::env::temp_dir().join(format!("kuma-connect-bad-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let script = "printf 'User: '; read u; printf 'Password: '; read p; \
            echo 'Error mounting: auth failed'; exit 1";
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let child = Arc::new(Mutex::new(Some(child)));
        let (ev_tx, ev_rx) = mpsc::channel::<ConnectEvent>();
        let (ans_tx, ans_rx) = mpsc::channel::<String>();
        let pump_dir = dir.clone();
        let handle = std::thread::spawn(move || {
            run_mount_process(stdin, stdout, stderr, child, pump_dir, ev_tx, ans_rx)
        });
        let mut answered = 0;
        loop {
            match ev_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("pump finished in time")
            {
                ConnectEvent::Prompt { .. } => {
                    answered += 1;
                    ans_tx.send(format!("wrong{answered}")).unwrap();
                }
                ConnectEvent::Note(_) => {}
                ConnectEvent::Done { ok, message, mount } => {
                    assert!(!ok);
                    assert_eq!(mount, None);
                    assert_eq!(message, "connection failed");
                    break;
                }
            }
        }
        handle.join().unwrap();
        assert_eq!(answered, 2);
        let _ = fs::remove_dir_all(&dir);
    }
}
