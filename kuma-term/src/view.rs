//! The gpui view: renders engine snapshots as one painted grid, feeds
//! keystrokes and scroll events back into the engine.
//!
//! The grid is a single custom element: run backgrounds and vector glyphs
//! paint as quads, text rows shape and paint directly. No per-row or
//! per-cell layout nodes, so a full-screen TUI costs the same handful of
//! quads a frame regardless of how many cells it touches.

use std::hash::Hasher;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use gpui::{
    App, Bounds, ClipboardItem, Context, FocusHandle, Focusable, FontStyle, FontWeight,
    Keystroke, MouseButton, ParentElement, Pixels, Render, ScrollDelta, ScrollWheelEvent,
    SharedString, Task, TextAlign, TextRun, UnderlineStyle, Window, WindowTextSystem, canvas, div,
    fill, point, px, prelude::*, size,
};
use log::warn;
use rustc_hash::FxHashMap;

use crate::encoder;
use crate::font;
use crate::glyphs;
use crate::palette::Rgb8;
use crate::term::{CellEdge, Engine, GridPoint, Row, SelectMode, SelectSpan, UiEvent};
use crate::theme::Theme;

const PADDING: f32 = 8.0;

/// Bound on the shaped-row cache: entries are small and a full clear on
/// overflow just costs one rebuild burst after very long scrolls.
const ROW_SHAPE_CACHE_CAP: usize = 4096;

/// Domain tag mixed into the line-layout cache keys, so a row hash can
/// never collide with another text-system user's hash (we all feed the
/// same text-system cache).
const SHAPE_DOMAIN_ROW: u64 = 0x524f5730;

fn hsla_of(c: Rgb8) -> gpui::Hsla {
    let hex = ((c.0 as u32) << 16) | ((c.1 as u32) << 8) | c.2 as u32;
    gpui::rgb(hex).into()
}

pub struct TerminalView {
    engine: Engine,
    theme: Theme,
    focus: FocusHandle,
    font: gpui::Font,
    cell_w: f32,
    cell_h: f32,
    cols: u16,
    lines: u16,
    title: Option<String>,
    /// a left-drag is actively stretching the selection
    selecting: bool,
    /// shaped rows keyed on the row's cell-content hash: a row the grid
    /// did not change skips the cell walk, the string build, and (through
    /// gpui's hash-keyed layout cache) the shaper itself
    row_shapes: FxHashMap<u64, Arc<RowRender>>,
    // the pump task aborts if dropped, so it stays owned by the view
    _pump: Task<()>,
    _palette_tick: Task<()>,
}

