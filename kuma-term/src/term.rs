//! The engine seam: everything alacritty_terminal-specific lives in this
//! module. The rest of kuma-term sees only `Engine`, `Snapshot`, `UiEvent`
//! and raw bytes, so the emulator behind the seam stays swappable.
//!
//! Ownership model: the PTY reader thread (alacritty's EventLoop) and the UI
//! thread share the term behind a fair mutex. The reader parses bytes into
//! the grid and wakes the UI through the event proxy; the UI never touches
//! the PTY, it only pushes encoded bytes and resize requests back through
//! the event loop's channel.

use std::io;
use std::sync::Arc;

use alacritty_terminal::event::{Event, EventListener, Notify, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, Msg, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{CursorShape, NamedColor, Rgb};

use crate::palette::{self, Rgb8};
use futures::channel::mpsc::UnboundedSender;

/// Events the engine forwards to the UI thread. Alacritty's own event type
/// is rewrapped here so its callback types never escape the seam.
pub enum UiEvent {
    /// Grid changed, the view should re-render.
    Wakeup,
    /// The shell set the window title (OSC 0/2).
    Title(String),
    ResetTitle,
    Bell,
    /// The child process is gone.
    Exit,
    /// A color query (OSC 4/10/11): the UI resolves the index to a color and
    /// the callback encodes the reply for the PTY.
    ColorRequest(usize, Arc<dyn Fn(Rgb8) -> Vec<u8> + Send + Sync>),
    /// A text-area size query (CSI 14t): same shape as ColorRequest, the
    /// callback wants the current WindowSize.
    TextAreaSizeRequest(Arc<dyn Fn(WindowSize) -> Vec<u8> + Send + Sync>),
    /// The shell asked to write to the clipboard (OSC 52, copy only by
    /// default per the emulator's security default).
    ClipboardStore(String),
    /// The shell asked to write bytes to the PTY (DA replies, DSR, and the
    /// replies to the queries above).
    PtyWrite(Vec<u8>),
}

/// The event proxy: lives on the PTY reader thread, forwards into the UI
/// channel.
#[derive(Clone)]
pub struct UiProxy {
    tx: UnboundedSender<UiEvent>,
}

impl UiProxy {
    fn forward(&self, event: UiEvent) {
        let _ = self.tx.unbounded_send(event);
    }
}

impl EventListener for UiProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::Wakeup => self.forward(UiEvent::Wakeup),
            Event::MouseCursorDirty | Event::CursorBlinkingChange => self.forward(UiEvent::Wakeup),
            Event::Title(title) => self.forward(UiEvent::Title(title)),
            Event::ResetTitle => self.forward(UiEvent::ResetTitle),
            Event::Bell => self.forward(UiEvent::Bell),
            Event::Exit => self.forward(UiEvent::Exit),
            Event::ChildExit(_) => self.forward(UiEvent::Exit),
            Event::ClipboardStore(_, text) => self.forward(UiEvent::ClipboardStore(text)),
            Event::PtyWrite(text) => self.forward(UiEvent::PtyWrite(text.into_bytes())),
            Event::ColorRequest(index, format) => {
                let f = Arc::new(move |c: Rgb8| format(Rgb { r: c.0, g: c.1, b: c.2 }).into_bytes());
                self.forward(UiEvent::ColorRequest(index, f));
            }
            Event::TextAreaSizeRequest(format) => {
                let f = Arc::new(move |ws: WindowSize| format(ws).into_bytes());
                self.forward(UiEvent::TextAreaSizeRequest(f));
            }
            Event::ClipboardLoad(_, _) => {
                // paste-from-terminal (application asks the terminal for its
                // clipboard): deliberately unsupported, like copy-heavy
                // defaults elsewhere
            }
        }
    }
}

/// Grid dimensions for term construction and resize.
struct GridDims {
    columns: usize,
    screen_lines: usize,
}

impl GridDims {
    fn from_window_size(ws: WindowSize) -> Self {
        Self { columns: ws.num_cols as usize, screen_lines: ws.num_lines as usize }
    }
}

