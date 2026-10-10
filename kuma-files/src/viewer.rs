//! The standalone open surface: a full-window viewer for the types
//! kuma-files claims as the system default opener (images and pdf).
//! The peek (spacebar in the grid) and this window share everything
//! underneath: the same decode arms, the same zoom and pan math, the
//! same fail-soft honesty. The peek is transient and grid-coupled;
//! this one is a plain window an xdg-open can land in.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::fs;

use gpui::{
    div, img, prelude::*, px, size, App, AppContext, Bounds, FocusHandle, Focusable, ImageSource,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point,
    Render, RenderImage, ScrollWheelEvent, SharedString, TitlebarOptions, Window, WindowBounds,
    WindowOptions,
};

use crate::browser::{
    parse_pdf_pages, ql_clamp_pan, ql_fit_scale, write_recent, Browser, QL_DECODE_MAX, QL_ZOOM_MAX,
};
use crate::{icons, theme};

/// Every open window root in the process: the manager (created
/// eagerly on a dir launch, lazily on a dir activation) and the one
/// viewer. The activation pump and the manager's Enter both reach
/// the viewer through this global.
#[derive(Default)]
pub(crate) struct Hosts {
    pub(crate) browser: Option<gpui::WindowHandle<Browser>>,
    pub(crate) viewer: Option<gpui::WindowHandle<Viewer>>,
}

impl gpui::Global for Hosts {}

/// The manager window, created lazily: a viewer-only process (opened
/// on a file) grows one when a dir activation or a bare poke asks
/// for the manager.
pub(crate) fn ensure_browser(cx: &mut App) -> Option<gpui::WindowHandle<Browser>> {
    if let Some(handle) = cx.try_global::<Hosts>().and_then(|hosts| hosts.browser.clone())
        && handle.update(cx, |_, _, _| {}).is_ok()
    {
        return Some(handle);
    }
    let bounds = Bounds::centered(None, size(px(960.), px(640.)), cx);
    match cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            app_id: Some("kuma-files".into()),
            titlebar: Some(TitlebarOptions {
                title: Some(SharedString::from("Koguma")),
                ..Default::default()
            }),
            ..Default::default()
        },
        |window, cx| cx.new(|cx| Browser::new(None, window, cx)),
    ) {
        Ok(handle) => {
            let viewer = cx.try_global::<Hosts>().and_then(|hosts| hosts.viewer.clone());
            cx.set_global(Hosts {
                browser: Some(handle.clone()),
                viewer,
            });
            Some(handle)
        }
        Err(err) => {
            log::error!("manager window: {err:#}");
            None
        }
    }
}

/// Open (or refocus) the viewer on this file: the one-window rule,
/// so a stream of opens replaces the content instead of stacking
/// windows.
pub(crate) fn open_or_focus(path: PathBuf, cx: &mut App) {
    if let Some(handle) = cx.try_global::<Hosts>().and_then(|hosts| hosts.viewer.clone())
        && handle
            .update(cx, |viewer, window, cx| {
                viewer.show(path.clone(), window, cx);
                window.activate_window();
            })
            .is_ok()
    {
        return;
    }
    let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
    match cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            app_id: Some("kuma-files".into()),
            titlebar: Some(TitlebarOptions {
                title: Some(SharedString::from(file_title(&path))),
                ..Default::default()
            }),
            ..Default::default()
        },
        |window, cx| cx.new(|cx| Viewer::new(path, window, cx)),
    ) {
        Ok(handle) => {
            let browser = cx.try_global::<Hosts>().and_then(|hosts| hosts.browser.clone());
            cx.set_global(Hosts {
                browser,
                viewer: Some(handle),
            });
            cx.activate(true);
        }
        Err(err) => log::error!("viewer window: {err:#}"),
    }
}

fn file_title(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "Viewer".to_string())
}

/// Whether the open surface renders this file at all. The mimetype
/// claims (the desktop file) are the honest subset: avif and svg
/// have no decoder here and stay unclaimed, avif inside is_image's
/// own list included, so the grid's Enter keeps handing those to
/// whatever the system already opens them with.
pub(crate) fn is_openable(name: &str) -> bool {
    let ext = Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase());
    icons::is_pdf(name) || (icons::is_image(name) && ext.as_deref() != Some("avif"))
}

/// The viewer's decode budget: the peek's own cap, shared.
const VIEWER_DECODE_MAX: u32 = QL_DECODE_MAX;

