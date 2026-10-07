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
use crate::palette::{self, Rgb8};
use crate::term::{Engine, Row, UiEvent};

const FONT_SIZE: f32 = 14.0;
const PADDING: f32 = 8.0;

fn hsla_of(c: Rgb8) -> gpui::Hsla {
    let hex = ((c.0 as u32) << 16) | ((c.1 as u32) << 8) | c.2 as u32;
    gpui::rgb(hex).into()
}

pub struct TerminalView {
    engine: Engine,
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
        let font = gpui::Font { family: "monospace".into(), ..Default::default() };
        let (cell_w, cell_h) = cell_metrics(&font, window.text_system());

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
        let engine = match Engine::new(window_size, tx) {
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
        let default_bg = palette::default_bg();
        let font_size = px(FONT_SIZE);

        let mut row_elements = Vec::with_capacity(snapshot.rows.len());
        for (ix, row) in snapshot.rows.iter().enumerate() {
            let on_cursor_row = matches!(snapshot.cursor, Some(c) if c.row == ix);
            let (text, runs) = build_line(row, &self.font, font_size, on_cursor_row);
            row_elements.push(
                div()
                    .h(px(self.cell_h))
                    .overflow_hidden()
                    .child(StyledText::new(text).with_runs(runs)),
            );
        }

        div()
            .id("terminal")
            .relative()
            .size_full()
            .p(px(PADDING))
            .bg(hsla_of(default_bg))
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
                        .bg(hsla_of(palette::default_fg().scale(0.4))),
                )
            })
    }
}

/// Measure the monospace cell from the real font: the shaped advance of `M`
/// for the width, its ascent plus descent for the height.
fn cell_metrics(font: &gpui::Font, text_system: &WindowTextSystem) -> (f32, f32) {
    let size = px(FONT_SIZE);
    let layout = text_system.layout_line(
        "M",
        size,
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
        (advance, height)
    } else {
        warn!("font metrics came back empty; using a generic cell size");
        (8.0, FONT_SIZE * 1.35)
    }
}

/// Merge a row of cells into one display string plus styled runs.
///
/// Trailing default cells (plain spaces on the default background) are
/// dropped, spacer cells are skipped, and adjacent same-style cells merge
/// into one run. The cursor row is marked by the caller so its restyled cell
/// keeps its painted background.
fn build_line(
    row: &Row,
    font: &gpui::Font,
    font_size: Pixels,
    on_cursor_row: bool,
) -> (String, Vec<TextRun>) {
    let default_bg = palette::default_bg();
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

    let mut text = String::new();
    let mut runs: Vec<TextRun> = Vec::new();
    // (bold, italic, underline, strikeout, fg, bg) of the run being built
    let mut current: Option<(bool, bool, bool, bool, Rgb8, Rgb8)> = None;

    for cell in &row.cells[..end] {
        if cell.spacer {
            continue;
        }
        text.push(cell.c);

        let key = (cell.bold, cell.italic, cell.underline, cell.strikeout, cell.fg, cell.bg);
        match &mut current {
            Some(prev) if *prev == key => {
                runs.last_mut().unwrap().len += cell.c.len_utf8();
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
                    len: cell.c.len_utf8(),
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
            color: hsla_of(palette::default_fg()),
            background_color: None,
            underline: None,
            strikethrough: None,
        });
    }

    (text, runs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::RenderCell;

    fn cell(c: char) -> RenderCell {
        RenderCell {
            c,
            fg: palette::default_fg(),
            bg: palette::default_bg(),
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
        let (text, runs) = build_line(&row, &font, px(14.0), false);
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
        let (text, runs) = build_line(&row, &font, px(14.0), false);
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
        let (text, runs) = build_line(&row, &font, px(14.0), false);
        // the marked background cell is not trailing-default, so it stays
        assert_eq!(text, "a  ");
        assert_eq!(runs.len(), 2);
        assert!(runs[1].background_color.is_some());
    }

    #[test]
    fn empty_rows_collapse_to_a_placeholder_space() {
        let row = Row { cells: vec![] };
        let font = font();
        let (text, runs) = build_line(&row, &font, px(14.0), false);
        assert_eq!(text, " ");
        assert_eq!(runs.len(), 1);
    }
}