impl TerminalView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let theme = Theme::load();
        // the window surface must opt into alpha before anything paints,
        // and subpixel AA is wrong over a translucent background
        if theme.background_opacity < 1.0 || theme.background_opacity_unfocused < 1.0 {
            window.set_background_appearance(gpui::WindowBackgroundAppearance::Transparent);
        }
        let family = font::pick_family(theme.font_family.as_deref(), window.text_system());
        let font = gpui::Font {
            family: family.into(),
            fallbacks: Some(theme.font_fallbacks()),
            ..Default::default()
        };
        let font_size = px(theme.font_size_px());
        let (cell_w, cell_h) = cell_metrics(&font, font_size, window.text_system());

        let focus = cx.focus_handle();
        window.focus(&focus, cx);

        // 80x24 until the first render fits the grid to the real viewport
        let window_size = alacritty_terminal::event::WindowSize {
            num_cols: 80,
            num_lines: 24,
            cell_width: cell_w as u16,
            cell_height: cell_h as u16,
        };
        let (tx, rx) = futures::channel::mpsc::unbounded::<UiEvent>();
        let engine = match Engine::new(window_size, theme.clone(), tx) {
            Ok(engine) => engine,
            Err(err) => {
                // without a shell there is no terminal
                panic!("kuma-term: cannot spawn the shell: {err}");
            }
        };

        let pump = cx.spawn(async move |this, cx| {
            let mut rx = rx;
            while let Some(event) = rx.next().await {
                if this
                    .update_in(cx, |this, window, cx| this.handle_event(event, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        // the wallpaper republishes the shell palette; swap the chrome
        // live so the terminal follows the session like kuma-files does
        let palette_tick = cx.spawn(async move |this, cx| {
            let mut mtime = crate::theme::Theme::palette_mtime();
            loop {
                cx.background_executor().timer(Duration::from_secs(2)).await;
                let now = crate::theme::Theme::palette_mtime();
                if now == mtime {
                    continue;
                }
                mtime = now;
                if this
                    .update_in(cx, |this, _window, cx| this.apply_shell_palette(cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        Self {
            engine,
            theme,
            focus,
            font,
            cell_w,
            cell_h,
            cols: 80,
            lines: 24,
            title: None,
            selecting: false,
            row_shapes: FxHashMap::default(),
            _pump: pump,
            _palette_tick: palette_tick,
        }
    }

    fn handle_event(&mut self, event: UiEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            UiEvent::Wakeup => cx.notify(),
            UiEvent::Title(title) => {
                self.title = Some(title);
                // dynamic window titles are a later concern (needs the
                // window API surface for it)
            }
            UiEvent::ResetTitle => self.title = None,
            UiEvent::Bell => log::info!("bell"),
            UiEvent::Exit => {
                log::info!("child exited, closing");
                window.remove_window();
            }
            UiEvent::ColorRequest(index, format) => {
                let color = self.engine.resolve_index(index);
                self.engine.input(format(color));
            }
            UiEvent::TextAreaSizeRequest(format) => {
                self.engine.input(format(self.engine.window_size()));
            }
            UiEvent::ClipboardStore(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            UiEvent::PtyWrite(bytes) => self.engine.input(bytes),
        }
    }

    /// A republished shell palette: swap the chrome colors in place, no
    /// restart. The tick only calls this when the file changed.
    fn apply_shell_palette(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = Theme::shell_palette_text() {
            crate::theme::apply_shell_palette(&mut self.theme, &text);
            self.engine.set_theme(self.theme.clone());
            cx.notify();
        }
    }

    /// The shaped render for one grid row: cache hit returns the stored
    /// build, miss builds and stores. The key doubles as the text hash for
    /// gpui's line-layout cache, so an unchanged row pays neither the cell
    /// walk nor the shaper.
    fn row_render(&mut self, row: &Row) -> Arc<RowRender> {
        let key = SHAPE_DOMAIN_ROW ^ hash_row(row);
        if let Some(cached) = self.row_shapes.get(&key) {
            return cached.clone();
        }
        let built = RowRender {
            key,
            ..build_row(row, &self.font, &self.theme)
        };
        let built = Arc::new(built);
        if self.row_shapes.len() >= ROW_SHAPE_CACHE_CAP {
            self.row_shapes.clear();
        }
        self.row_shapes.insert(key, built.clone());
        built
    }

    /// Paste from the clipboard, bracketed when the shell asked for it.
    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else { return };
        let Some(text) = item.text() else { return };
        // newlines become carriage returns: that is what the shell's line
        // discipline expects from a paste
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let mut bytes = Vec::with_capacity(text.len() + 16);
        if self.engine.snapshot().bracketed_paste {
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(text.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
        } else {
            bytes.extend_from_slice(text.as_bytes());
        }
        self.engine.input(bytes);
    }

    fn on_key(&mut self, event: &gpui::KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let k: &Keystroke = &event.keystroke;

        // clipboard chords first; the shell never sees these
        if k.modifiers.control && k.modifiers.shift {
            match k.key.as_str() {
                "c" => {
                    if let Some(text) = self.engine.selection_text() {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                    cx.stop_propagation();
                    return;
                }
                "v" => {
                    cx.stop_propagation();
                    self.paste(cx);
                    return;
                }
                _ => {}
            }
        }

        let app_cursor = self.engine.app_cursor();
        if let Some(bytes) = encoder::encode(k, app_cursor) {
            // landing input collapses any active selection, like every
            // terminal; bare modifier presses do not
            self.engine.clear_selection();
            cx.stop_propagation();
            self.engine.input(bytes);
        }
    }

    /// The grid cell a window-coordinate pointer sits over, plus which half
    /// of the cell: both anchor the selection ends.
    fn cell_at(&self, x: f32, y: f32) -> (usize, usize, CellEdge) {
        let col = (((x - PADDING) / self.cell_w).floor().max(0.0) as usize)
            .min(self.cols.max(1) as usize - 1);
        let row = (((y - PADDING) / self.cell_h).floor().max(0.0) as usize)
            .min(self.lines.max(1) as usize - 1);
        let into_cell = (x - PADDING) - col as f32 * self.cell_w;
        let edge = if into_cell < self.cell_w / 2.0 { CellEdge::Left } else { CellEdge::Right };
        (row, col, edge)
    }

    fn on_mouse_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        let (row, col, edge) = self.cell_at(event.position.x.as_f32(), event.position.y.as_f32());
        let mode = match event.click_count {
            2 => SelectMode::Word,
            3.. => SelectMode::Line,
            _ => SelectMode::Char,
        };
        let offset = self.engine.display_offset();
        self.engine.begin_selection(
            GridPoint { line: row as i32 - offset as i32, column: col },
            edge,
            mode,
        );
        self.selecting = true;
        cx.notify();
    }

    fn on_mouse_move(
        &mut self,
        event: &gpui::MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selecting {
            return;
        }
        let (row, col, edge) = self.cell_at(event.position.x.as_f32(), event.position.y.as_f32());
        let offset = self.engine.display_offset();
        self.engine
            .update_selection(GridPoint { line: row as i32 - offset as i32, column: col }, edge);
        cx.notify();
    }

    fn on_mouse_up(
        &mut self,
        event: &gpui::MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left || !self.selecting {
            return;
        }
        self.selecting = false;
        self.engine.finish_selection();
        cx.notify();
    }

    fn on_wheel(&mut self, event: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let lines = match event.delta {
            ScrollDelta::Lines(point) => point.y,
            ScrollDelta::Pixels(point) => point.y.as_f32() / self.cell_h,
        };
        // a notch below one line still moves one line
        let lines = if lines.abs() < 1.0 {
            if lines < 0.0 {
                -1
            } else {
                1
            }
        } else {
            lines.round() as i32
        }
        .clamp(-8, 8);
        if lines == 0 {
            return;
        }

        let snapshot = self.engine.snapshot();
        if snapshot.alt_screen {
            // full-screen programs own the viewport: wheel becomes arrows,
            // which is what they expect
            let key = if lines < 0 { "\x1b[A" } else { "\x1b[B" };
            let bytes = key.repeat(lines.unsigned_abs() as usize).into_bytes();
            self.engine.input(bytes);
        } else {
            // wheel up shows older output
            self.engine.scroll(-lines);
        }
        cx.notify();
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // fit the grid to the viewport: rounded-down cell counts, so the
        // last row and column never clip
        let viewport = window.viewport_size();
        let usable_w = (viewport.width - px(PADDING * 2.0)).as_f32().max(1.0);
        let usable_h = (viewport.height - px(PADDING * 2.0)).as_f32().max(1.0);
        let want_cols = ((usable_w / self.cell_w).floor() as u16).max(2);
        let want_lines = ((usable_h / self.cell_h).floor() as u16).max(2);
        if want_cols != self.cols || want_lines != self.lines {
            self.cols = want_cols;
            self.lines = want_lines;
            self.engine.resize(want_cols, want_lines);
            cx.notify();
        }

        let snapshot = self.engine.snapshot();
        let font_size = px(self.theme.font_size_px());
        let rows = snapshot.rows.iter().map(|row| self.row_render(row)).collect();

        // one div for interactivity, one canvas for the painted grid. No
        // flex layout in the paint path
        let grid_w = self.cols as f32 * self.cell_w;
        let grid = GridPaint {
            rows,
            font_size,
            cell_w: self.cell_w,
            cell_h: self.cell_h,
            fg: self.theme.foreground,
            display_offset: snapshot.display_offset,
            selection: snapshot.selection,
            selection_color: {
                let mut color = hsla_of(self.theme.cursor);
                color.a *= 0.35;
                color
            },
        };
        let grid_h = snapshot.rows.len() as f32 * self.cell_h;
        // background alpha: the focused window shows the most wallpaper,
        // inactive windows dim toward solid for contrast
        let opacity = if window.is_window_active() {
            self.theme.background_opacity
        } else {
            self.theme.background_opacity_unfocused
        };
        let mut bg = hsla_of(self.theme.background);
        bg.a *= opacity;

        div()
            .id("terminal")
            .size_full()
            .flex()
            .flex_col()
            .p(px(PADDING))
            .bg(bg)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_scroll_wheel(cx.listener(Self::on_wheel))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_click(cx.listener(|this, _event: &gpui::ClickEvent, window, cx| {
                window.focus(&this.focus, cx);
            }))
            .child(
                canvas(|_, _, _| (), move |bounds, _, window, cx| grid.paint(bounds, window, cx))
                    .w(px(grid_w))
                    .h(px(grid_h)),
            )
    }
}

/// Content hash of a row's cells. Every input build_row reads lives in the
/// cells (the engine resolves theme colors into them), so this is a sound
/// key for the RowRender cache, and a fortiori for the text hash contract
/// of gpui's line-layout cache: same hash implies the same text.
fn hash_row(row: &Row) -> u64 {
    let mut hasher = rustc_hash::FxHasher::default();
    hasher.write_usize(row.cells.len());
    for cell in &row.cells {
        hasher.write_u32(cell.c as u32);
        hasher.write_u8(cell.fg.0);
        hasher.write_u8(cell.fg.1);
        hasher.write_u8(cell.fg.2);
        hasher.write_u8(cell.bg.0);
        hasher.write_u8(cell.bg.1);
        hasher.write_u8(cell.bg.2);
        let flags = cell.bold as u8
            | (cell.italic as u8) << 1
            | (cell.underline as u8) << 2
            | (cell.strikeout as u8) << 3
            | (cell.spacer as u8) << 4;
        hasher.write_u8(flags);
    }
    hasher.finish()
}

/// Measure the monospace cell from the real font: the shaped advance of `M`
/// for the width, its ascent plus descent for the height.
fn cell_metrics(
    font: &gpui::Font,
    font_size: Pixels,
    text_system: &WindowTextSystem,
) -> (f32, f32) {
    let layout = text_system.layout_line(
        "M",
        font_size,
        &[TextRun {
            len: 1,
            font: font.clone(),
            color: hsla_of(Rgb8(0, 0, 0)),
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    );
    let advance = layout.width.as_f32();
    let height = layout.ascent.as_f32() + layout.descent.as_f32();
    if advance > 0.0 && height > 0.0 {
        log::info!("cell metrics: {advance:.2} x {height:.2}px at {}px", font_size.as_f32());
        (advance, height)
    } else {
        warn!("font metrics came back empty; using a generic cell size");
        (8.0, font_size.as_f32() * 1.35)
    }
}

/// One painted row: the shaped text plus everything painted behind or
/// instead of it.
struct RowRender {
    /// this row's content hash: the key into both the view's cache and
    /// gpui's line-layout cache
    key: u64,
    text: String,
    runs: Vec<TextRun>,
    /// background rects, cell aligned: (column, length, color)
    bg_quads: Vec<(usize, usize, Rgb8)>,
    /// cells the vector layer draws
    glyphs: Vec<CellGlyph>,
}

/// A cell the vector layer draws: grid column, the glyph, its color.
#[derive(Clone, Copy)]
struct CellGlyph {
    col: usize,
    ch: char,
    fg: Rgb8,
}

/// Merge a row of cells into one display string plus its paint lists.
///
/// Trailing default cells (plain spaces on the default background) are
/// dropped, spacer cells are skipped, and adjacent same-style cells merge
/// into one run. Runs split only on shaping style (font, fg, underline,
/// strikeout); backgrounds collect separately as cell-aligned quads, which
/// also keeps the quads exact across wide characters.
fn build_row(row: &Row, font: &gpui::Font, theme: &Theme) -> RowRender {
    let default_bg = theme.background;

    // last cell worth drawing
    let mut end = row.cells.len();
    while end > 0 {
        let cell = &row.cells[end - 1];
        if cell.spacer {
            end -= 1;
            continue;
        }
        let plain = cell.c == ' ' && cell.bg == default_bg && !cell.underline && !cell.strikeout;
        if plain {
            end -= 1;
        } else {
            break;
        }
    }

    // grid column of every cell: a wide char leader counts two, its spacer
    // counts zero; everything else counts one
    let mut col_of = Vec::with_capacity(row.cells.len());
    let mut col = 0usize;
    for i in 0..row.cells.len() {
        col_of.push(col);
        let width = if row.cells[i].spacer {
            0
        } else if i + 1 < row.cells.len() && row.cells[i + 1].spacer {
            2
        } else {
            1
        };
        col += width;
    }

    let mut text = String::new();
    let mut runs: Vec<TextRun> = Vec::new();
    let mut glyphs: Vec<CellGlyph> = Vec::new();
    let mut bg_quads: Vec<(usize, usize, Rgb8)> = Vec::new();
    // (bold, italic, underline, strikeout, fg) of the run being built
    let mut current: Option<(bool, bool, bool, bool, Rgb8)> = None;
    // (start column, length, color) of the open background run
    let mut bg_open: Option<(usize, usize, Rgb8)> = None;

    for i in 0..end {
        let cell = &row.cells[i];
        if cell.spacer {
            continue;
        }

        // extend or close the open background quad at this cell's start
        let bg_color = if cell.bg == default_bg { None } else { Some(cell.bg) };
        match (bg_open.as_ref().map(|&(_, _, c)| c), bg_color) {
            (Some(c), Some(color)) if c == color => {
                let open = bg_open.as_mut().unwrap();
                open.1 = col_of[i] + cell_width(row, i) - open.0;
            }
            (Some(_), Some(color)) => {
                if let Some((col, len, c)) = bg_open.take() {
                    if len > 0 {
                        bg_quads.push((col, len, c));
                    }
                }
                bg_open = Some((col_of[i], cell_width(row, i), color));
            }
            (Some(_), None) => {
                if let Some((col, len, c)) = bg_open.take() {
                    if len > 0 {
                        bg_quads.push((col, len, c));
                    }
                }
            }
            (None, Some(color)) => {
                bg_open = Some((col_of[i], cell_width(row, i), color));
            }
            (None, None) => {}
        }

        // box drawing, block elements, braille: drawn as vector quads on
        // top of the row; the text slot becomes a blank that still wears
        // the cell's background
        let drawn = if glyphs::is_vector_glyph(cell.c) && cell.fg != cell.bg {
            glyphs.push(CellGlyph { col: col_of[i], ch: cell.c, fg: cell.fg });
            ' '
        } else {
            cell.c
        };
        text.push(drawn);

        let key = (cell.bold, cell.italic, cell.underline, cell.strikeout, cell.fg);
        let len_in_text = drawn.len_utf8();
        match &mut current {
            Some(prev) if *prev == key => {
                runs.last_mut().unwrap().len += len_in_text;
            }
            _ => {
                current = Some(key);
                let fg = hsla_of(cell.fg);
                let underline =
                    cell.underline.then(|| UnderlineStyle { thickness: px(1.0), color: Some(fg), wavy: false });
                let strikethrough = cell
                    .strikeout
                    .then(|| gpui::StrikethroughStyle { thickness: px(1.0), color: Some(fg) });
                let cell_font = gpui::Font {
                    weight: if cell.bold { FontWeight::BOLD } else { font.weight },
                    style: if cell.italic { FontStyle::Italic } else { font.style },
                    ..font.clone()
                };
                runs.push(TextRun {
                    len: len_in_text,
                    font: cell_font,
                    color: fg,
                    background_color: None,
                    underline,
                    strikethrough,
                });
            }
        }
    }
    flush_bg_at_end(&mut bg_quads, bg_open.take());

    RowRender { key: 0, text, runs, bg_quads, glyphs }
}

/// Push a finished background quad if it covers anything.
fn flush_bg_at_end(quads: &mut Vec<(usize, usize, Rgb8)>, open: Option<(usize, usize, Rgb8)>) {
    if let Some((col, len, color)) = open {
        if len > 0 {
            quads.push((col, len, color));
        }
    }
}

/// The grid width this cell contributes, mirroring the col_of pass.
fn cell_width(row: &Row, i: usize) -> usize {
    if row.cells[i].spacer {
        0
    } else if i + 1 < row.cells.len() && row.cells[i + 1].spacer {
        2
    } else {
        1
    }
}

/// The whole visible grid, painted directly: run backgrounds and vector
/// glyphs as quads, each row's text shaped and painted. No flex layout in
/// the hot path, and the data is rebuilt from the engine snapshot every
/// frame.
struct GridPaint {
    rows: Vec<Arc<RowRender>>,
    font_size: Pixels,
    cell_w: f32,
    cell_h: f32,
    fg: Rgb8,
    display_offset: usize,
    /// selection highlight, viewport row spans
    selection: Vec<SelectSpan>,
    selection_color: gpui::Hsla,
}
impl GridPaint {
    fn paint(self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let origin = bounds.origin;
        for (iy, row) in self.rows.iter().enumerate() {
            let row_y = origin.y + px(iy as f32 * self.cell_h);

            // run backgrounds, cell aligned
            for &(col, len, color) in &row.bg_quads {
                let quad_bounds = Bounds::new(
                    point(origin.x + px(col as f32 * self.cell_w), row_y),
                    size(px(len as f32 * self.cell_w), px(self.cell_h)),
                );
                window.paint_quad(fill(quad_bounds, hsla_of(color)));
            }

            // the selection wash sits over cell backgrounds, under text
            for span in &self.selection {
                let quad_bounds = Bounds::new(
                    point(
                        origin.x + px(span.start as f32 * self.cell_w),
                        origin.y + px(span.row as f32 * self.cell_h),
                    ),
                    size(px(span.len as f32 * self.cell_w), px(self.cell_h)),
                );
                window.paint_quad(fill(quad_bounds, self.selection_color));
            }

            // vector glyphs: box drawing, block elements, braille
            for g in &row.glyphs {
                for (x, y, w, h) in glyphs::cell_rects(g.ch, self.cell_w, self.cell_h) {
                    let quad_bounds = Bounds::new(
                        point(origin.x + px(g.col as f32 * self.cell_w + x), row_y + px(y)),
                        size(px(w), px(h)),
                    );
                    window.paint_quad(fill(quad_bounds, hsla_of(g.fg)));
                }
            }

            // the text itself; the layout comes from gpui's hash-keyed
            // cache, so an unchanged row never re-shapes (and never
            // materializes its text)
            if !row.text.is_empty() {
                let shaped = window.text_system().shape_line_by_hash(
                    row.key,
                    row.text.len(),
                    self.font_size,
                    &row.runs,
                    None,
                    || SharedString::from(row.text.clone()),
                );
                if let Err(err) = shaped.paint(
                    point(origin.x, row_y),
                    px(self.cell_h),
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                ) {
                    warn!("row paint failed: {err}");
                }
            }
        }

        // scrolled back into history: a thin edge mark, since the
        // viewport no longer shows the live grid
        if self.display_offset > 0 {
            let quad_bounds = Bounds::new(
                point(origin.x + bounds.size.width - px(3.0), origin.y),
                size(px(3.0), bounds.size.height),
            );
            window.paint_quad(fill(quad_bounds, hsla_of(self.fg.scale(0.4))));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::RenderCell;
    use crate::theme::Theme;

    fn theme() -> Theme {
        Theme::builtin()
    }

    fn cell(c: char) -> RenderCell {
        let t = theme();
        RenderCell {
            c,
            fg: t.foreground,
            bg: t.background,
            bold: false,
            italic: false,
            underline: false,
            strikeout: false,
            spacer: false,
        }
    }

    fn font() -> gpui::Font {
        gpui::Font { family: "monospace".into(), ..Default::default() }
    }

    #[test]
    fn plain_rows_trim_trailing_space() {
        let mut row = Row { cells: vec![cell('h'), cell('i')] };
        row.cells.push(cell(' '));
        row.cells.push(cell(' '));
        let font = font();
        let t = theme();
        let rendered = build_row(&row, &font, &t);
        assert_eq!(rendered.text, "hi");
        assert_eq!(rendered.runs.len(), 1);
        assert_eq!(rendered.runs[0].len, 2);
        assert!(rendered.bg_quads.is_empty());
    }

    #[test]
    fn styled_runs_split_on_color_change() {
        let mut red = cell('r');
        red.fg = Rgb8(255, 0, 0);
        let green = cell('g');
        let row = Row { cells: vec![red, green] };
        let font = font();
        let t = theme();
        let rendered = build_row(&row, &font, &t);
        assert_eq!(rendered.text, "rg");
        assert_eq!(rendered.runs.len(), 2);
        assert_eq!(rendered.runs[0].len, 1);
        assert_eq!(rendered.runs[1].len, 1);
    }

    #[test]
    fn colored_background_becomes_a_cell_aligned_quad() {
        let mut marked = cell(' ');
        marked.bg = Rgb8(40, 40, 40);
        let row = Row { cells: vec![cell('a'), cell(' '), marked] };
        let font = font();
        let t = theme();
        let rendered = build_row(&row, &font, &t);
        // the marked background cell is not trailing-default, so it stays;
        // the background rides as a quad, not inside the run
        assert_eq!(rendered.text, "a  ");
        // fg is equal across the row, so the shaping run merges fully
        assert_eq!(rendered.runs.len(), 1);
        assert_eq!(rendered.runs[0].len, 3);
        assert_eq!(rendered.bg_quads, vec![(2, 1, Rgb8(40, 40, 40))]);
    }

    #[test]
    fn background_quads_span_wide_characters() {
        let mut wide = cell('漢');
        wide.bg = Rgb8(40, 40, 40);
        let mut spacer = cell(' ');
        spacer.spacer = true;
        let row = Row { cells: vec![wide, spacer] };
        let font = font();
        let t = theme();
        let rendered = build_row(&row, &font, &t);
        assert_eq!(rendered.bg_quads, vec![(0, 2, Rgb8(40, 40, 40))]);
    }

    #[test]
    fn empty_rows_paint_nothing() {
        let row = Row { cells: vec![] };
        let font = font();
        let t = theme();
        let rendered = build_row(&row, &font, &t);
        assert_eq!(rendered.text, "");
        assert!(rendered.runs.is_empty());
        assert!(rendered.bg_quads.is_empty());
    }

    #[test]
    fn box_drawing_becomes_a_blank_plus_an_overlay() {
        let row = Row { cells: vec![cell('\u{2500}'), cell('x')] };
        let font = font();
        let t = theme();
        let rendered = build_row(&row, &font, &t);
        // the box cell is a space in the text (background intact), the
        // glyph rides in the overlay list at grid column 0
        assert_eq!(rendered.text, " x");
        assert_eq!(rendered.glyphs.len(), 1);
        assert_eq!(rendered.glyphs[0].col, 0);
        assert_eq!(rendered.glyphs[0].ch, '\u{2500}');
        assert_eq!(rendered.runs[0].len, 2);
    }

    #[test]
    fn overlay_columns_survive_wide_chars() {
        let mut spacer = cell(' ');
        spacer.spacer = true;
        let row = Row { cells: vec![cell('a'), cell('漢'), spacer, cell('\u{2588}')] };
        let font = font();
        let t = theme();
        let rendered = build_row(&row, &font, &t);
        // the wide pair eats columns 1 and 2, so the block lands at 3
        assert_eq!(rendered.glyphs.len(), 1);
        assert_eq!(rendered.glyphs[0].col, 3);
    }

    #[test]
    fn invisible_vector_glyphs_stay_on_text() {
        // fg == bg: the overlay would paint nothing visible, leave the
        // char to the shaper
        let mut hidden = cell('\u{2500}');
        hidden.fg = hidden.bg;
        let row = Row { cells: vec![hidden] };
        let font = font();
        let t = theme();
        let rendered = build_row(&row, &font, &t);
        assert_eq!(rendered.text, "\u{2500}");
        assert!(rendered.glyphs.is_empty());
    }

    #[test]
    fn row_hash_is_stable_per_content_and_sensitive_to_changes() {
        let mut a = Row { cells: vec![RenderCell {
            c: 'x', fg: Rgb8(1, 2, 3), bg: Rgb8(0, 0, 0),
            bold: false, italic: false, underline: false, strikeout: false, spacer: false,
        }] };
        let b = Row { cells: vec![RenderCell {
            c: 'x', fg: Rgb8(1, 2, 3), bg: Rgb8(0, 0, 0),
            bold: false, italic: false, underline: false, strikeout: false, spacer: false,
        }] };
        assert_eq!(hash_row(&a), hash_row(&b));

        // every field build_row reads must move the hash
        a.cells[0].c = 'y';
        assert_ne!(hash_row(&a), hash_row(&b));
        a.cells[0].c = 'x';
        a.cells[0].fg = Rgb8(9, 2, 3);
        assert_ne!(hash_row(&a), hash_row(&b));
        a.cells[0].fg = Rgb8(1, 2, 3);
        a.cells[0].bg = Rgb8(9, 9, 9);
        assert_ne!(hash_row(&a), hash_row(&b));
        a.cells[0].bg = Rgb8(0, 0, 0);
        a.cells[0].bold = true;
        assert_ne!(hash_row(&a), hash_row(&b));
        a.cells[0].bold = false;
        a.cells[0].spacer = true;
        assert_ne!(hash_row(&a), hash_row(&b));
    }

    #[test]
    fn row_hash_collapses_equivalent_flag_truthiness() {
        // identical cells built twice hash identically regardless of
        // struct instance
        let mk = |bold| Row { cells: vec![RenderCell {
            c: 'q', fg: Rgb8(5, 5, 5), bg: Rgb8(0, 0, 0),
            bold, italic: false, underline: false, strikeout: false, spacer: false,
        }] };
        assert_eq!(hash_row(&mk(true)), hash_row(&mk(true)));
        assert_ne!(hash_row(&mk(true)), hash_row(&mk(false)));
    }
}
