//! The engine seam: everything alacritty_terminal-specific lives in this
//! module. The rest of kuma-term sees only `Engine`, `Snapshot`, `UiEvent`
//! and raw bytes, so the emulator behind the seam stays swappable.
//!
//! Ownership model: the pump thread reads the PTY and shares the term
//! behind a fair mutex; the UI never touches the PTY read side, it pushes
//! encoded bytes back through the master's write half.

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::thread;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::search::RegexSearch;
use alacritty_terminal::term::{RenderableContent, Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Processor, Rgb};

/// The cursor shape rides through the term seam so the view never
/// touches alacritty directly.
pub use alacritty_terminal::vte::ansi::{CursorShape, CursorStyle};

use crate::palette::{self, Rgb8};
use crate::theme::Theme;
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
/// Grid dimensions for term construction and resize. Scrollback is NOT
/// carried here: the emulator's Term::new reads only columns and
/// screen_lines off the dims, and the history size rides the term
/// config's scrolling_history instead.
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
/// Where the cursor paints this frame, and what it looks like: the
/// block's colors bake into the row's own cells, the other shapes
/// paint as overlays in the view.
pub struct CursorSpot {
    pub row: usize,
    pub col: usize,
    /// the DECSCUSR shape in effect this frame
    pub shape: CursorShape,
    /// the style asks for a blink; the view animates it while focused
    pub blinking: bool,
}

/// A grid position for selection, in buffer coordinates: line 0 is the top
/// of the live screen, negative lines are scrollback history. The view
/// converts pointer pixels into this, the engine converts it into the
/// emulator's own point type behind the seam.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridPoint {
    pub line: i32,
    pub column: usize,
}

/// Which half of a cell a pointer position lands in: terminals anchor a
/// selection's ends to cell edges, not cell centers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellEdge {
    Left,
    Right,
}

/// What a pointer gesture selects: plain drag, double-click word, or
/// triple-click line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectMode {
    Char,
    Word,
    Line,
}

/// One highlighted run of cells, in viewport coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectSpan {
    pub row: usize,
    pub start: usize,
    pub len: usize,
}

/// A frame's worth of terminal state, plain data for the renderer.
/// Input-mode flags (alt screen, bracketed paste) are read on demand
/// through their accessors: no consumer needs them per frame.
pub struct Snapshot {
    pub rows: Vec<Row>,
    pub cursor: Option<CursorSpot>,
    /// how far the viewport is scrolled back into history (0 = live)
    pub display_offset: usize,
    /// the active selection, projected into viewport row spans
    pub selection: Vec<SelectSpan>,
}

fn cell_edge(edge: CellEdge) -> Side {
    match edge {
        CellEdge::Left => Side::Left,
        CellEdge::Right => Side::Right,
    }
}

fn select_type(mode: SelectMode) -> SelectionType {
    match mode {
        SelectMode::Char => SelectionType::Simple,
        SelectMode::Word => SelectionType::Semantic,
        SelectMode::Line => SelectionType::Lines,
    }
}

/// Project a selection range (buffer coordinates) into viewport row spans:
/// middle rows run the full width, the anchor rows clamp to the range's
/// columns. A line selection fills every row it touches.
fn selection_spans(
    range: &SelectionRange,
    mode: SelectMode,
    display_offset: usize,
    screen_lines: usize,
    columns: usize,
) -> Vec<SelectSpan> {
    let last_col = columns.saturating_sub(1);
    let mut spans = Vec::new();
    for row in 0..screen_lines {
        let line = row as i32 - display_offset as i32;
        if line < range.start.line.0 || line > range.end.line.0 {
            continue;
        }
        let (start_col, end_col) = match mode {
            SelectMode::Line => (0, last_col),
            _ => {
                let start_col = if line == range.start.line.0 { range.start.column.0 } else { 0 };
                let end_col = if line == range.end.line.0 { range.end.column.0 } else { last_col };
                (start_col, end_col)
            }
        };
        let start_col = start_col.min(last_col);
        let end_col = end_col.min(last_col);
        if end_col >= start_col {
            spans.push(SelectSpan { row, start: start_col, len: end_col - start_col + 1 });
        }
    }
    spans
}

/// One scrollback-search match, in buffer coordinates: line 0 is the top
/// of the live screen, negative lines are history. Columns are inclusive
/// on both ends; a match can wrap rows and the view expands it into
/// per-row spans at paint time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    pub from: GridPoint,
    pub to: GridPoint,
}

/// The cap on reported matches: a pathological buffer full of hits ends
/// the scan and the user narrows the query instead.
const SEARCH_MATCH_CAP: usize = 10_000;

