//! The pty is the terminal's attack surface: raw bytes from any remote
//! (an ssh session, a curl of a crafted file) reach the parser unmolested.
//! This target feeds arbitrary bytes through the same seam kuma-term's
//! pump rides (`vte::ansi::Processor` advancing an alacritty `Term`,
//! split mid-stream like the pump's 8 KiB reads), then walks the reads
//! the render path makes. A panic here is a remote-triggered crash; a
//! hang or unbounded growth (audit T1's class) shows as a timeout or an
//! rss abort.

#![no_main]

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use libfuzzer_sys::fuzz_target;

/// The fuzz viewport: 80x24 plus the scrollback the real config allows.
struct Dims;

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        24 + 10_000
    }
    fn screen_lines(&self) -> usize {
        24
    }
    fn columns(&self) -> usize {
        80
    }
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let mut term = Term::new(Config::default(), &Dims, VoidListener);
    let mut parser = Processor::<StdSyncHandler>::new();

    // feed split mid-sequence: the pump's reads land on 8 KiB
    // boundaries, and escapes that straddle a read boundary are the
    // state machine's interesting case
    let split = data.len() / 3 + 1;
    parser.advance(&mut term, &data[..split]);
    parser.advance(&mut term, &data[split..]);

    // the reads the render path makes, done cheaply: the visible grid
    // and the cursor
    let content = term.renderable_content();
    for cell in content.display_iter {
        let _ = cell.bg;
    }
    let _ = term.grid().cursor.point;
});
