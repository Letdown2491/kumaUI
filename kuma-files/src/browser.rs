use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::fs;
use gpui::{
    App, ClickEvent, Context, Div, ExternalDragPayload, ExternalPaths, FileDragPaths, FocusHandle,
    Focusable, KeyDownEvent, Pixels, Point, Render, Stateful, Window, div, prelude::*, px, rgb,
};

use crate::theme;

/// One drag origin. The external payload resolver needs the path plus
/// whether it is a directory, so the platform drag advertises both.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct DragEntry {
    pub(crate) path: PathBuf,
    pub(crate) is_dir: bool,
}

#[derive(Clone)]
struct Entry {
    path: PathBuf,
    name: String,
    is_dir: bool,
    size: Option<u64>,
}

struct Place {
    name: String,
    path: PathBuf,
}

pub(crate) struct Browser {
    dir: PathBuf,
    entries: Vec<Entry>,
    selection: HashSet<PathBuf>,
    cursor: Option<usize>,
    history: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    renaming: Option<PathBuf>,
    rename_buffer: String,
    status: String,
    focus: FocusHandle,
    places: Vec<Place>,
}

impl Focusable for Browser {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Browser {
    pub(crate) fn new(dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let mut browser = Self {
            dir,
            entries: Vec::new(),
            selection: HashSet::new(),
            cursor: None,
            history: Vec::new(),
            forward: Vec::new(),
            renaming: None,
            rename_buffer: String::new(),
            status: String::new(),
            focus,
            places: Self::places(),
        };
        browser.reload();
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

    fn reload(&mut self) {
        match fs::read_dir(&self.dir) {
            Ok(read) => {
                let mut entries = Vec::new();
                for entry in read.flatten() {
                    let path = entry.path();
                    let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
                    let size = fs::symlink_metadata(&path).ok().and_then(|meta| {
                        if meta.is_dir() {
                            None
                        } else {
                            Some(meta.len())
                        }
                    });
                    let name = entry.file_name().to_string_lossy().into_owned();
                    entries.push(Entry {
                        path,
                        name,
                        is_dir,
                        size,
                    });
                }
                entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
                self.entries = entries;
            }
            Err(err) => {
                log::error!("read_dir {}: {err}", self.dir.display());
                self.status = format!("cannot read {}: {err}", self.dir.display());
                self.entries.clear();
            }
        }
    }

    fn load(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if dir == self.dir {
            return;
        }
        self.history.push(self.dir.clone());
        self.forward.clear();
        self.dir = dir;
        self.selection.clear();
        self.cursor = None;
        self.renaming = None;
        self.reload();
        self.status.clear();
        cx.notify();
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        let Some(previous) = self.history.pop() else {
            return;
        };
        self.forward.push(self.dir.clone());
        self.dir = previous;
        self.selection.clear();
        self.cursor = None;
        self.renaming = None;
        self.reload();
        cx.notify();
    }

    fn go_forward(&mut self, cx: &mut Context<Self>) {
        let Some(next) = self.forward.pop() else {
            return;
        };
        self.history.push(self.dir.clone());
        self.dir = next;
        self.selection.clear();
        self.cursor = None;
        self.renaming = None;
        self.reload();
        cx.notify();
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = self.dir.parent().map(Path::to_path_buf) {
            self.load(parent, cx);
        }
    }

    fn open(&mut self, entry: &Entry, cx: &mut Context<Self>) {
        if entry.is_dir {
            self.load(entry.path.clone(), cx);
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

    fn open_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(ix) = self.cursor
            && let Some(entry) = self.entries.get(ix).cloned()
        {
            self.open(&entry, cx);
        }
    }

    fn move_cursor(&mut self, step: isize, cx: &mut Context<Self>) {
        if self.entries.is_empty() {
            return;
        }
        let count = self.entries.len() as isize;
        let current = self.cursor.map_or(-1, |ix| ix as isize);
        let next = (current + step).clamp(0, count - 1);
        self.cursor = Some(next as usize);
        let path = self.entries[next as usize].path.clone();
        self.selection.clear();
        self.selection.insert(path);
        cx.notify();
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.selection = self.entries.iter().map(|entry| entry.path.clone()).collect();
        cx.notify();
    }

    fn start_rename(&mut self, cx: &mut Context<Self>) {
        let Some(ix) = self.cursor else {
            return;
        };
        let Some(entry) = self.entries.get(ix) else {
            return;
        };
        self.renaming = Some(entry.path.clone());
        self.rename_buffer = entry.name.clone();
        cx.notify();
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(renaming) = self.renaming.take() else {
            return;
        };
        let target_name = self.rename_buffer.trim().to_owned();
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
                self.selection.retain(|path| *path != renaming);
                if let Some(ix) = self.cursor
                    && let Some(entry) = self.entries.get_mut(ix)
                    && entry.path == renaming
                {
                    entry.path = dest;
                    entry.name = target_name;
                }
            }
            Err(err) => {
                log::error!("rename {} -> {}: {err}", renaming.display(), dest.display());
                self.status = format!("rename failed: {err}");
            }
        }
        self.reload();
        cx.notify();
    }

    fn trash_selection(&mut self, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            return;
        }
        let mut trashed = 0usize;
        let mut last_error = None;
        for path in self.selection.drain() {
            match trash::delete(&path) {
                Ok(()) => trashed += 1,
                Err(err) => {
                    log::error!("trash {}: {err}", path.display());
                    last_error = Some(err.to_string());
                }
            }
        }
        self.cursor = None;
        self.status = match last_error {
            Some(err) => format!("trashed {trashed}, last error: {err}"),
            None => format!("trashed {trashed}"),
        };
        self.reload();
        cx.notify();
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
        match fs::rename(&dragged.path, &dest) {
            Ok(()) => {
                log::info!("move: {} into {}", dragged.path.display(), target.name);
                self.status = format!("moved {} into {}", name, target.name);
            }
            Err(err) => {
                log::error!("move {} -> {}: {err}", dragged.path.display(), dest.display());
                self.status = format!("move failed: {err}");
            }
        }
        self.reload();
        cx.notify();
    }