/// Escape a plain-text query into a regex literal: every regex
/// metacharacter becomes a literal. Case handling stays with the search
/// DFA, which is case-insensitive unless the query has uppercase.
fn escape_regex(query: &str) -> String {
    let mut out = String::with_capacity(query.len() + 8);
    for c in query.chars() {
        if matches!(
            c,
            '\\' | '^' | '.' | '$' | '|' | '(' | ')' | '[' | ']' | '{' | '}' | '*' | '+' | '?'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A thread-safe scan handle: a background task runs buffer searches
/// through it without dragging the engine (and its PTY handles) across
/// threads.
pub struct SearchHandle {
    term: Arc<FairMutex<Term<UiProxy>>>,
}

impl SearchHandle {
    /// All matches of the `query` across the whole buffer, oldest
    /// history first. Literal text by default; `regex` treats the query
    /// as a regex instead (ctrl+enter in the bar toggles it). Holds the
    /// term lock for the scan; the pump and the renderer interleave
    /// behind the same fair mutex.
    pub fn search(&self, query: &str, regex: bool) -> SearchScan {
        let term = self.term.lock();
        search_scan(&term, query, regex)
    }
}

/// The scan's outcome: matches plus a bad-pattern flag, so the search
/// bar can name a rejected pattern instead of calling it no matches.
/// Building the flag into the scan also keeps the DFA construction off
/// the render path: the bar reads the stored result, it never rebuilds
/// the pattern per frame.
pub struct SearchScan {
    pub matches: Vec<SearchMatch>,
    /// regex mode rejected the pattern outright
    pub bad_pattern: bool,
}

/// The scan behind `SearchHandle::search`: enumerate alacritty's own
/// matches from the topmost buffer line forward. Two quirks of the
/// emulator's search shape the loop: the window bound must stay below
/// `total_lines - 1` or the scan silently covers nothing, and past the
/// last match it falls back to the window's first match (search wraps in
/// the emulator), so any result before the origin ends the walk.
/// The scan as a bare match list: the tests' view. Production callers
/// go through `search_scan`, whose bad-pattern flag the bar reports.
#[cfg(test)]
fn search_term(term: &Term<UiProxy>, query: &str, regex: bool) -> Vec<SearchMatch> {
    search_scan(term, query, regex).matches
}

/// The scan behind `SearchHandle::search`: enumerate alacritty's own
/// matches from the topmost buffer line forward. Two quirks of the
/// emulator's search shape the loop: the window bound must stay below
/// `total_lines - 1` or the scan silently covers nothing, and past the
/// last match it falls back to the window's first match (search wraps in
/// the emulator), so any result before the origin ends the walk.
fn search_scan(term: &Term<UiProxy>, query: &str, regex: bool) -> SearchScan {
    let mut out = Vec::new();
    if query.is_empty() {
        return SearchScan { matches: out, bad_pattern: false };
    }
    // literal mode escapes the metacharacters; regex mode hands the
    // query over as typed. Both share the emulator's smart case: an
    // uppercase anywhere makes the scan case-sensitive.
    let pattern = if regex { query.to_string() } else { escape_regex(query) };
    let mut regex_engine = match RegexSearch::new(&pattern) {
        Ok(regex_engine) => regex_engine,
        // a rejected pattern is information, not an empty result
        Err(_) => return SearchScan { matches: out, bad_pattern: true },
    };
    let columns = term.columns();
    let bottommost = term.bottommost_line().0;
    let max_lines = term.total_lines().saturating_sub(2);
    if max_lines == 0 {
        return SearchScan { matches: out, bad_pattern: false };
    }
    let mut origin = Point::new(term.topmost_line(), Column(0));
    for _ in 0..SEARCH_MATCH_CAP {
        if origin.line.0 > bottommost {
            break;
        }
        let Some(found) = term.search_next(
            &mut regex_engine,
            origin,
            Direction::Right,
            Side::Left,
            Some(max_lines),
        ) else {
            break;
        };
        // the wrap fallback: a match starting before the origin means
        // the buffer behind us is all that is left
        if (found.start().line, found.start().column) < (origin.line, origin.column) {
            break;
        }
        out.push(SearchMatch {
            from: GridPoint { line: found.start().line.0, column: found.start().column.0 },
            to: GridPoint { line: found.end().line.0, column: found.end().column.0 },
        });
        // continue one cell past the match's last cell
        let (line, col) = (found.end().line.0, found.end().column.0);
        if col + 1 >= columns {
            if line >= bottommost {
                break;
            }
            origin = Point::new(Line(line + 1), Column(0));
        } else {
            origin = Point::new(Line(line), Column(col + 1));
        }
    }
    SearchScan { matches: out, bad_pattern: false }
}

/// Scroll the viewport to an absolute history offset (0 = live), clamped
/// to the buffer's actual history. No-op on the alternate screen, where
/// the program owns the viewport.
fn scroll_display_to(term: &mut Term<UiProxy>, offset: usize) {
    if term.mode().contains(TermMode::ALT_SCREEN) {
        return;
    }
    let max = term.grid().history_size();
    let target = offset.min(max);
    let delta = target as i32 - term.grid().display_offset() as i32;
    if delta != 0 {
        term.scroll_display(Scroll::Delta(delta));
    }
}

/// The PTY master out of alacritty's Pty: the pump owns reading, input
/// writes own the same fd through a clone. The child handle stays inside
/// the Pty (its Drop sends SIGHUP and reaps), which the Engine keeps.
fn pty_take_master(pty: &mut tty::Pty) -> std::fs::File {
    pty.file().try_clone().expect("clone the PTY master")
}

/// The byte pump: the terminal's own read loop, replacing alacritty's
/// EventLoop. Reads the PTY master in blocking mode, taps shell-integration
/// markers from the raw bytes, then hands the same bytes to the vte parser
/// (which updates the grid and fires UiProxy events for queries/titles).
/// On Linux a PTY master read after child exit returns EIO once the
/// kernel buffer drains, which is the drain-on-exit behavior alacritty's
/// loop implemented: final output lands, then the Exit event closes the
/// window.
fn pump_thread(
    mut master: std::fs::File,
    term: Arc<FairMutex<Term<UiProxy>>>,
    proxy: UiProxy,
) {
    // alacritty leaves the master nonblocking for its poller; the pump
    // wants blocking reads: zero idle CPU, the child's exit surfaces as
    // EIO once the buffer drains, and input writes never lose bytes to
    // EAGAIN
    use std::os::fd::AsRawFd;
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    if flags >= 0 {
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) };
    }

    let mut parser = Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
    let mut buf = [0u8; 8192];
    loop {
        match master.read(&mut buf) {
            Ok(0) => {
                // EOF: child gone, buffer drained
                proxy.forward(UiEvent::Exit);
                return;
            },
            Ok(n) => {
                let mut locked = term.lock();
                parser.advance(&mut *locked, &buf[..n]);
                // like alacritty's loop: wake the UI once per chunk, after
                // the grid took the bytes
                drop(locked);
                proxy.forward(UiEvent::Wakeup);
            },
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                // EIO is the normal end: the child exited and the kernel
                // buffer drained. Anything else is a real error, but the
                // terminal is done either way.
                if err.raw_os_error() != Some(libc::EIO) {
                    log::warn!("pty read failed: {err}");
                }
                proxy.forward(UiEvent::Exit);
                return;
            },
        }
    }
}

