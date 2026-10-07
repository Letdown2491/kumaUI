//! The gpui view: renders engine snapshots as styled text lines, feeds
//! keystrokes and scroll events back into the engine.
//!
//! Rendering strategy for the spike: each visible grid line becomes one
//! `StyledText` element whose runs merge adjacent cells sharing a style.
//! Backgrounds ride inside the runs (gpui paints them behind the glyphs), so
//! one pass over a row produces both the string and its paint list. Damage
//! tracking and a purpose-built element come after the spike.

use futures::StreamExt;
use gpui::{
    ClipboardItem, Context, FocusHandle, Focusable, FontStyle, FontWeight, IntoElement, Keystroke,
    ParentElement, Pixels, Render, ScrollDelta, ScrollWheelEvent, Styled, StyledText, Task,
    TextRun, UnderlineStyle, Window, WindowTextSystem, div, px, prelude::*,
};
use log::warn;

use crate::encoder;
use crate::font;
use crate::glyphs;
use crate::palette::Rgb8;
use crate::term::{Engine, Row, UiEvent};
use crate::theme::Theme;

const PADDING: f32 = 8.0;

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
    // the pump task aborts if dropped, so it stays owned by the view
    _pump: Task<()>,
}

impl TerminalView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let theme = Theme::load();
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
            _pump: pump,
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
                    // selection copy arrives with selection support
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
            cx.stop_propagation();
            self.engine.input(bytes);
        }
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
        let default_bg = self.theme.background;
        let font_size = px(self.theme.font_size_px());

        let mut row_elements = Vec::with_capacity(snapshot.rows.len());
        let (cw, chh) = (self.cell_w, self.cell_h);
        for (ix, row) in snapshot.rows.iter().enumerate() {
            let on_cursor_row = matches!(snapshot.cursor, Some(c) if c.row == ix);
            let (text, runs, glyphs) = build_line(row, &self.font, font_size, on_cursor_row, &self.theme);
            // vector glyphs paint as absolutely positioned quads over the
            // row text, one div per rect; ordering is children order, so
            // they land above the runs' backgrounds
            let overlays = glyphs.into_iter().flat_map(move |g| {
                glyphs::cell_rects(g.ch, cw, chh).into_iter().map(move |(x, y, w, h)| {
                    div()
                        .absolute()
                        .left(px(g.col as f32 * cw + x))
                        .top(px(y))
                        .w(px(w))
                        .h(px(h))
                        .bg(hsla_of(g.fg))
                })
            });
            row_elements.push(
                div()
                    .relative()
                    .h(px(self.cell_h))
                    .overflow_hidden()
                    .child(StyledText::new(text).with_runs(runs))
                    .children(overlays),
            );
        }

        div()
            .id("terminal")
            .relative()
            .size_full()
            .p(px(PADDING))
            .bg(hsla_of(default_bg))
            // the grid is measured in theme cells; every StyledText row
            // inherits this, so it must match cell_metrics
            .text_size(font_size)
            .font_family(self.font.family.clone())
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_scroll_wheel(cx.listener(Self::on_wheel))
            .on_click(cx.listener(|this, _event: &gpui::ClickEvent, window, cx| {
                window.focus(&this.focus, cx);
            }))
            .child(div().flex().flex_col().children(row_elements))
            // scrolled back into history: a thin edge mark, since the
            // viewport no longer shows the live grid
            .when(snapshot.display_offset > 0, |edge| {
                edge.child(
                    div()
                        .absolute()
                        .top_0()
                        .right_0()
                        .w(px(3.0))
                        .h_full()
                        .bg(hsla_of(self.theme.foreground.scale(0.4))),
                )
            })
    }
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

/// Merge a row of cells into one display string plus styled runs.
///
/// Trailing default cells (plain spaces on the default background) are
/// dropped, spacer cells are skipped, and adjacent same-style cells merge
/// into one run. The cursor row is marked by the caller so its restyled cell
/// keeps its painted background.
/// A cell the vector layer draws: grid column, the glyph, its color.
#[derive(Clone, Copy)]
struct CellGlyph {
    col: usize,
    ch: char,
    fg: Rgb8,
}