impl Dimensions for GridDims {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// One rendered cell. Spacers (the second half of a wide char) are kept for
/// column arithmetic but skipped when the display string is built.
#[derive(Clone, Copy, Debug)]
pub struct RenderCell {
    pub c: char,
    pub fg: Rgb8,
    pub bg: Rgb8,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikeout: bool,
    /// true for the second half of a wide char: no glyph, no width
    pub spacer: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Row {
    pub cells: Vec<RenderCell>,
}

/// What the cursor looks like, in viewport coordinates.
#[derive(Clone, Copy, Debug)]
pub struct CursorSpot {
    pub row: usize,
    /// kept for the hit-testing and overlay cursor work after the spike
    #[allow(dead_code)]
    pub col: usize,
}

/// A frame's worth of terminal state, plain data for the renderer.
pub struct Snapshot {
    pub rows: Vec<Row>,
    pub cursor: Option<CursorSpot>,
    pub alt_screen: bool,
    pub bracketed_paste: bool,
    /// how far the viewport is scrolled back into history (0 = live)
    pub display_offset: usize,
}

pub struct Engine {
    term: Arc<FairMutex<Term<UiProxy>>>,
    notifier: Notifier,
    loop_tx: alacritty_terminal::event_loop::EventLoopSender,
    window_size: WindowSize,
}

impl Engine {
    /// Spawn the shell, wire the PTY reader thread. `window_size` carries
    /// the initial cell geometry (measured from the real font by the view).
    pub fn new(window_size: WindowSize, tx: UnboundedSender<UiEvent>) -> io::Result<Self> {
        // TERM and COLORTERM for the child; alacritty's own helper picks
        // alacritty terminfo when installed, xterm-256color otherwise
        tty::setup_env();

        let options = tty::Options::default();
        let pty = tty::new(&options, window_size, 0)?;

        let proxy = UiProxy { tx };
        let term = Term::new(
            alacritty_terminal::term::Config::default(),
            &GridDims::from_window_size(window_size),
            proxy.clone(),
        );
        let term = Arc::new(FairMutex::new(term));

        // drain_on_exit: flush the child's final output before the window
        // closes, so `exit` leaves clean scrollback behind
        let event_loop = EventLoop::new(term.clone(), proxy, pty, true, false)?;
        let loop_tx = event_loop.channel();
        event_loop.spawn();

        Ok(Self { term, notifier: Notifier(loop_tx.clone()), loop_tx, window_size })
    }

    /// Write bytes to the PTY (user input, query replies).
    pub fn input(&self, bytes: Vec<u8>) {
        self.notifier.notify(bytes);
    }

    /// Resize the grid and PTY. Cell pixel size is unchanged (font fixed for
    /// the spike); only the counts move.
    pub fn resize(&mut self, cols: u16, lines: u16) {
        let ws = WindowSize {
            num_lines: lines,
            num_cols: cols,
            cell_width: self.window_size.cell_width,
            cell_height: self.window_size.cell_height,
        };
        if ws.num_cols == self.window_size.num_cols && ws.num_lines == self.window_size.num_lines {
            return;
        }
        self.window_size = ws;
        // the PTY first so SIGWINCH lands with the new size already set
        let _ = self.loop_tx.send(Msg::Resize(ws));
        self.term.lock().resize(GridDims::from_window_size(ws));
    }

    /// Scroll the viewport by lines (positive = back into history). No-op on
    /// the alternate screen, where the program owns scrolling.
    pub fn scroll(&self, lines: i32) {
        let mut term = self.term.lock();
        if term.mode().contains(TermMode::ALT_SCREEN) {
            return;
        }
        let scroll = Scroll::Delta(lines);
        term.scroll_display(scroll);
    }

    pub fn window_size(&self) -> WindowSize {
        self.window_size
    }

    /// Resolve a raw palette index (0..269) for OSC 4 replies.
    pub fn resolve_index(&self, index: usize) -> Rgb8 {
        let term = self.term.lock();
        palette::resolve_index(index, term.colors())
    }

    /// The shell's current input mode flags the encoder needs.
    pub fn app_cursor(&self) -> bool {
        self.term.lock().mode().contains(TermMode::APP_CURSOR)
    }

