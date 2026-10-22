//! The kitty keyboard protocol: negotiation over the output stream and
//! progressive-enhancement flags on the input encoder.
//!
//! A program turns the protocol on by writing CSI-u sequences on its
//! own output (the same bytes the vte parser consumes, so the tap here
//! only observes): `CSI ? u` asks what the terminal has active,
//! `CSI = flags ; mode u` sets the flags (mode 1 replace, 2 union,
//! 3 difference), `CSI > flags u` pushes a mode onto the stack,
//! `CSI < n u` pops n modes. The tap answers the query on the PTY and
//! keeps the flag stack shared with the encoder, which shifts into the
//! CSI-u encoding while the program asked for disambiguation:
//! shift+enter, ctrl+tab, and chords that would otherwise collapse
//! into bare C0 bytes arrive as distinct escapes.
//!
//! Only the disambiguate flag changes the encoding today. The other
//! enhancement bits (event types, alternate keys, all keys as escape
//! codes, associated text) are stored and reported honestly, but the
//! encoder keeps their legacy shapes: gpui's keystroke carries no
//! release events and no key location, so event reporting cannot be
//! honest and the numpad keys arrive under the names of their twin
//! keys anyway.

use std::sync::Mutex;

/// Flag 1: disambiguate escape codes, the CSI-u encoding for chords.
pub const DISAMBIGUATE: u16 = 0b00001;

/// How deep the push/pop stack may grow before the bottom falls off:
/// the spec wants unbounded, every real program pushes a mode or two,
/// and a leak (a program that pushes and dies) would otherwise grow
/// the stack forever.
const STACK_CAP: usize = 32;

/// The shared flag stack. The output tap (pump thread) mutates it as
/// the program's sequences arrive; the input encoder (UI thread) reads
/// the active flags before encoding every keystroke.
#[derive(Default)]
pub struct Modes {
    stack: Mutex<Vec<u16>>,
}

impl Modes {
    /// The flags currently in force: the top of the stack, or none
    /// (legacy encoding) when the stack is empty.
    pub fn active(&self) -> u16 {
        let stack = self.stack.lock().unwrap_or_else(|p| p.into_inner());
        stack.last().copied().unwrap_or(0)
    }

    /// `CSI = flags ; mode u`: set the active flags, replace (1),
    /// union (2), or difference (3). An unknown mode is ignored.
    fn apply_set(&self, flags: u16, mode: u16) {
        let mut stack = self.stack.lock().unwrap_or_else(|p| p.into_inner());
        let current = stack.last().copied().unwrap_or(0);
        let next = match mode {
            1 => flags,
            2 => current | flags,
            3 => current & !flags,
            _ => return,
        };
        if stack.is_empty() {
            stack.push(next);
        } else {
            *stack.last_mut().expect("stack not empty") = next;
        }
    }

    /// `CSI > flags u`: push a mode. Over the cap the bottom of the
    /// stack is dropped, like alacritty's own bound.
    fn apply_push(&self, flags: u16) {
        let mut stack = self.stack.lock().unwrap_or_else(|p| p.into_inner());
        if stack.len() >= STACK_CAP {
            stack.remove(0);
        }
        stack.push(flags);
    }

    /// `CSI < n u`: pop n modes (1 when the program omits the count).
    /// Popping an empty stack is a no-op: the program miscounted.
    fn apply_pop(&self, n: u16) {
        let mut stack = self.stack.lock().unwrap_or_else(|p| p.into_inner());
        let keep = stack.len().saturating_sub(n.max(1) as usize);
        stack.truncate(keep);
    }
}

/// The output tap: a byte-wise state machine over the program's output
/// stream that spots CSI-u sequences and applies them. Sequences split
/// across PTY reads are fine (the state machine persists between
/// chunks); an overlong parameter list is abandoned rather than grown.
pub struct Tap {
    stage: Stage,
    buf: [u8; 64],
    len: usize,
}

impl Default for Tap {
    fn default() -> Self {
        Self::new()
    }
}

impl Tap {
    pub fn new() -> Self {
        Self { stage: Stage::Ground, buf: [0u8; 64], len: 0 }
    }
}

#[derive(Default, PartialEq)]
enum Stage {
    #[default]
    Ground,
    Esc,
    Csi,
}