fn build_line(
    row: &Row,
    font: &gpui::Font,
    font_size: Pixels,
    on_cursor_row: bool,
    theme: &Theme,
) -> (String, Vec<TextRun>, Vec<CellGlyph>) {
    let default_bg = theme.background;
    let _ = (font_size, on_cursor_row);

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
    // (bold, italic, underline, strikeout, fg, bg) of the run being built
    let mut current: Option<(bool, bool, bool, bool, Rgb8, Rgb8)> = None;

    for i in 0..end {
        let cell = &row.cells[i];
        if cell.spacer {
            continue;
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

        let key = (cell.bold, cell.italic, cell.underline, cell.strikeout, cell.fg, cell.bg);
        let len_in_text = drawn.len_utf8();
        match &mut current {
            Some(prev) if *prev == key => {
                runs.last_mut().unwrap().len += len_in_text;
            }
            _ => {
                current = Some(key);
                let fg = hsla_of(cell.fg);
                let background_color =
                    if cell.bg == default_bg { None } else { Some(hsla_of(cell.bg)) };
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
                    background_color,
                    underline,
                    strikethrough,
                });
            }
        }
    }

    if text.is_empty() {
        // keep the row height stable for untouched lines
        text.push(' ');
        runs.push(TextRun {
            len: 1,
            font: font.clone(),
            color: hsla_of(theme.foreground),
            background_color: None,
            underline: None,
            strikethrough: None,
        });
    }

    (text, runs, glyphs)
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
        let (text, runs, _) = build_line(&row, &font, px(14.0), false, &t);
        assert_eq!(text, "hi");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].len, 2);
    }

    #[test]
    fn styled_runs_split_on_color_change() {
        let mut red = cell('r');
        red.fg = Rgb8(255, 0, 0);
        let green = cell('g');
        let row = Row { cells: vec![red, green] };
        let font = font();
        let t = theme();
        let (text, runs, _) = build_line(&row, &font, px(14.0), false, &t);
        assert_eq!(text, "rg");
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].len, 1);
        assert_eq!(runs[1].len, 1);
    }

    #[test]
    fn colored_background_survives_trimming() {
        let mut marked = cell(' ');
        marked.bg = Rgb8(40, 40, 40);
        let row = Row { cells: vec![cell('a'), cell(' '), marked] };
        let font = font();
        let t = theme();
        let (text, runs, _) = build_line(&row, &font, px(14.0), false, &t);
        // the marked background cell is not trailing-default, so it stays
        assert_eq!(text, "a  ");
        assert_eq!(runs.len(), 2);
        assert!(runs[1].background_color.is_some());
    }

    #[test]
    fn empty_rows_collapse_to_a_placeholder_space() {
        let row = Row { cells: vec![] };
        let font = font();
        let t = theme();
        let (text, runs, _) = build_line(&row, &font, px(14.0), false, &t);
        assert_eq!(text, " ");
        assert_eq!(runs.len(), 1);
    }

    #[test]
    fn box_drawing_becomes_a_blank_plus_an_overlay() {
        let row = Row { cells: vec![cell('\u{2500}'), cell('x')] };
        let font = font();
        let t = theme();
        let (text, runs, glyphs) = build_line(&row, &font, px(14.0), false, &t);
        // the box cell is a space in the text (background intact), the
        // glyph rides in the overlay list at grid column 0
        assert_eq!(text, " x");
        assert_eq!(glyphs.len(), 1);
        assert_eq!(glyphs[0].col, 0);
        assert_eq!(glyphs[0].ch, '\u{2500}');
        assert_eq!(runs[0].len, 2);
    }

    #[test]
    fn overlay_columns_survive_wide_chars() {
        let mut spacer = cell(' ');
        spacer.spacer = true;
        let row = Row { cells: vec![cell('a'), cell('漢'), spacer, cell('\u{2588}')] };
        let font = font();
        let t = theme();
        let (_, _, glyphs) = build_line(&row, &font, px(14.0), false, &t);
        // the wide pair eats columns 1 and 2, so the block lands at 3
        assert_eq!(glyphs.len(), 1);
        assert_eq!(glyphs[0].col, 3);
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
        let (text, _, glyphs) = build_line(&row, &font, px(14.0), false, &t);
        assert_eq!(text, "\u{2500}");
        assert!(glyphs.is_empty());
    }
}
