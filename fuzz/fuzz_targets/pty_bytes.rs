//! The pty is the terminal's attack surface: raw bytes from any remote
//! (an ssh session, a curl of a crafted file) reach the parser unmolested.
//! This target feeds arbitrary bytes through the seams kuma-term's pump
//! rides: `vte::ansi::Processor` advancing an alacritty `Term`, the
//! shell-integration OSC tap over the same bytes (split mid-stream like
//! the pump's 8 KiB reads, the grid advanced in slices at the tap's
//! event boundaries, the pump's exact arrangement), then walks the reads
//! the render path makes. A panic here is a remote-triggered crash; a
//! hang or unbounded growth (audit T1's class) shows as a timeout or an
//! rss abort.

#![no_main]

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use libfuzzer_sys::fuzz_target;
use term_tap::Tap as OscTap;

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
    let mut osc = OscTap::default();

    // feed split mid-sequence: the pump's reads land on 8 KiB
    // boundaries, and escapes that straddle a read boundary are the
    // state machine's interesting case
    let split = data.len() / 3 + 1;

    // the first chunk rides the pump's slice walk: the grid advances
    // in slices so each OSC event is handled with the cursor exactly
    // at its sequence's boundary
    let events = osc.feed(&data[..split]);
    let mut ate = 0;
    for (at, _event) in events {
        parser.advance(&mut term, &data[..split][ate..at]);
        ate = at;
    }
    parser.advance(&mut term, &data[..split][ate..]);

    // the second chunk rides the plain whole-chunk advance (the pump's
    // no-events path)
    parser.advance(&mut term, &data[split..]);

    // the reads the render path makes, done cheaply: the visible grid
    // and the cursor
    let content = term.renderable_content();
    for cell in content.display_iter {
        let _ = cell.bg;
    }
    let _ = term.grid().cursor.point;
});