    fn handle_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if self.renaming.is_some() {
            match keystroke.key.as_str() {
                "enter" => self.commit_rename(cx),
                "escape" => {
                    self.renaming = None;
                    cx.notify();
                }
                "backspace" => {
                    self.rename_buffer.pop();
                    cx.notify();
                }
                _ if !keystroke.modifiers.control
                    && !keystroke.modifiers.alt
                    && !keystroke.modifiers.platform
                    && !keystroke.modifiers.function =>
                {
                    if let Some(character) = keystroke.key_char.as_deref() {
                        self.rename_buffer.push_str(character);
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
                self.selection.clear();
                self.cursor = None;
                cx.notify();
            }
            "delete" => self.trash_selection(cx),
            "f2" => self.start_rename(cx),
            "left" if keystroke.modifiers.alt => self.go_back(cx),
            "right" if keystroke.modifiers.alt => self.go_forward(cx),
            "down" => self.move_cursor(1, cx),
            "up" => self.move_cursor(-1, cx),
            "a" if keystroke.modifiers.control => self.select_all(cx),
            _ => {}
        }
    }

    fn row(&self, ix: usize, entry: &Entry, cx: &mut Context<Self>) -> Stateful<Div> {
        let dragged = DragEntry {
            path: entry.path.clone(),
            is_dir: entry.is_dir,
        };
        let selected = self.selection.contains(&entry.path);
        let renaming = self.renaming.as_ref().is_some_and(|path| *path == entry.path);
        let entry_path = entry.path.clone();
        let size_text = entry
            .size
            .map(human_size)
            .unwrap_or_else(|| String::from("folder"));

        let base = div()
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
                if event.click_count() >= 2 {
                    if let Some(found) = this
                        .entries
                        .iter()
                        .find(|e| e.path == entry_path)
                        .cloned()
                    {
                        this.open(&found, cx);
                    }
                    return;
                }
                if event.modifiers().control {
                    if this.selection.contains(&entry_path) {
                        this.selection.remove(&entry_path);
                    } else {
                        this.selection.insert(entry_path.clone());
                    }
                } else {
                    this.selection.clear();
                    this.selection.insert(entry_path.clone());
                }
                this.cursor = Some(ix);
                cx.notify();
            }))
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
            div()
                .flex_1()
                .border_1()
                .border_color(theme::accent())
                .rounded_sm()
                .px_1()
                .child(format!("{}▏", self.rename_buffer))
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
                .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                    let list = paths
                        .paths()
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    log::info!("external drop onto folder {target_name}: {list}");
                    this.status = format!("external drop onto {target_name} (not imported yet)");
                    cx.notify();
                }))
        } else {
            base
        };

        base.child(name_child)
            .child(div().text_size(px(12.)).text_color(theme::text_dim()).child(size_text))
    }

    fn place_row(&self, ix: usize, place: &Place, cx: &mut Context<Self>) -> Stateful<Div> {
        let here = place.path == self.dir;
        let path = place.path.clone();
        div()
            .id(format!("place-{ix}"))
            .px_3()
            .py_1()
            .rounded_sm()
            .text_size(px(13.))
            .cursor_pointer()
            .text_color(if here { theme::accent() } else { theme::text_dim() })
            .bg(if here { theme::row_selected() } else { theme::clear() })
            .hover(|this| this.bg(theme::row_hover()))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.load(path.clone(), cx);
            }))
            .child(place.name.clone())
    }
}

impl Render for Browser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut rows: Vec<Stateful<Div>> = Vec::new();
        for (ix, entry) in self.entries.iter().take(200).enumerate() {
            rows.push(self.row(ix, entry, cx));
        }

        let mut places: Vec<Stateful<Div>> = Vec::new();
        for (ix, place) in self.places.iter().enumerate() {
            places.push(self.place_row(ix, place, cx));
        }

        let status_color = if self.status.contains("failed") || self.status.contains("cannot") {
            theme::error()
        } else {
            theme::text_dim()
        };

        div()
            .size_full()
            .flex()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                this.handle_key(event, cx);
            }))
            .bg(theme::bg())
            .text_color(theme::text())
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
                    .children(places),
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
                                    .child("◀"),
                            )
                            .child(
                                div()
                                    .id("forward")
                                    .cursor_pointer()
                                    .hover(|this| this.text_color(theme::text()))
                                    .on_click(cx.listener(|this, _, _, cx| this.go_forward(cx)))
                                    .child("▶"),
                            )
                            .child(
                                div()
                                    .id("up")
                                    .cursor_pointer()
                                    .hover(|this| this.text_color(theme::text()))
                                    .on_click(cx.listener(|this, _, _, cx| this.go_up(cx)))
                                    .child("↑"),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .truncate()
                                    .text_color(theme::text())
                                    .child(self.dir.display().to_string()),
                            ),
                    )
                    .child(
                        div()
                            .id("list")
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .gap_px()
                            .p_2()
                            .overflow_y_scroll()
                            .children(rows),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .px_3()
                            .py_1()
                            .border_t_1()
                            .border_color(theme::border())
                            .text_size(px(12.))
                            .text_color(status_color)
                            .child(format!(
                                "{} items, {} selected",
                                self.entries.len(),
                                self.selection.len()
                            ))
                            .child(
                                div()
                                    .truncate()
                                    .child(self.status.clone()),
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