/// The standalone viewer: one file, its same-kind siblings, and the
/// peek's rendering mechanics without the grid.
pub(crate) struct Viewer {
    dir: PathBuf,
    /// Same-kind files in the containing directory, name-sorted,
    /// hidden files skipped: the range the arrows walk.
    siblings: Vec<PathBuf>,
    index: usize,
    pub(crate) path: PathBuf,
    /// The landed tile and the (path, page) it belongs to.
    render: Option<(PathBuf, usize, Arc<RenderImage>)>,
    inflight: Option<(PathBuf, usize)>,
    failed: bool,
    zoom: f32,
    pan: (f32, f32),
    drag_from: Option<(Point<Pixels>, (f32, f32))>,
    /// Quarter turns for images, applied at decode time (gpui has
    /// no image rotation).
    rotation: u8,
    /// PDF page, 1-based; images hold 0.
    page: usize,
    pages: Option<usize>,
    counting: bool,
    focus: FocusHandle,
}

impl Focusable for Viewer {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Viewer {
    pub(crate) fn new(path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let mut viewer = Self {
            dir: PathBuf::new(),
            siblings: Vec::new(),
            index: 0,
            path: PathBuf::new(),
            render: None,
            inflight: None,
            failed: false,
            zoom: 1.0,
            pan: (0.0, 0.0),
            drag_from: None,
            rotation: 0,
            page: 1,
            pages: None,
            counting: false,
            focus,
        };
        viewer.load(path, window, cx);
        viewer
    }

    /// Point the viewer at a file: the nav range, the fresh state,
    /// the title, the recency note, and the decode kick. Everything
    /// a launch, an in-grid Enter, or an arrow step needs.
    pub(crate) fn show(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.load(path, window, cx);
    }

    fn load(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        // the listing's kind filter reads the file name, so the new
        // path lands before the reload
        self.path = path;
        if dir != self.dir {
            self.dir = dir;
            self.reload_siblings();
        }
        self.index = self.siblings.iter().position(|p| *p == self.path).unwrap_or(0);
        self.render = None;
        self.inflight = None;
        self.failed = false;
        self.zoom = 1.0;
        self.pan = (0.0, 0.0);
        self.rotation = 0;
        self.page = 1;
        self.pages = None;
        self.counting = false;
        window.set_window_title(&file_title(&self.path));
        write_recent(&self.path);
        self.kick(cx);
        if icons::is_pdf(
            &self
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
        ) {
            self.request_pages(cx);
        }
        cx.notify();
    }