    /// Take a snapshot of the visible grid for the renderer.
    pub fn snapshot(&self) -> Snapshot {
        let term = self.term.lock();
        let content = term.renderable_content();
        let offset = content.display_offset;
        let screen_lines = term.screen_lines();
        let columns = term.columns();
        let mode = content.mode;

        let mut rows: Vec<Row> = Vec::with_capacity(screen_lines);
        for _ in 0..screen_lines {
            rows.push(Row { cells: Vec::with_capacity(columns) });
        }

        for indexed in content.display_iter {
            let viewport_row = indexed.point.line.0 + offset as i32;
            if viewport_row < 0 || viewport_row as usize >= screen_lines {
                continue;
            }
            let row = &mut rows[viewport_row as usize];
            let cell = indexed.cell;
            let flags = cell.flags;

            if flags.contains(Flags::WIDE_CHAR_SPACER) {
                row.cells.push(RenderCell {
                    c: ' ',
                    fg: palette::default_fg(),
                    bg: palette::default_bg(),
                    bold: false,
                    italic: false,
                    underline: false,
                    strikeout: false,
                    spacer: true,
                });
                continue;
            }

            let mut fg = palette::resolve(cell.fg, content.colors);
            let mut bg = palette::resolve(cell.bg, content.colors);
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if flags.contains(Flags::HIDDEN) {
                fg = bg;
            }
            if flags.contains(Flags::DIM) {
                fg = fg.scale(2.0 / 3.0);
            }

            row.cells.push(RenderCell {
                c: if flags.contains(Flags::HIDDEN) { ' ' } else { cell.c },
                fg,
                bg,
                bold: flags.contains(Flags::BOLD),
                italic: flags.contains(Flags::ITALIC),
                underline: flags.contains(Flags::UNDERLINE),
                strikeout: flags.contains(Flags::STRIKEOUT),
                spacer: false,
            });
        }

        // block cursor, rendered by restyling the cell under it
        let mut cursor = None;
        if offset == 0 && content.cursor.shape != CursorShape::Hidden {
            let point = content.cursor.point;
            let line = point.line.0 as usize;
            let col = point.column.0;
            if line < screen_lines && col < columns {
                let cell = &mut rows[line].cells[col];
                // foreground becomes the cursor color, background keeps
                // what the cell's foreground was: the classic inverted block
                let cursor_color = term.colors()[NamedColor::Cursor]
                    .map(|c| palette::Rgb8(c.r, c.g, c.b))
                    .unwrap_or_else(palette::default_fg);
                cell.bg = cursor_color;
                std::mem::swap(&mut cell.fg, &mut cell.bg);
                cursor = Some(CursorSpot { row: line, col });
            }
        }

        Snapshot {
            rows,
            cursor,
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
            display_offset: offset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::vte::ansi::Processor;

    /// A term wired to a dead channel: events are fire-and-forget in tests.
    fn test_term(cols: usize, lines: usize) -> Term<UiProxy> {
        let (tx, _rx) = futures::channel::mpsc::unbounded::<UiEvent>();
        Term::new(
            alacritty_terminal::term::Config::default(),
            &GridDims { columns: cols, screen_lines: lines },
            UiProxy { tx },
        )
    }

    fn feed(term: &mut Term<UiProxy>, bytes: &[u8]) {
        let mut parser = Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
        parser.advance(term, bytes);
    }

    fn text_of_row(term: &Term<UiProxy>, row: usize) -> String {
        let snapshot = term.renderable_content();
        let mut out = String::new();
        // rows arrive in viewport order, top to bottom
        for indexed in snapshot.display_iter {
            if (indexed.point.line.0 as usize) == row {
                out.push(indexed.cell.c);
            }
        }
        out
    }

    #[test]
    fn plain_bytes_land_on_the_grid() {
        let mut term = test_term(10, 3);
        feed(&mut term, b"hello");
        // the grid pads rows to the column count
        assert_eq!(text_of_row(&term, 0).trim_end(), "hello");
    }

    #[test]
    fn history_scrolls_and_display_offset_moves() {
        let mut term = test_term(10, 3);
        for _ in 0..10 {
            feed(&mut term, b"line\r\n");
        }
        // viewport shows the last 3 lines; the earlier ones live in history
        assert_eq!(term.grid().display_offset(), 0);
        term.scroll_display(Scroll::Delta(10));
        assert!(term.grid().display_offset() > 0);
    }

    #[test]
    fn mode_flags_cross_the_seam() {
        let mut term = test_term(10, 3);
        assert!(!term.mode().contains(TermMode::ALT_SCREEN));
        feed(&mut term, b"\x1b[?1049h");
        assert!(term.mode().contains(TermMode::ALT_SCREEN));
        feed(&mut term, b"\x1b[?1h");
        assert!(term.mode().contains(TermMode::APP_CURSOR));
        feed(&mut term, b"\x1b[?2004h");
        assert!(term.mode().contains(TermMode::BRACKETED_PASTE));
    }

    #[test]
    fn truecolor_and_indexed_cells_resolve() {
        let mut term = test_term(10, 3);
        feed(&mut term, b"\x1b[38;5;196mX\x1b[0m");
        let content = term.renderable_content();
        for indexed in content.display_iter {
            if indexed.cell.c == 'X' {
                let fg = palette::resolve(indexed.cell.fg, content.colors);
                assert_eq!(fg, palette::Rgb8(255, 0, 0));
                return;
            }
        }
        panic!("no X cell found");
    }

    #[test]
    fn snapshot_marks_wide_chars_and_spacers() {
        let mut term = test_term(10, 3);
        // a wide CJK char followed by a normal one
        feed(&mut term, "漢x".as_bytes());
        let content = term.renderable_content();
        let mut row0: Vec<(char, bool)> = Vec::new();
        for indexed in content.display_iter {
            if indexed.point.line.0 == 0 {
                row0.push((
                    indexed.cell.c,
                    indexed.cell.flags.contains(Flags::WIDE_CHAR_SPACER),
                ));
            }
        }
        // wide char first, spacer second, then the x
        assert_eq!(row0[0].1, false);
        assert_eq!(row0[1].1, true);
        assert_eq!(row0[2].1, false);
    }
}