/// The mouse reporting mode a program asked for (DECSET 1000/1002/1003).
/// While any is active the pointer belongs to the program: wheel and
/// clicks are reported to it instead of scrolling the local scrollback.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MouseTracking {
    /// No reporting: the terminal owns the pointer (selection, wheel
    /// scrolling, URL hover).
    #[default]
    Off,
    /// 1000: clicks only.
    Click,
    /// 1002: clicks and motion while a button is held.
    Drag,
    /// 1003: clicks and every motion.
    Motion,
}

/// The xterm protocol's button codes: three buttons, the wheel as
/// buttons 64/65, motion flagged by adding 32.
pub(crate) const MOUSE_LEFT: u8 = 0;
pub(crate) const MOUSE_MIDDLE: u8 = 1;
pub(crate) const MOUSE_RIGHT: u8 = 2;
pub(crate) const MOUSE_WHEEL_UP: u8 = 64;
pub(crate) const MOUSE_WHEEL_DOWN: u8 = 65;
pub(crate) const MOUSE_MOTION_FLAG: u8 = 32;
/// X10's "no button held" code, used in release events and motion
/// without a button.
pub(crate) const MOUSE_NONE: u8 = 3;

/// Which reporting mode the mode bits ask for.
fn tracking_of(mode: &TermMode) -> MouseTracking {
    if mode.contains(TermMode::MOUSE_MOTION) {
        MouseTracking::Motion
    } else if mode.contains(TermMode::MOUSE_DRAG) {
        MouseTracking::Drag
    } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
        MouseTracking::Click
    } else {
        MouseTracking::Off
    }
}

/// The bytes for one mouse event: SGR 1006 when the program asked for
/// that encoding, X10's three-byte form otherwise. The protocol is
/// 1-based; X10's position bytes top out at 223 or they leave the
/// printable range.
pub(crate) fn encode_mouse(
    sgr: bool,
    button: u8,
    col: usize,
    row: usize,
    release: bool,
) -> Vec<u8> {
    if sgr {
        format!(
            "\x1b[<{};{};{}{}",
            button,
            col + 1,
            row + 1,
            if release { 'm' } else { 'M' }
        )
        .into_bytes()
    } else {
        // X10 releases report "no button" (3); the wheel is press-only
        // and the caller knows that
        let code = if release { MOUSE_NONE } else { button };
        let x = ((col + 1).min(223) as u8) + 32;
        let y = ((row + 1).min(223) as u8) + 32;
        vec![0x1b, b'[', b'M', code + 32, x, y]
    }
}

pub struct Engine {
    term: Arc<FairMutex<Term<UiProxy>>>,
    /// alacritty's Pty handle, kept for its Drop: it sends SIGHUP to the
    /// child and reaps it when the view (and with it the Engine) goes away
    _pty: tty::Pty,
    /// the PTY master: input writes here, the pump thread reads a clone
    master: std::fs::File,
    window_size: WindowSize,
    theme: Theme,
    /// how the current selection started, so the snapshot projects its
    /// spans the way the gesture expects (a line select fills whole rows)
    selection_mode: Option<SelectMode>,
}

impl Engine {
    /// Spawn the shell, wire the PTY reader thread. `window_size` carries
    /// the initial cell geometry (measured from the real font by the view).
    /// `command` is `-e`'s argv, run directly (no shell between), which
    /// wins over the env hook when both are present.
    pub fn new(
        window_size: WindowSize,
        theme: Theme,
        tx: UnboundedSender<UiEvent>,
        command: Option<Vec<String>>,
    ) -> io::Result<Self> {
        // TERM and COLORTERM for the child; alacritty's own helper picks
        // alacritty terminfo when installed, xterm-256color otherwise
        tty::setup_env();

        // `-e` first, then the spike hook: KUMA_TERM_COMMAND replaces
        // the login shell so the headless smoke and visual tests can
        // drive exact content (the hook is env, so it persists into
        // child processes: exactly why `-e` never rides it)
        let argv = command.or_else(|| {
            std::env::var("KUMA_TERM_COMMAND")
                .ok()
                .map(|hook| vec!["/bin/sh".to_string(), "-c".to_string(), hook])
        });
        let options = match argv {
            Some(argv) => tty::Options {
                shell: Some(tty::Shell::new(
                    argv[0].clone(),
                    argv[1..].iter().cloned().collect(),
                )),
                ..Default::default()
            },
            None => tty::Options::default(),
        };
        let mut pty = tty::new(&options, window_size, 0)?;

        let proxy = UiProxy { tx };
        let term = Term::new(
            alacritty_terminal::term::Config {
                scrolling_history: theme.scrollback_lines,
                // kitty parity: the plain prompt cursor blinks; apps
                // that manage the cursor send DECSCUSR and take over
                default_cursor_style: CursorStyle {
                    shape: CursorShape::Block,
                    blinking: true,
                },
                ..Default::default()
            },
            &GridDims::from_window_size(window_size),
            proxy.clone(),
        );
        let term = Arc::new(FairMutex::new(term));

        // The byte pump: read the PTY master, tap shell-integration
        // markers, feed the vte parser, wake the UI. alacritty's own
        // EventLoop is not used: its vte version has no hook for arbitrary
        // OSC sequences, so OSC 133/7 must be tapped from the raw stream
        // anyway, and once the tap exists the rest of the loop is a small
        // read-parse-wake cycle this module can own. The parser lives on
        // the pump thread only, so no lock is held while bytes decode.
        let pump_proxy = proxy.clone();
        let pump_term = term.clone();
        let master = pty_take_master(&mut pty);
        let pump_master = master.try_clone().expect("clone the PTY master for the pump");
        thread::Builder::new()
            .name("kuma-term-pty".into())
            .spawn(move || pump_thread(pump_master, pump_term, pump_proxy))
            .expect("spawn the PTY pump");

        Ok(Self { term, _pty: pty, master, window_size, theme, selection_mode: None })
    }