    /// The same-kind listing: files in the viewer's directory whose
    /// kind matches the open file's, name-sorted, hidden files
    /// skipped (the grid's default).
    fn reload_siblings(&mut self) {
        let name = file_title(&self.path);
        self.siblings = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false))
            .map(|entry| entry.path())
            .filter(|path| {
                let sibling = file_title(path);
                !sibling.starts_with('.') && same_kind(&name, &sibling)
            })
            .collect();
        self.siblings.sort_by(|a, b| {
            file_title(a)
                .to_lowercase()
                .cmp(&file_title(b).to_lowercase())
                .then_with(|| a.cmp(b))
        });
    }

    /// Kick the decode for the current file (and page, for a pdf).
    /// One ask per (path, page); a held tile for the same path
    /// stays up while the next page decodes.
    fn kick(&mut self, cx: &mut Context<Self>) {
        let name = file_title(&self.path);
        let is_pdf = icons::is_pdf(&name);
        let is_image = icons::is_image(&name);
        let page = if is_pdf { self.page } else { 0 };
        if !is_pdf && !is_image {
            // an unclaimed type landed here via a direct CLI call:
            // the failed card is the honest surface
            self.failed = true;
            cx.notify();
            return;
        }
        if self
            .render
            .as_ref()
            .is_some_and(|(p, pg, _)| *p == self.path && *pg == page)
            || self.inflight.as_ref() == Some(&(self.path.clone(), page))
        {
            return;
        }
        let Ok(meta) = fs::symlink_metadata(&self.path) else {
            self.failed = true;
            cx.notify();
            return;
        };
        // the peek's cap: decoding cost is in the file read, not
        // the resize
        if meta.len() > 32 * 1024 * 1024 {
            self.failed = true;
            cx.notify();
            return;
        }
        self.inflight = Some((self.path.clone(), page));
        self.failed = false;
        let path = self.path.clone();
        let rotation = self.rotation;
        cx.spawn(async move |this, cx| {
            let bg_path = path.clone();
            let render = cx
                .background_spawn(async move {
                    std::panic::catch_unwind(move || {
                        if is_pdf {
                            icons::decode_pdf_thumbnail(&bg_path, page, VIEWER_DECODE_MAX)
                        } else {
                            icons::decode_thumbnail_dynamic(&bg_path)
                                .map(|image| match rotation {
                                    1 => image.rotate90(),
                                    2 => image.rotate180(),
                                    3 => image.rotate270(),
                                    _ => image,
                                })
                                .map(|image| {
                                    icons::decode_to_render(
                                        image,
                                        VIEWER_DECODE_MAX,
                                        VIEWER_DECODE_MAX,
                                    )
                                })
                        }
                    })
                    .unwrap_or(None)
                })
                .await;
            let update = this.update(cx, |this, cx| {
                if this.inflight.as_ref() == Some(&(path.clone(), page)) {
                    this.inflight = None;
                }
                match render {
                    Some(render) => {
                        this.render = Some((path.clone(), page, Arc::new(render)));
                    }
                    None => match &this.render {
                        // paging: the held tile stays up, the page
                        // reverts to the one it shows
                        Some((p, pg, _)) if *p == path && is_pdf => this.page = *pg,
                        // nothing held (the opening decode failed):
                        // the card stands in
                        _ => this.failed = true,
                    },
                }
                cx.notify();
            });
            if let Err(err) = update {
                log::error!("viewer decode: {err:#}");
            }
        })
        .detach();
    }

    /// Ask pdfinfo how many pages the open pdf has; a missing tool
    /// or a failed call leaves paging dormant, like the peek's.
    fn request_pages(&mut self, cx: &mut Context<Self>) {
        if self.counting {
            return;
        }
        self.counting = true;
        let path = self.path.clone();
        cx.spawn(async move |this, cx| {
            let bg_path = path.clone();
            let pages = cx
                .background_spawn(async move {
                    let out = Command::new("pdfinfo").arg(&bg_path).output().ok()?;
                    parse_pdf_pages(&String::from_utf8_lossy(&out.stdout))
                })
                .await;
            let update = this.update(cx, |this, cx| {
                this.counting = false;
                if this.path == path {
                    this.pages = pages;
                    cx.notify();
                }
            });
            if let Err(err) = update {
                log::error!("viewer page count: {err:#}");
            }
        })
        .detach();
    }

    /// Arrows walk the same-kind range; a shift multiplies the step
    /// for images and turns into a page turn for pdfs.
    fn flip(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.siblings.is_empty() {
            return;
        }
        let next = (self.index as isize + step)
            .clamp(0, self.siblings.len() as isize - 1) as usize;
        if next == self.index {
            return;
        }
        let path = self.siblings[next].clone();
        self.load(path, window, cx);
    }

    fn zoom_step(&mut self, step: f32, cx: &mut Context<Self>) {
        self.zoom = (self.zoom * step).clamp(1.0, QL_ZOOM_MAX);
        if self.zoom <= 1.0 {
            self.pan = (0.0, 0.0);
        }
        cx.notify();
    }

    /// R: a quarter turn for images, re-decoded with the rotation
    /// baked in (gpui has no image rotation).
    fn rotate(&mut self, cx: &mut Context<Self>) {
        if !icons::is_image(&file_title(&self.path)) {
            return;
        }
        self.rotation = (self.rotation + 1) % 4;
        self.render = None;
        self.kick(cx);
    }

    fn page_turn(&mut self, target: usize, cx: &mut Context<Self>) {
        let Some(pages) = self.pages else {
            return;
        };
        let next = target.clamp(1, pages);
        if next == self.page {
            return;
        }
        self.page = next;
        self.pan = (0.0, 0.0);
        self.drag_from = None;
        self.kick(cx);
        cx.notify();
    }

    fn page_step(&mut self, step: isize, cx: &mut Context<Self>) {
        let target = (self.page as isize + step).max(0) as usize;
        self.page_turn(target, cx);
    }

    fn route_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let shift = keystroke.modifiers.shift;
        match keystroke.key.as_str() {
            // escape closes the window; ctrl+q too, like the manager
            "escape" => window.remove_window(),
            "q" if keystroke.modifiers.control => window.remove_window(),
            "left" | "up" => {
                if shift && icons::is_pdf(&file_title(&self.path)) {
                    self.page_step(-1, cx);
                } else {
                    let step = if shift { -5 } else { -1 };
                    self.flip(step, window, cx);
                }
            }
            "right" | "down" => {
                if shift && icons::is_pdf(&file_title(&self.path)) {
                    self.page_step(1, cx);
                } else {
                    let step = if shift { 5 } else { 1 };
                    self.flip(step, window, cx);
                }
            }
            "=" | "+" => self.zoom_step(1.2, cx),
            "-" | "_" => self.zoom_step(1.0 / 1.2, cx),
            "0" => {
                self.zoom = 1.0;
                self.pan = (0.0, 0.0);
                cx.notify();
            }
            "r" if !keystroke.modifiers.control && !keystroke.modifiers.alt => self.rotate(cx),
            "pageup" => self.page_step(-1, cx),
            "pagedown" => self.page_step(1, cx),
            "home" => self.page_turn(1, cx),
            "end" => {
                let last = self.pages.unwrap_or(1);
                self.page_turn(last, cx);
            }
            _ => {}
        }
    }
}