impl Tap {
    /// Feed one PTY read's bytes. Returns true when the stream asked
    /// the keyboard-mode question (`CSI ? u`) and the caller owes a
    /// reply naming the active flags.
    pub fn feed(&mut self, bytes: &[u8], modes: &Modes) -> bool {
        let mut query = false;
        for &b in bytes {
            match self.stage {
                Stage::Ground => {
                    if b == 0x1b {
                        self.stage = Stage::Esc;
                    }
                },
                Stage::Esc => {
                    if b == b'[' {
                        self.stage = Stage::Csi;
                        self.len = 0;
                    } else if b != 0x1b {
                        self.stage = Stage::Ground;
                    }
                    // ESC ESC stays in Esc: the second escape opens the
                    // sequence the first no longer can
                },
                Stage::Csi => {
                    if b == 0x1b {
                        // a fresh escape inside the sequence: the
                        // program's output interleaved, start over
                        self.stage = Stage::Esc;
                    } else if (0x20..=0x3f).contains(&b) {
                        // parameter and intermediate bytes
                        if self.len < self.buf.len() {
                            self.buf[self.len] = b;
                            self.len += 1;
                        } else {
                            // overlong: abandon the sequence
                            self.stage = Stage::Ground;
                        }
                    } else if (0x40..=0x7e).contains(&b) {
                        // final byte: the sequence is complete
                        query |= self.finalize(b, modes);
                        self.stage = Stage::Ground;
                    } else {
                        // a C0 byte inside the sequence cancels it
                        self.stage = Stage::Ground;
                    }
                },
            }
        }
        query
    }

    fn finalize(&mut self, final_byte: u8, modes: &Modes) -> bool {
        // only CSI u interests the tap; every other escape is the
        // program's ordinary traffic vte already consumed
        if final_byte != b'u' {
            return false;
        }
        let params = std::str::from_utf8(&self.buf[..self.len]).unwrap_or("");
        let numbers: Vec<u16> = params
            .trim_start_matches(|c: char| c == '>' || c == '<' || c == '=')
            .split(';')
            .map(|n| n.parse::<u16>().unwrap_or(0))
            .collect();
        let first = numbers.first().copied().unwrap_or(0);
        let second = numbers.get(1).copied().unwrap_or(0);
        match params.as_bytes().first() {
            Some(b'?') => true,
            Some(b'>') => {
                modes.apply_push(first);
                false
            },
            Some(b'<') => {
                // the spec's default pop count is one
                modes.apply_pop(if numbers.first().is_some() { first } else { 1 });
                false
            },
            Some(b'=') => {
                // the spec's default set mode is replace
                modes.apply_set(first, if numbers.get(1).is_some() { second } else { 1 });
                false
            },
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_query_asks_and_the_flags_change() {
        let modes = Modes::default();
        let mut tap = Tap::default();

        // the query: the caller owes a reply with the current flags
        assert!(tap.feed(b"\x1b[?u", &modes));
        assert_eq!(modes.active(), 0);

        // replace mode: the flags are set
        assert!(!tap.feed(b"\x1b[=1;1u", &modes));
        assert_eq!(modes.active(), 1);

        // union mode: the flag joins what is set
        assert!(!tap.feed(b"\x1b[=8;2u", &modes));
        assert_eq!(modes.active(), 0b1001);

        // difference mode: the flag clears
        assert!(!tap.feed(b"\x1b[=1;3u", &modes));
        assert_eq!(modes.active(), 0b1000);

        // the query again now answers with the active flags
        assert!(tap.feed(b"\x1b[?u", &modes));
        assert_eq!(modes.active(), 0b1000);
    }

    #[test]
    fn push_and_pop_nest() {
        let modes = Modes::default();
        let mut tap = Tap::default();

        assert!(!tap.feed(b"\x1b[=1;1u", &modes));
        assert!(!tap.feed(b"\x1b[>3u", &modes));
        assert_eq!(modes.active(), 3);
        assert!(!tap.feed(b"\x1b[>5u", &modes));
        assert_eq!(modes.active(), 5);
        // pop two: back under the first push
        assert!(!tap.feed(b"\x1b[<2u", &modes));
        assert_eq!(modes.active(), 1);
        // a bare pop count pops one, and popping empty stays empty
        assert!(!tap.feed(b"\x1b[<u", &modes));
        assert_eq!(modes.active(), 0);
        assert!(!tap.feed(b"\x1b[<u", &modes));
        assert_eq!(modes.active(), 0);
    }

    #[test]
    fn sequences_split_across_reads() {
        let modes = Modes::default();
        let mut tap = Tap::default();
        assert!(!tap.feed(b"\x1b[", &modes));
        assert!(!tap.feed(b">1", &modes));
        assert_eq!(tap.feed(b"u", &modes), false);
        assert_eq!(modes.active(), 1);
    }

    #[test]
    fn ordinary_escape_traffic_leaves_the_flags_alone() {
        let modes = Modes::default();
        let mut tap = Tap::default();
        // cursor show, a color set, an OSC title: none of these are
        // CSI u, none move the flags, none owe a reply
        assert!(!tap.feed(b"\x1b[?25h\x1b[38;5;9m\x1b]0;title\x07", &modes));
        assert_eq!(modes.active(), 0);
        // an escape arriving mid-sequence restarts the scan, and the
        // abandoned prefix leaves nothing behind
        assert!(!tap.feed(b"\x1b[=\x1b[>2u", &modes));
        assert_eq!(modes.active(), 2);
    }
}