    /// Swap the chrome colors for a republished shell palette; the next
    /// snapshot picks it up.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// Write bytes to the PTY (user input, query replies).
    pub fn input(&self, bytes: Vec<u8>) {
        if let Err(err) = (&self.master).write_all(&bytes) {
            log::warn!("pty write failed: {err}");
        }
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
        use std::os::fd::AsRawFd;
        let winsize = libc::winsize {
            ws_row: lines,
            ws_col: cols,
            ws_xpixel: self.window_size.cell_width as u16 * cols,
            ws_ypixel: self.window_size.cell_height as u16 * lines,
        };
        let ok = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &winsize) };
        if ok != 0 {
            log::warn!("TIOCSWINSZ failed: {}", std::io::Error::last_os_error());
        }
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

    /// How far the viewport is scrolled back into history (0 = live).
    pub fn display_offset(&self) -> usize {
        self.term.lock().grid().display_offset()
    }

    /// A handle for background search scans: shares the term behind its
    /// fair mutex without moving the engine across threads.
    pub fn search_handle(&self) -> SearchHandle {
        SearchHandle { term: self.term.clone() }
    }

    /// Scroll the viewport to an absolute history offset (0 = live),
    /// clamped to the buffer's actual history. No-op on the alternate
    /// screen.
    pub fn scroll_to_offset(&self, offset: usize) {
        let mut term = self.term.lock();
        scroll_display_to(&mut term, offset);
    }

    /// Start a selection: a pointer press at `point` anchors one end.
    pub fn begin_selection(&mut self, point: GridPoint, edge: CellEdge, mode: SelectMode) {
        let mut term = self.term.lock();
        term.selection = Some(Selection::new(
            select_type(mode),
            Point::new(Line(point.line), Column(point.column)),
            cell_edge(edge),
        ));
        self.selection_mode = Some(mode);
    }

    /// Drag the selection's moving end.
    pub fn update_selection(&mut self, point: GridPoint, edge: CellEdge) {
        let mut term = self.term.lock();
        let point = Point::new(Line(point.line), Column(point.column));
        match &mut term.selection {
            Some(selection) => selection.update(point, cell_edge(edge)),
            None => {
                // the emulator cleared under us (scroll or input); start over
                term.selection =
                    Some(Selection::new(SelectionType::Simple, point, cell_edge(edge)));
            }
        }
    }

    /// A pointer release: drop selections that never left the anchor.
    pub fn finish_selection(&mut self) {
        let mut term = self.term.lock();
        if term.selection.as_ref().is_some_and(|s| s.is_empty()) {
            term.selection = None;
            self.selection_mode = None;
        }
    }

    /// Collapse any active selection (a keystroke lands).
    pub fn clear_selection(&mut self) {
        let mut term = self.term.lock();
        term.selection = None;
        self.selection_mode = None;
    }

    /// The selected text, if any, in reading order with line breaks.
    pub fn selection_text(&self) -> Option<String> {
        self.term.lock().selection_to_string()
    }

    /// Resolve a raw palette index (0..269) for OSC 4 replies.
    pub fn resolve_index(&self, index: usize) -> Rgb8 {
        let term = self.term.lock();
        palette::resolve_index(index, term.colors(), &self.theme)
    }

    /// The shell's current input mode flags the encoder needs.
    pub fn app_cursor(&self) -> bool {
        self.term.lock().mode().contains(TermMode::APP_CURSOR)
    }

    /// Whether the alternate screen buffer is active: full-screen
    /// programs own the viewport, so the wheel becomes arrows.
    pub fn alt_screen(&self) -> bool {
        self.term.lock().mode().contains(TermMode::ALT_SCREEN)
    }

    /// Whether the program wants pastes wrapped in the bracket markers.
    pub fn bracketed_paste(&self) -> bool {
        self.term
            .lock()
            .mode()
            .contains(TermMode::BRACKETED_PASTE)
    }

    /// The program's mouse reporting mode, if it asked for the pointer.
    pub fn mouse_tracking(&self) -> MouseTracking {
        tracking_of(self.term.lock().mode())
    }

    /// Hand the program one mouse event: SGR 1006 when it asked for
    /// that encoding, X10 bytes otherwise. Wheel events are presses
    /// only; clicks get a press from the down handler and a release
    /// from the up handler.
    pub fn report_mouse(&self, button: u8, col: usize, row: usize, release: bool) {
        let sgr = self.term.lock().mode().contains(TermMode::SGR_MOUSE);
        self.input(encode_mouse(sgr, button, col, row, release));
    }

    /// One visible row's cells, the same decode a snapshot row gets,
    /// without materializing the rest of the frame.
    pub fn row_cells(&self, row: usize) -> Option<Vec<RenderCell>> {
        let term = self.term.lock();
        let content = term.renderable_content();
        let mut rows: Vec<Row> = (0..term.screen_lines())
            .map(|_| Row { cells: Vec::new() })
            .collect();
        fill_rows(&self.theme, content, &mut rows, Some(row));
        rows.into_iter().nth(row).map(|row| row.cells)
    }

    /// Take a snapshot of the visible grid for the renderer.
    /// A frame's worth of terminal state, plain data for the renderer.
    /// `cursor_visible` says whether a blinking style is in its lit
    /// phase right now (the view folds in focus and typing); the block
    /// bakes its colors into the cell only when it would paint.
    pub fn snapshot(&self, cursor_visible: bool) -> Snapshot {
        let term = self.term.lock();
        let content = term.renderable_content();
        let offset = content.display_offset;
        let screen_lines = term.screen_lines();
        let columns = term.columns();

        let mut rows: Vec<Row> = Vec::with_capacity(screen_lines);
        for _ in 0..screen_lines {
            rows.push(Row { cells: Vec::with_capacity(columns) });
        }

        // the cursor: decided up front (before the cell walk moves
        // pieces of the content), baked after it. The block restyles the
        // cell under it (cursor color behind, the theme's cursor text on
        // the glyph); the bar, underline, and hollow shapes paint as
        // overlays in the view so the cell's own colors survive. The
        // spot carries the shape and blink flag either way, so the view
        // can paint the overlays and gate them on the blink phase.
        let cursor = if offset == 0 {
            let spot = cursor_spot(&term, &content);
            if let Some(spot) = &spot {
                if cursor_visible && spot.shape == CursorShape::Block {
                    if spot.row < rows.len() && spot.col < rows[spot.row].cells.len() {
                        let cell = &mut rows[spot.row].cells[spot.col];
                        // cursor color behind, the theme's cursor text
                        // color for the glyph, regardless of what the
                        // cell wore
                        cell.bg = self.theme.cursor;
                        cell.fg = self.theme.cursor_text;
                    }
                }
            }
            spot
        } else {
            None
        };

        fill_rows(&self.theme, content, &mut rows, None);
        // the block's bake lands on the filled cells
        if let Some(spot) = &cursor {
            if cursor_visible && spot.shape == CursorShape::Block {
                let cell = &mut rows[spot.row].cells[spot.col];
                // cursor color behind, the theme's cursor text color
                // for the glyph, regardless of what the cell wore
                cell.bg = self.theme.cursor;
                cell.fg = self.theme.cursor_text;
            }
        }

        // the active selection, as viewport row spans for the renderer
        let selection = match (&term.selection, self.selection_mode) {
            (Some(sel), Some(mode)) => sel
                .to_range(&*term)
                .map(|range| selection_spans(&range, mode, offset, screen_lines, columns))
                .unwrap_or_default(),
            (Some(sel), None) => sel
                .to_range(&*term)
                .map(|range| {
                    selection_spans(&range, SelectMode::Char, offset, screen_lines, columns)
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };

        Snapshot {
            rows,
            cursor,
            display_offset: offset,
            selection,
        }
    }
}

/// The visible-grid cell walk behind `snapshot` and `row_cells`: decodes
/// each grid cell into a viewport `RenderCell` (INVERSE swaps, HIDDEN
/// blanks, DIM dims, wide-char spacers as blank two-wide slots).
/// `only_row` decodes just that viewport row's cells and leaves the other
/// rows empty: the URL hover reads one row per mouse move and has no
/// business materializing the whole frame.
fn fill_rows(
    theme: &Theme,
    content: RenderableContent,
    rows: &mut [Row],
    only_row: Option<usize>,
) {
    let offset = content.display_offset;
    for indexed in content.display_iter {
        let viewport_row = indexed.point.line.0 + offset as i32;
        if viewport_row < 0 || viewport_row as usize >= rows.len() {
            continue;
        }
        if let Some(only_row) = only_row
            && viewport_row != only_row as i32
        {
            continue;
        }
        let row = &mut rows[viewport_row as usize];
        let cell = indexed.cell;
        let flags = cell.flags;

        if flags.contains(Flags::WIDE_CHAR_SPACER) {
            row.cells.push(RenderCell {
                c: ' ',
                fg: theme.foreground,
                bg: theme.background,
                bold: false,
                italic: false,
                underline: false,
                strikeout: false,
                spacer: true,
            });
            continue;
        }

        let mut fg = palette::resolve(cell.fg, content.colors, theme);
        let mut bg = palette::resolve(cell.bg, content.colors, theme);
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
}

/// The frame's cursor spot: position, the DECSCUSR shape in effect,
/// and the style's blink flag. None when the cursor is hidden or off
/// the live screen (a vi-mode cursor can sit in history).
fn cursor_spot(term: &Term<UiProxy>, content: &RenderableContent) -> Option<CursorSpot> {
    if content.cursor.shape == CursorShape::Hidden {
        return None;
    }
    let point = content.cursor.point;
    let row = point.line.0 as usize;
    let col = point.column.0;
    if row >= term.screen_lines() || col >= term.columns() {
        return None;
    }
    Some(CursorSpot {
        row,
        col,
        shape: content.cursor.shape,
        blinking: term.cursor_style().blinking,
    })
}

/// The block cursor bakes its colors into the cell under it; the bar,
/// underline, and hollow shapes paint as overlays, leaving the cell's
/// own colors alone. `visible` folds in focus and the blink phase.
#[cfg(test)]
fn bakes_block(shape: CursorShape, visible: bool) -> bool {
    shape == CursorShape::Block && visible
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::vte::ansi::Processor;
    use futures::StreamExt;

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
    fn decscusr_rides_the_snapshot() {
        let mut term = test_term(10, 3);
        feed(&mut term, b"\x1b[?25h"); // DECTCEM: the cursor shows
        feed(&mut term, b"\x1b[2 q"); // DECSCUSR 2: steady block
        let spot = cursor_spot(&term, &term.renderable_content()).unwrap();
        assert_eq!(spot.shape, CursorShape::Block);
        assert!(!spot.blinking);
        // the block's restyle bakes into the cell only when lit
        assert!(bakes_block(spot.shape, true));
        assert!(!bakes_block(spot.shape, false));
    }

    #[test]
    fn decscusr_underline_beam_and_blink() {
        let mut term = test_term(10, 3);
        feed(&mut term, b"\x1b[?25h");
        feed(&mut term, b"\x1b[4 q"); // steady underline
        let spot = cursor_spot(&term, &term.renderable_content()).unwrap();
        assert_eq!(spot.shape, CursorShape::Underline);
        assert!(!spot.blinking);
        // overlay shapes leave the cell's own colors alone
        assert!(!bakes_block(spot.shape, true));

        feed(&mut term, b"\x1b[5 q"); // blinking beam
        let spot = cursor_spot(&term, &term.renderable_content()).unwrap();
        assert_eq!(spot.shape, CursorShape::Beam);
        assert!(spot.blinking);

        // DECSCUSR 0 resets to the term's default style
        feed(&mut term, b"\x1b[0 q");
        assert_eq!(
            cursor_spot(&term, &term.renderable_content()).unwrap().shape,
            CursorShape::Block
        );
    }

    #[test]
    fn decscusr_hidden_cursor_has_no_spot() {
        let mut term = test_term(10, 3);
        feed(&mut term, b"\x1b[?25h");
        feed(&mut term, b"\x1b[2 q"); // steady block
        assert!(cursor_spot(&term, &term.renderable_content()).is_some());
        feed(&mut term, b"\x1b[?25l"); // DECTCEM: hide the cursor
        assert!(cursor_spot(&term, &term.renderable_content()).is_none());
    }

    #[test]
    fn row_walk_can_target_a_single_row() {
        let mut term = test_term(10, 3);
        feed(&mut term, b"\x1b[2;1Hrow one");
        let theme = crate::theme::Theme::builtin();

        // only the targeted viewport row decodes; the others stay empty
        let content = term.renderable_content();
        let mut rows: Vec<Row> = (0..3).map(|_| Row { cells: Vec::new() }).collect();
        fill_rows(&theme, content, &mut rows, Some(1));
        assert!(rows[0].cells.is_empty());
        assert!(rows[1].cells.iter().any(|c| c.c == 'r'));
        assert!(rows[2].cells.is_empty());

        // the unfiltered walk (snapshot's path) fills every row
        let content = term.renderable_content();
        let mut rows: Vec<Row> = (0..3).map(|_| Row { cells: Vec::new() }).collect();
        fill_rows(&theme, content, &mut rows, None);
        assert!(!rows[0].cells.iter().any(|c| c.c == 'r'));
        assert!(rows[1].cells.iter().any(|c| c.c == 'r'));

        // a row past the screen is None territory for row_cells' caller:
        // the walk never indexes it
        let content = term.renderable_content();
        let mut rows: Vec<Row> = (0..3).map(|_| Row { cells: Vec::new() }).collect();
        fill_rows(&theme, content, &mut rows, Some(5));
        assert!(rows.iter().all(|row| row.cells.is_empty()));
    }

    #[test]
    fn scrollback_size_rides_the_term_config() {
        // the default grid keeps 10,000 history lines; a config can keep
        // fewer (or none), and the cap is honored exactly
        let (tx, _rx) = futures::channel::mpsc::unbounded::<UiEvent>();
        let mut term = Term::new(
            alacritty_terminal::term::Config { scrolling_history: 5, ..Default::default() },
            &GridDims { columns: 10, screen_lines: 3 },
            UiProxy { tx },
        );
        for _ in 0..50 {
            feed(&mut term, b"line\r\n");
        }
        assert_eq!(term.grid().history_size(), 5);
    }

    #[test]
    fn search_reaches_the_configured_scrollback() {
        // a needle 30 lines above the live screen, on a 3-line viewport:
        // finding it means the history the config asked for is really
        // there and the scan spans it
        let (tx, _rx) = futures::channel::mpsc::unbounded::<UiEvent>();
        let mut term = Term::new(
            alacritty_terminal::term::Config { scrolling_history: 100, ..Default::default() },
            &GridDims { columns: 20, screen_lines: 3 },
            UiProxy { tx },
        );
        feed(&mut term, b"needle\r\n");
        for _ in 0..30 {
            feed(&mut term, b"pad pad pad\r\n");
        }
        let matches = search_term(&term, "needle", false);
        assert_eq!(matches.len(), 1);
        // buffer coordinates: the live screen is rows 0..3, history is
        // negative, and the needle scrolled off 30 lines ago
        assert!(matches[0].from.line < 0);
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
    fn mouse_modes_cross_the_seam() {
        let mut term = test_term(10, 3);
        assert_eq!(tracking_of(term.mode()), MouseTracking::Off);
        feed(&mut term, b"\x1b[?1000h");
        assert_eq!(tracking_of(term.mode()), MouseTracking::Click);
        feed(&mut term, b"\x1b[?1002h");
        assert_eq!(tracking_of(term.mode()), MouseTracking::Drag);
        feed(&mut term, b"\x1b[?1003h");
        assert_eq!(tracking_of(term.mode()), MouseTracking::Motion);
        // SGR encoding rides 1006
        feed(&mut term, b"\x1b[?1006h");
        assert!(term.mode().contains(TermMode::SGR_MOUSE));
        // and every reset returns the pointer to the terminal
        feed(&mut term, b"\x1b[?1003l\x1b[?1006l");
        assert_eq!(tracking_of(term.mode()), MouseTracking::Off);
        assert!(!term.mode().contains(TermMode::SGR_MOUSE));
    }

    #[test]
    fn sgr_mouse_encoding_is_one_based_and_marked_releases() {
        assert_eq!(encode_mouse(true, MOUSE_LEFT, 0, 0, false), b"\x1b[<0;1;1M".to_vec());
        assert_eq!(encode_mouse(true, MOUSE_RIGHT, 3, 7, false), b"\x1b[<2;4;8M".to_vec());
        assert_eq!(encode_mouse(true, MOUSE_LEFT, 3, 7, true), b"\x1b[<0;4;8m".to_vec());
        // the wheel is press-only and keeps its high button numbers
        assert_eq!(
            encode_mouse(true, MOUSE_WHEEL_UP, 9, 2, false),
            b"\x1b[<64;10;3M".to_vec()
        );
        assert_eq!(
            encode_mouse(true, MOUSE_WHEEL_DOWN, 9, 2, false),
            b"\x1b[<65;10;3M".to_vec()
        );
    }

    #[test]
    fn x10_mouse_encoding_caps_and_marks_releases() {
        // X10 packs each field into one byte, offset by 32
        assert_eq!(
            encode_mouse(false, MOUSE_LEFT, 0, 0, false),
            vec![0x1b, b'[', b'M', 32, 33, 33]
        );
        // a release reports "no button" (3)
        assert_eq!(
            encode_mouse(false, MOUSE_LEFT, 5, 5, true),
            vec![0x1b, b'[', b'M', 35, 38, 38]
        );
        // positions cap at 223 so the bytes stay printable
        assert_eq!(
            encode_mouse(false, MOUSE_RIGHT, 500, 900, false),
            vec![0x1b, b'[', b'M', 34, 223 + 32, 223 + 32]
        );
    }

    #[test]
    fn truecolor_and_indexed_cells_resolve() {
        let mut term = test_term(10, 3);
        feed(&mut term, b"\x1b[38;5;196mX\x1b[0m");
        let content = term.renderable_content();
        for indexed in content.display_iter {
            if indexed.cell.c == 'X' {
                let theme = crate::theme::Theme::builtin();
                let fg = palette::resolve(indexed.cell.fg, content.colors, &theme);
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

    /// Mirror of the engine's gesture: anchor, drag, park on the term.
    fn drag_select(
        term: &mut Term<UiProxy>,
        from: (i32, usize),
        to: (i32, usize),
    ) {
        let mut selection = Selection::new(
            SelectionType::Simple,
            Point::new(Line(from.0), Column(from.1)),
            Side::Left,
        );
        selection.update(Point::new(Line(to.0), Column(to.1)), Side::Right);
        term.selection = Some(selection);
    }

    #[test]
    fn selection_text_crosses_lines() {
        let mut term = test_term(10, 3);
        feed(&mut term, b"alpha\r\nbeta\r\ngamma");
        // right edge of column 2 includes that column: "bet"
        drag_select(&mut term, (0, 0), (1, 2));
        assert_eq!(term.selection_to_string().as_deref(), Some("alpha\nbet"));
    }

    #[test]
    fn selection_text_reaches_into_history() {
        let mut term = test_term(10, 3);
        for _ in 0..10 {
            feed(&mut term, b"line\r\n");
        }
        term.scroll_display(Scroll::Delta(10));
        // buffer line -2 is two lines above the live screen top
        drag_select(&mut term, (-2, 0), (-1, 4));
        assert_eq!(term.selection_to_string().as_deref(), Some("line\nline"));
    }

    #[test]
    fn selection_spans_fill_middle_rows() {
        let range = SelectionRange::new(
            Point::new(Line(0), Column(2)),
            Point::new(Line(2), Column(5)),
            false,
        );
        let spans = selection_spans(&range, SelectMode::Char, 0, 3, 10);
        assert_eq!(
            spans,
            vec![
                SelectSpan { row: 0, start: 2, len: 8 },
                SelectSpan { row: 1, start: 0, len: 10 },
                SelectSpan { row: 2, start: 0, len: 6 },
            ]
        );
    }

    #[test]
    fn selection_spans_track_display_offset() {
        let range = SelectionRange::new(
            Point::new(Line(0), Column(2)),
            Point::new(Line(2), Column(5)),
            false,
        );
        // scrolled back one line: buffer line 0 sits at viewport row 1, and
        // the range's last line (buffer line 2) is off the viewport, so its
        // row renders as a middle row at full width
        let spans = selection_spans(&range, SelectMode::Char, 1, 3, 10);
        assert_eq!(
            spans,
            vec![
                SelectSpan { row: 1, start: 2, len: 8 },
                SelectSpan { row: 2, start: 0, len: 10 },
            ]
        );
    }

    #[test]
    fn line_selection_fills_whole_rows() {
        let range = SelectionRange::new(
            Point::new(Line(0), Column(4)),
            Point::new(Line(1), Column(7)),
            false,
        );
        let spans = selection_spans(&range, SelectMode::Line, 0, 3, 10);
        assert_eq!(
            spans,
            vec![
                SelectSpan { row: 0, start: 0, len: 10 },
                SelectSpan { row: 1, start: 0, len: 10 },
            ]
        );
    }

    #[test]
    fn single_cell_selection_is_one_span() {
        let range = SelectionRange::new(
            Point::new(Line(0), Column(2)),
            Point::new(Line(0), Column(2)),
            false,
        );
        let spans = selection_spans(&range, SelectMode::Char, 0, 2, 10);
        assert_eq!(spans, vec![SelectSpan { row: 0, start: 2, len: 1 }]);
    }

    #[test]
    fn escape_regex_makes_metacharacters_literal() {
        assert_eq!(
            escape_regex("a.b*c?d[e]f(g)h|i^j$k\\l{m}"),
            "a\\.b\\*c\\?d\\[e\\]f\\(g\\)h\\|i\\^j\\$k\\\\l\\{m\\}"
        );
        assert_eq!(escape_regex("plain text"), "plain text");
        assert_eq!(escape_regex(""), "");
    }

    #[test]
    fn search_finds_matches_across_history_oldest_first() {
        let mut term = test_term(10, 3);
        for _ in 0..6 {
            feed(&mut term, b"find me\r\n");
        }
        feed(&mut term, b"nothing");
        let hits = search_term(&term, "find", false);
        assert_eq!(hits.len(), 6);
        // oldest history first, and never out of buffer order
        for pair in hits.windows(2) {
            assert!(
                (pair[0].from.line, pair[0].from.column)
                    < (pair[1].from.line, pair[1].from.column)
            );
        }
        // "find" occupies columns 0..=3 of its row
        assert!(hits.iter().all(|m| m.from.column == 0 && m.to.column == 3));
    }

    #[test]
    fn search_is_literal_text() {
        let mut term = test_term(10, 3);
        feed(&mut term, b"a.b c*x");
        // metacharacters in the query match themselves, not patterns
        assert_eq!(search_term(&term, "a.b", false).len(), 1);
        assert_eq!(search_term(&term, "aXb", false).len(), 0);
        assert_eq!(search_term(&term, "c*x", false).len(), 1);
        assert_eq!(search_term(&term, "cXx", false).len(), 0);
        assert!(search_term(&term, "", false).is_empty());
    }

    #[test]
    fn search_is_smart_case() {
        let mut term = test_term(10, 3);
        feed(&mut term, b"Kuma KUMA kuma");
        // a lowercase query ignores case, an uppercase one demands it:
        // the DFA builder's own rule, kitty's too
        assert_eq!(search_term(&term, "kuma", false).len(), 3);
        assert_eq!(search_term(&term, "Kuma", false).len(), 1);
    }

    #[test]
    fn regex_mode_takes_the_query_as_typed() {
        let mut term = test_term(10, 3);
        feed(&mut term, b"a.b cxx axb c*x");
        // literal mode: the dot matches a dot
        assert_eq!(search_term(&term, "a.b", false).len(), 1);
        // regex mode: the dot matches any character
        assert_eq!(search_term(&term, "a.b", true).len(), 2);
        // alternation and classes work
        assert_eq!(search_term(&term, "c[xy]+x", true).len(), 1);
        assert_eq!(search_term(&term, "cxx|c\\*x", true).len(), 2);
    }

    #[test]
    fn regex_mode_keeps_smart_case_and_rejects_bad_patterns() {
        let mut term = test_term(10, 3);
        feed(&mut term, b"Kuma kuma");
        assert_eq!(search_term(&term, "k.ma", true).len(), 2);
        assert_eq!(search_term(&term, "K.ma", true).len(), 1);
        // an unbalanced pattern builds no DFA: no matches, no panic
        assert!(search_term(&term, "(unclosed", true).is_empty());
        // an empty match pattern also builds, but the scan's dedup keeps
        // the walk finite; it just finds nothing useful to report
        assert!(search_term(&term, "x*", true).is_empty());
    }

    #[test]
    fn search_matches_wrap_across_rows() {
        let mut term = test_term(10, 3);
        // 12 characters on a 10-column grid wrap onto the next row
        feed(&mut term, b"abcdefghijkl");
        let hits = search_term(&term, "hijk", false);
        assert_eq!(hits.len(), 1);
        // h, i, j fill row 0's last three columns; k opens row 1
        assert_eq!(hits[0].from, GridPoint { line: 0, column: 7 });
        assert_eq!(hits[0].to, GridPoint { line: 1, column: 0 });
    }

    #[test]
    fn scroll_display_to_clamps_and_sets() {
        let mut term = test_term(10, 3);
        for _ in 0..10 {
            feed(&mut term, b"line\r\n");
        }
        let max = term.grid().history_size();
        assert!(max >= 8);
        scroll_display_to(&mut term, usize::MAX);
        assert_eq!(term.grid().display_offset(), max);
        scroll_display_to(&mut term, 2);
        assert_eq!(term.grid().display_offset(), 2);
    }

    /// A real PTY end to end: a child writes bytes, the pump parses them
    /// into the grid, the UI channel carries Wakeup/Exit, and a resize
    /// lands on the live pty. Skipped when the sandbox has no /bin/sh
    /// (the container has it).
    #[test]
    fn pump_moves_pty_bytes_through_a_live_engine() {
        if std::env::var("KUMA_TERM_SKIP_PTY_TEST").is_ok() {
            return;
        }
        let window_size = WindowSize {
            num_cols: 20,
            num_lines: 5,
            cell_width: 10,
            cell_height: 20,
        };
        // the child prints visible text then exits: exercises the pump,
        // the parser, and the EIO exit path in one go. Env mutation is
        // safe here: the PTY_ENV_LOCK keeps the other engine-spawning
        // test from reading a clobbered fixture.
        let _env = crate::PTY_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe { std::env::set_var("KUMA_TERM_COMMAND", "echo kuma") };
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<UiEvent>();
        let mut engine =
            Engine::new(window_size, crate::theme::Theme::builtin(), tx, None).expect("engine with a live pty");

        // pump the event channel until the child exits, with a wall clock
        // bound so a broken pump fails instead of hanging the suite
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut saw_text = false;
        let mut saw_exit = false;
        while std::time::Instant::now() < deadline && !saw_exit {
            let event = futures::executor::block_on(rx.next());
            match event {
                Some(UiEvent::Wakeup) => {
                    if engine.snapshot(true).rows.iter().any(|r| {
                        r.cells.iter().any(|c| c.c == 'k')
                            && r.cells.iter().any(|c| c.c == 'a')
                            && r.cells.iter().any(|c| c.c == 'm')
                            && r.cells.iter().any(|c| c.c == 'u')
                    }) {
                        saw_text = true;
                    }
                },
                Some(UiEvent::Exit) => saw_exit = true,
                Some(_) => {},
                None => break,
            }
        }
        unsafe { std::env::remove_var("KUMA_TERM_COMMAND") };

        assert!(saw_text, "grid never received the visible text");
        assert!(saw_exit, "pump never reported the child exit");
        engine.resize(19, 4);
    }

    #[test]
    fn engine_command_beats_the_env_hook() {
        if std::env::var("KUMA_TERM_SKIP_PTY_TEST").is_ok() {
            return;
        }
        let window_size = WindowSize {
            num_cols: 20,
            num_lines: 5,
            cell_width: 10,
            cell_height: 20,
        };
        // `-e` is a stateless argv contract: even with the env hook
        // set (as a kuma-launch'd kuma-term inside a kuma-launch'd
        // kuma-term would see), the explicit command runs and the
        // hook's script does not
        let _env = crate::PTY_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe { std::env::set_var("KUMA_TERM_COMMAND", "echo envhook-mistake") };
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<UiEvent>();
        let mut engine = Engine::new(
            window_size,
            crate::theme::Theme::builtin(),
            tx,
            Some(vec!["echo".to_string(), "direct-wins".to_string()]),
        )
        .expect("engine with a live pty");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut saw_direct = false;
        let mut saw_hook = false;
        let mut saw_exit = false;
        while std::time::Instant::now() < deadline && !saw_exit {
            let event = futures::executor::block_on(rx.next());
            match event {
                Some(UiEvent::Wakeup) => {
                    for row in engine.snapshot(true).rows {
                        let line: String = row.cells.iter().map(|c| c.c).collect();
                        saw_direct |= line.contains("direct-wins");
                        saw_hook |= line.contains("envhook");
                    }
                },
                Some(UiEvent::Exit) => saw_exit = true,
                Some(_) => {},
                None => break,
            }
        }
        unsafe { std::env::remove_var("KUMA_TERM_COMMAND") };

        assert!(saw_direct, "the -e command never reached the grid");
        assert!(!saw_hook, "the env hook leaked past an explicit -e");
        assert!(saw_exit, "pump never reported the child exit");
        engine.resize(19, 4);
    }
}