/// Whether two file names belong to the same nav kind: pdfs group
/// with pdfs, images with images.
fn same_kind(a: &str, b: &str) -> bool {
    if icons::is_pdf(b) {
        icons::is_pdf(a)
    } else if icons::is_image(b) {
        icons::is_image(a)
    } else {
        false
    }
}

impl Render for Viewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let name = file_title(&self.path);
        let is_pdf = icons::is_pdf(&name);
        // the nav position and the pdf's page label
        let pos_label = (!self.siblings.is_empty())
            .then(|| format!("{} of {}", self.index + 1, self.siblings.len()));
        let page_label = if is_pdf {
            self.pages.map(|pages| format!("page {} / {}", self.page, pages))
        } else {
            None
        };
        let hint = if is_pdf {
            "PgUp/PgDn pages · Esc closes".to_string()
        } else {
            "←/→ files · +/- zoom · R rotate · Esc closes".to_string()
        };
        let failed = self.failed;
        let held = self
            .render
            .as_ref()
            .filter(|(p, pg, _)| *p == self.path && *pg == if is_pdf { self.page } else { 0 })
            .map(|(_, _, render)| render.clone());

        // the top row: name, page label, position in the kind, the
        // close road
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
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme::text())
                    .truncate()
                    .child(name.clone()),
            )
            .children(page_label.map(|label| {
                div()
                    .text_size(px(12.))
                    .text_color(theme::text_dim())
                    .child(label)
            }))
            .children(pos_label.map(|label| {
                div()
                    .text_size(px(12.))
                    .text_color(theme::text_dim())
                    .child(label)
            }))
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::text_dim())
                    .child(hint),
            )
            .child(
                div()
                    .id("viewer-close")
                    .cursor_pointer()
                    .px_2()
                    .rounded_sm()
                    .text_size(px(16.))
                    .text_color(theme::text_dim())
                    .hover(|this| this.text_color(theme::text()).bg(theme::row_hover()))
                    .on_click(|_, window, _| window.remove_window())
                    .child("×"),
            );

        // the pane: the decoded tile with the peek's fit, zoom, and
        // pan mechanics, or the honest card
        let pane = if let Some(render) = held {
            let viewport = window.viewport_size();
            let avail_w = (viewport.width - px(16.)).max(px(1.)).into();
            let avail_h = (viewport.height - px(40.)).max(px(1.)).into();
            let natural = render.size(0);
            let natural = (
                u32::from(natural.width) as f32,
                u32::from(natural.height) as f32,
            );
            let fit = ql_fit_scale(natural, (avail_w, avail_h));
            let zoom = self.zoom.clamp(1.0, QL_ZOOM_MAX);
            let display = (natural.0 * fit * zoom, natural.1 * fit * zoom);
            let (pan_x, pan_y) = ql_clamp_pan(self.pan, display, (avail_w, avail_h));
            div()
                .id("viewer-pane")
                .debug_selector(|| "viewer-pane".into())
                .flex_1()
                .overflow_hidden()
                .flex()
                .items_center()
                .justify_center()
                .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                    // one wheel notch is about 1/1.2, same as the
                    // +/- keys; trackpads land in between smoothly
                    let dy: f32 = event.delta.pixel_delta(px(20.)).y.into();
                    this.zoom = (this.zoom * (-dy * 0.003f32).exp()).clamp(1.0, QL_ZOOM_MAX);
                    if this.zoom <= 1.0 {
                        this.pan = (0.0, 0.0);
                    }
                    cx.notify();
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &MouseDownEvent, _, _| {
                        // any press arms a pan; at fit the clamps
                        // hold the pan at zero
                        this.drag_from = Some((event.position, this.pan));
                    }),
                )
                .on_mouse_move(cx.listener(
                    |this, event: &MouseMoveEvent, _, cx| {
                        let Some((from, from_pan)) = this.drag_from else {
                            return;
                        };
                        if event.pressed_button != Some(MouseButton::Left) {
                            this.drag_from = None;
                            cx.notify();
                            return;
                        }
                        let dx: f32 = (event.position.x - from.x).into();
                        let dy: f32 = (event.position.y - from.y).into();
                        this.pan = (from_pan.0 + dx, from_pan.1 + dy);
                        cx.notify();
                    },
                ))
                .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| {
                    if this.drag_from.take().is_some() {
                        cx.notify();
                    }
                }))
                .child(
                    img(ImageSource::Render(render))
                        .w(px(display.0))
                        .h(px(display.1))
                        .ml(px(pan_x))
                        .mr(px(-pan_x))
                        .mt(px(pan_y))
                        .mb(px(-pan_y)),
                )
        } else {
            // the decode failed or the type has no decoder here:
            // say so, never a blank window
            let ext = Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_uppercase())
                .unwrap_or_else(|| "file".to_string());
            div()
                .id("viewer-card")
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    div()
                        .text_size(px(15.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme::text())
                        .child(format!("{ext} file")),
                )
                .child(
                    div()
                        .max_w(px(360.))
                        .text_size(px(12.))
                        .text_color(theme::text_dim())
                        .child(if failed {
                            "no preview available for this file".to_string()
                        } else {
                            "rendering…".to_string()
                        }),
                )
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(
                |this, event: &KeyDownEvent, window, cx| this.route_key(event, window, cx),
            ))
            .bg(theme::bg())
            .text_color(theme::text())
            .child(header)
            .child(pane)
    }
}

/// A two-page pdf assembled in place, xref offsets computed: pdfinfo
/// counts it, pdftocairo rasters it, no committed fixture needed.
#[cfg(test)]
fn two_page_pdf() -> Vec<u8> {
    let objects = [
        "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".to_string(),
        "2 0 obj\n<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>\nendobj\n".to_string(),
        "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] >>\nendobj\n".to_string(),
        "4 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] >>\nendobj\n".to_string(),
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for object in &objects {
        offsets.push(out.len() as u32);
        out.extend_from_slice(object.as_bytes());
    }
    let xref_at = out.len();
    out.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            xref_at
        )
        .as_bytes(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Keystroke;

    fn open_viewer(app: &mut gpui::TestApp, path: &Path) -> gpui::TestAppWindow<Viewer> {
        let window = app.open_window(|window, cx| Viewer::new(path.to_path_buf(), window, cx));
        app.run_until_parked();
        window
    }

    fn key(k: &str) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke::parse(k).unwrap(),
            is_held: false,
            prefer_character_input: false,
        }
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(width, height)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        png
    }

    struct Lab {
        dir: PathBuf,
    }

    impl Lab {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "koguma-viewer-{}-{}",
                name,
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }
    }

    impl Drop for Lab {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn viewer_renders_an_image_and_sees_its_kind() {
        let lab = Lab::new("image");
        fs::write(lab.dir.join("a.png"), png_bytes(20, 10)).unwrap();
        fs::write(lab.dir.join("b.jpg"), png_bytes(8, 8)).unwrap();
        fs::write(lab.dir.join("note.txt"), b"not an image").unwrap();
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(crate::icons::Assets),
        );
        let mut window = open_viewer(&mut app, &lab.dir.join("a.png"));
        window.update(|viewer, _, _| {
            // plain name sort, hidden files and other kinds gone
            assert_eq!(viewer.siblings, vec![
                lab.dir.join("a.png"),
                lab.dir.join("b.jpg")
            ]);
            assert_eq!(viewer.index, 0);
            assert!(!viewer.failed);
        });
        app.run_until_parked();
        window.update(|viewer, _, _| {
            assert!(matches!(
                viewer.render.as_ref(),
                Some((path, 0, _)) if *path == lab.dir.join("a.png")
            ));
        });
    }

    #[test]
    fn viewer_arrows_stay_within_the_kind() {
        let lab = Lab::new("nav");
        fs::write(lab.dir.join("a.png"), png_bytes(8, 8)).unwrap();
        fs::write(lab.dir.join("b.jpg"), png_bytes(8, 8)).unwrap();
        fs::write(lab.dir.join("note.txt"), b"text between images").unwrap();
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(crate::icons::Assets),
        );
        let mut window = open_viewer(&mut app, &lab.dir.join("a.png"));
        window.update(|viewer, window, cx| {
            viewer.route_key(&key("right"), window, cx);
            // the text file between them never enters the walk
            assert_eq!(viewer.path, lab.dir.join("b.jpg"));
            viewer.route_key(&key("right"), window, cx);
            // the last sibling clamps
            assert_eq!(viewer.path, lab.dir.join("b.jpg"));
            viewer.route_key(&key("left"), window, cx);
            assert_eq!(viewer.path, lab.dir.join("a.png"));
        });
    }

    #[test]
    fn viewer_rotates_an_image() {
        let mut before = (0u32, 0u32);
        let lab = Lab::new("rotate");
        fs::write(lab.dir.join("photo.png"), png_bytes(20, 10)).unwrap();
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(crate::icons::Assets),
        );
        let mut window = open_viewer(&mut app, &lab.dir.join("photo.png"));
        app.run_until_parked();
        window.update(|viewer, _, _| {
            let (_, _, render) = viewer.render.as_ref().unwrap();
            let size = render.size(0);
            before = (u32::from(size.width), u32::from(size.height));
            // the 2:1 source is never square, so the swap is
            // unambiguous
            assert_ne!(before.0, before.1);
        });
        window.update(|viewer, window, cx| viewer.route_key(&key("r"), window, cx));
        app.run_until_parked();
        window.update(|viewer, _, _| {
            assert_eq!(viewer.rotation, 1);
            let (_, _, render) = viewer.render.as_ref().unwrap();
            let size = render.size(0);
            // the quarter turn is baked into the raster
            assert_eq!(
                (u32::from(size.width), u32::from(size.height)),
                (before.1, before.0)
            );
        });
    }

    #[test]
    fn viewer_pages_a_pdf() {
        let lab = Lab::new("pdf");
        fs::write(lab.dir.join("doc.pdf"), two_page_pdf()).unwrap();
        fs::write(lab.dir.join("doc2.pdf"), two_page_pdf()).unwrap();
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(crate::icons::Assets),
        );
        let mut window = open_viewer(&mut app, &lab.dir.join("doc.pdf"));
        app.run_until_parked();
        window.update(|viewer, _, _| {
            // pdfinfo counted the pages; the first renders
            assert_eq!(viewer.pages, Some(2));
            assert!(viewer
                .render
                .as_ref()
                .is_some_and(|(p, pg, _)| *p == lab.dir.join("doc.pdf") && *pg == 1));
        });
        window.update(|viewer, window, cx| {
            viewer.route_key(&key("pagedown"), window, cx);
            assert_eq!(viewer.page, 2);
        });
        app.run_until_parked();
        window.update(|viewer, window, cx| {
            assert!(viewer
                .render
                .as_ref()
                .is_some_and(|(p, pg, _)| *p == lab.dir.join("doc.pdf") && *pg == 2));
            // same-kind nav: the other pdf is one flip away
            viewer.route_key(&key("right"), window, cx);
            assert_eq!(viewer.path, lab.dir.join("doc2.pdf"));
        });
    }

    #[test]
    fn viewer_fails_soft_on_an_unsupported_type() {
        let lab = Lab::new("unsupported");
        fs::write(lab.dir.join("notes.txt"), b"plain text").unwrap();
        let mut app = gpui::TestApp::with_text_system_and_assets(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("system-ui")),
            Arc::new(crate::icons::Assets),
        );
        let mut window = open_viewer(&mut app, &lab.dir.join("notes.txt"));
        window.update(|viewer, window, cx| {
            assert!(viewer.failed);
            // no same-kind siblings: the arrows clamp instead of
            // panicking on an empty walk
            viewer.route_key(&key("right"), window, cx);
            assert_eq!(viewer.path, lab.dir.join("notes.txt"));
        });
    }
}
