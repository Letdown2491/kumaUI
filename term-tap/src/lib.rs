//! The shell-integration tap: a byte-wise observer for the OSC sequences
//! shell integration and programs emit on the output stream. The grid
//! parser (vte, through alacritty_terminal) consumes the same bytes and
//! owns the screen; this tap only observes, the kitty keyboard tap's
//! arrangement, because vte's `osc_dispatch` has no hook for the
//! sequences a shell integration speaks (OSC 133 prompt markers, OSC 7
//! and OSC 9;9 cwd reports, OSC 9 and OSC 777 notifications, OSC 7501
//! program status).
//!
//! It is a second hand-rolled parser over an attacker-controllable
//! stream (any remote host an ssh session echoes reaches it unmolested),
//! so it is deliberately its own crate with no dependencies: the fuzz
//! target (`fuzz/fuzz_targets/pty_bytes.rs`) drives it standalone, and
//! its bounds are structural. A sequence is abandoned, never buffered
//! past [`OSC_CAP`] bytes; a terminator is required before anything
//! dispatches; every event carries its byte offset in the chunk so the
//! caller can advance the grid parser in slices and read the cursor
//! exactly at the sequence's boundary (the prompt mark's line is only
//! correct that way, since the shell prints the prompt after announcing
//! it).
//!
//! Recognized sequences and their shapes:
//!
//! - `OSC 133 ; A` prompt start, `B` command start, `C` output start,
//!   `D[;code]` command finished (finalterm / kitty / iTerm2's
//!   convention, the one every shell integration uses)
//! - `OSC 7 ; file://host/path` cwd report (the shell's `PWD` as a file
//!   URI, OSC 7's spec shape); a non-file URI is ignored
//! - `OSC 9 ; 9 ; path` cwd report, ConEmu's spelling (kitty 0.51
//!   treats it as OSC 7's equal, so both land on the same event)
//! - `OSC 9 ; message` a desktop notification, iTerm2's spelling
//! - `OSC 777 ; notify ; title ; body` a desktop notification, rxvt's
//!   spelling (the one kitty maps to its notify machinery)
//! - `OSC 7501 ; status[;message]` program status (kitty 0.50's
//!   Protocol), `OSC 7501 ; ?` the query variant
//!
//! Anything else is dropped. Unknown sub-commands of known numbers are
//! dropped: the OSC 777 keys other than `notify` (urllaunch and
//! friends) and the ConEmu OSC 9 variants with a leading number are
//! not ours to guess at.

/// The most bytes of one OSC sequence the tap will buffer. URIs ride
/// OSC 8 (the grid parser's to handle), so the sequences here are
/// shell integration lines, cwd paths, and notification texts; 4 KiB
/// covers every legitimate one with room to spare, and an abandoned
/// sequence is silent (no dispatch) rather than truncated.
pub const OSC_CAP: usize = 4096;

/// One recognized output-sequence event. Offsets ride alongside in
/// [`Tap::feed`], not here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OscEvent {
    /// `OSC 133 ; A`: the prompt is about to print.
    PromptStart,
    /// `OSC 133 ; B`: the command line is about to be echoed.
    CommandStart,
    /// `OSC 133 ; C`: the command's output is about to start.
    OutputStart,
    /// `OSC 133 ; D[;code]`: the command finished; the exit code when
    /// the shell reported one.
    CommandFinished(Option<i32>),
    /// `OSC 7` or `OSC 9 ; 9`: the shell's working directory. The host
    /// is the file URI's authority (`None` for OSC 9;9, which carries
    /// no host); the caller validates it against the local host before
    /// trusting the path.
    Cwd { host: Option<String>, path: String },
    /// `OSC 9 ; message` or `OSC 777 ; notify ; title ; body`.
    Notification { title: Option<String>, body: String },
    /// `OSC 7501 ; status[;message]`: a program reported its status.
    /// Every status value is forwarded as spelled; unknown values are
    /// the wiring's to ignore (the kitty keyboard tap's etiquette).
    ProgramStatus { status: String, message: Option<String> },
    /// `OSC 7501 ; ?`: the program asked what the current status is.
    /// The caller owes a reply on the PTY.
    ProgramStatusQuery,
}

#[derive(Default, PartialEq)]
enum Stage {
    #[default]
    Ground,
    /// ESC seen in the ground state.
    Esc,
    /// OSC body collecting (after `ESC ]`).
    Body,
    /// ESC seen inside an OSC body: either the ST terminator's first
    /// byte or a fresh escape that abandons the sequence.
    BodyEsc,
}

/// The tap: feed it every PTY read exactly as the grid parser sees
/// them (same chunk boundaries are not required; any split works, the
/// state machine persists).
#[derive(Default)]
pub struct Tap {
    stage: Stage,
    buf: Vec<u8>,
    /// set when the buffer blew the cap: keep consuming to the
    /// terminator, then drop the sequence whole
    abandoned: bool,
}

impl Tap {
    /// Feed one chunk; returns each recognized sequence with the byte
    /// offset just past its terminator (relative to this chunk's
    /// start). The common chunk yields an empty vector and allocates
    /// nothing.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<(usize, OscEvent)> {
        let mut events = Vec::new();
        for (at, &b) in bytes.iter().enumerate() {
            match self.stage {
                Stage::Ground => {
                    if b == 0x1b {
                        self.stage = Stage::Esc;
                    }
                },
                Stage::Esc => {
                    if b == b']' {
                        self.stage = Stage::Body;
                        self.buf.clear();
                        self.abandoned = false;
                    } else if b != 0x1b {
                        self.stage = Stage::Ground;
                    }
                    // ESC ESC stays in Esc: the second escape opens the
                    // sequence the first no longer can
                },
                Stage::Body => match b {
                    0x07 => self.dispatch(&bytes[..=at], &mut events),
                    0x1b => self.stage = Stage::BodyEsc,
                    _ => self.push(b),
                },
                Stage::BodyEsc => {
                    // ESC inside the body: ST terminates it, ESC opens
                    // a fresh escape (the sequence is abandoned, and
                    // ESC ] after it starts a new OSC), anything else
                    // is ground noise behind the abandoned sequence
                    match b {
                        b'\\' => self.dispatch(&bytes[..=at], &mut events),
                        0x1b => self.stage = Stage::Esc,
                        b']' => {
                            self.stage = Stage::Body;
                            self.buf.clear();
                            self.abandoned = false;
                        },
                        _ => self.stage = Stage::Ground,
                    }
                },
            }
        }
        events
    }

    fn push(&mut self, b: u8) {
        if self.buf.len() < OSC_CAP {
            self.buf.push(b);
        } else {
            // the cap: mark abandoned, stop buffering, keep scanning
            // for the terminator
            self.abandoned = true;
        }
    }

    /// The sequence is complete (`BEL` or `ESC \` seen): parse and
    /// dispatch, unless the cap abandoned it.
    fn dispatch(&mut self, consumed: &[u8], events: &mut Vec<(usize, OscEvent)>) {
        self.stage = Stage::Ground;
        if self.abandoned {
            self.abandoned = false;
            return;
        }
        let offset = consumed.len();
        if let Some(event) = parse(&self.buf) {
            events.push((offset, event));
        }
        self.buf.clear();
    }
}

/// Parse a completed OSC body: `number[;params...]`. Returns `None`
/// for everything the tap does not speak.
fn parse(body: &[u8]) -> Option<OscEvent> {
    let mut params = body.split(|&b| b == b';');
    let number = params.next()?;
    match number {
        b"133" => {
            let kind = params.next()?;
            match kind {
                b"A" => Some(OscEvent::PromptStart),
                b"B" => Some(OscEvent::CommandStart),
                b"C" => Some(OscEvent::OutputStart),
                b"D" => {
                    let code = match params.next() {
                        Some(code) if !code.is_empty() => {
                            // the shell may report negative codes; a
                            // non-number is a report of none
                            std::str::from_utf8(code)
                                .ok()
                                .and_then(|s| s.trim().parse::<i32>().ok())
                        },
                        _ => None,
                    };
                    Some(OscEvent::CommandFinished(code))
                },
                _ => None,
            }
        },
        b"7" => {
            let uri: Vec<u8> = params.collect::<Vec<&[u8]>>().join(&b';');
            file_uri_cwd(&uri)
        },
        b"9" => {
            let first = params.next()?;
            if first == b"9" {
                // ConEmu's cwd report: a plain path, no URI wrapping
                let path: Vec<u8> = params.collect::<Vec<&[u8]>>().join(&b';');
                let path = std::str::from_utf8(&path).ok()?.trim().to_owned();
                if !path.starts_with('/') {
                    return None;
                }
                return Some(OscEvent::Cwd { host: None, path });
            }
            // iTerm2's notification: the rest is the message. ConEmu's
            // numbered variants (`9;4;3` progress and friends) are not
            // ours to guess: a digit-led first segment with more
            // segments behind it is their shape, and a notification
            // flood from some tool's progress bar would be the
            // misread's price. A plain numeric message ("42") has no
            // second segment and stays a notification.
            let rest: Vec<&[u8]> = std::iter::once(first).chain(params).collect();
            if rest.len() > 1
                && !rest[0].is_empty()
                && rest[0].iter().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            let body = String::from_utf8_lossy(&rest.join(&b';')).into_owned();
            Some(OscEvent::Notification { title: None, body })
        },
        b"777" => {
            if params.next()? != b"notify" {
                return None;
            }
            let title = params.next().map(String::from_utf8_lossy).map(String::from);
            let body: Vec<u8> = params.collect::<Vec<&[u8]>>().join(&b';');
            let body = String::from_utf8_lossy(&body).into_owned();
            Some(OscEvent::Notification { title: Some(title?), body })
        },
        b"7501" => {
            let status = params.next()?;
            if status == b"?" {
                return Some(OscEvent::ProgramStatusQuery);
            }
            let status = String::from_utf8_lossy(status).into_owned();
            let message: Vec<u8> = params.collect::<Vec<&[u8]>>().join(&b';');
            let message = if message.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(&message).into_owned())
            };
            Some(OscEvent::ProgramStatus { status, message })
        },
        _ => None,
    }
}

/// `file://host/path` with the path percent-decoded. A non-file scheme
/// (an ftp URI, a bare path) is ignored: OSC 7's spec shape is a file
/// URI, and a foreign scheme's path would be a lie about a directory.
fn file_uri_cwd(uri: &[u8]) -> Option<OscEvent> {
    let uri = std::str::from_utf8(uri).ok()?;
    let rest = uri.strip_prefix("file://")?;
    let (host, path) = match rest.find('/') {
        Some(slash) => (&rest[..slash], &rest[slash..]),
        // file:///path spells an empty host
        None => ("", rest),
    };
    let host = if host.is_empty() { None } else { Some(host.to_owned()) };
    let path = percent_decode(path)?;
    if !path.starts_with('/') {
        return None;
    }
    Some(OscEvent::Cwd { host, path })
}

/// Percent-decode a URI path segment: `%XX` hex pairs become their
/// byte, everything else passes through. Invalid escapes stay literal
/// (a shell that half-encodes still reports a usable path). The result
/// must be valid UTF-8 to be a directory name; lossy would invent
/// characters for a path nobody owns.
fn percent_decode(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = bytes.get(at + 1..at + 3)?;
            let value = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
            out.push(value);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(tap: &mut Tap, bytes: &[u8]) -> Vec<OscEvent> {
        tap.feed(bytes).into_iter().map(|(_, event)| event).collect()
    }

    #[test]
    fn prompt_markers_arrive_in_order() {
        let mut tap = Tap::default();
        let events = feed(
            &mut tap,
            b"\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07out\r\n\x1b]133;D;0\x07",
        );
        assert_eq!(
            events,
            vec![
                OscEvent::PromptStart,
                OscEvent::CommandStart,
                OscEvent::OutputStart,
                OscEvent::CommandFinished(Some(0)),
            ]
        );
    }

    #[test]
    fn st_terminator_works_like_bel() {
        let mut tap = Tap::default();
        assert_eq!(
            feed(&mut tap, b"\x1b]133;A\x1b\\"),
            vec![OscEvent::PromptStart]
        );
    }

    #[test]
    fn chunk_splits_never_lose_a_sequence() {
        let full = b"\x1b]133;D;127\x07\x1b]7;file://host/home/user%20dir\x07";
        // every split of the two sequences reports both events
        for mid in 1..full.len() {
            let mut tap = Tap::default();
            let mut events = feed(&mut tap, &full[..mid]);
            events.extend(feed(&mut tap, &full[mid..]));
            assert_eq!(
                events,
                vec![
                    OscEvent::CommandFinished(Some(127)),
                    OscEvent::Cwd { host: Some("host".into()), path: "/home/user dir".into() },
                ],
                "split at {mid}"
            );
        }
    }

    #[test]
    fn conemu_cwd_reports_a_plain_path() {
        let mut tap = Tap::default();
        assert_eq!(
            feed(&mut tap, b"\x1b]9;9;/var/tmp\x07"),
            vec![OscEvent::Cwd { host: None, path: "/var/tmp".into() }]
        );
    }

    #[test]
    fn notifications_arrive_in_both_spellings() {
        let mut tap = Tap::default();
        assert_eq!(
            feed(&mut tap, b"\x1b]9;build done\x07"),
            vec![OscEvent::Notification { title: None, body: "build done".into() }]
        );
        assert_eq!(
            feed(&mut tap, b"\x1b]777;notify;Title;Body;more\x07"),
            vec![OscEvent::Notification {
                title: Some("Title".into()),
                body: "Body;more".into()
            }]
        );
    }

    #[test]
    fn program_status_forwards_and_queries() {
        let mut tap = Tap::default();
        assert_eq!(
            feed(&mut tap, b"\x1b]7501;working\x07"),
            vec![OscEvent::ProgramStatus { status: "working".into(), message: None }]
        );
        assert_eq!(
            feed(&mut tap, b"\x1b]7501;failed;exit 2\x07"),
            vec![OscEvent::ProgramStatus {
                status: "failed".into(),
                message: Some("exit 2".into())
            }]
        );
        assert_eq!(feed(&mut tap, b"\x1b]7501;?\x07"), vec![OscEvent::ProgramStatusQuery]);
    }

    #[test]
    fn overlong_sequences_are_abandoned_whole() {
        let mut tap = Tap::default();
        let long = vec![b'x'; OSC_CAP + 10];
        // the abandoned sequence dispatches nothing...
        assert!(feed(&mut tap, &[&b"\x1b]133;A;"[..], &long, b"\x07"].concat()).is_empty());
        // ...and the tap still recognizes the next one
        assert_eq!(feed(&mut tap, b"\x1b]133;A\x07"), vec![OscEvent::PromptStart]);
    }

    #[test]
    fn an_escape_inside_an_osc_abandons_it() {
        let mut tap = Tap::default();
        // xterm's behavior: the fresh escape wins, the OSC is gone
        assert!(feed(&mut tap, b"\x1b]133;A\x1b[2J").is_empty());
        assert_eq!(feed(&mut tap, b"\x1b]133;A\x07"), vec![OscEvent::PromptStart]);
    }

    #[test]
    fn an_escape_then_bracket_inside_an_osc_opens_a_new_one() {
        let mut tap = Tap::default();
        // ESC ] is not ST: the first OSC dies, the second begins
        assert_eq!(
            feed(&mut tap, b"\x1b]133;A\x1b]133;B\x07"),
            vec![OscEvent::CommandStart]
        );
    }

    #[test]
    fn esc_esc_inside_an_osc_then_still_dispatches_the_next() {
        let mut tap = Tap::default();
        assert!(feed(&mut tap, b"\x1b]133;A\x1b\x1b").is_empty());
        assert_eq!(feed(&mut tap, b"]133;C\x07"), vec![OscEvent::OutputStart]);
    }

    #[test]
    fn unknown_oscs_and_subcommands_are_dropped() {
        let mut tap = Tap::default();
        assert!(feed(&mut tap, b"\x1b]52;c;base64\x07").is_empty());
        assert!(feed(&mut tap, b"\x1b]777;urllaunch;http://x\x07").is_empty());
        // ConEmu's numbered OSC 9 variants are not notifications
        assert!(feed(&mut tap, b"\x1b]9;4;3\x07").is_empty());
        assert!(feed(&mut tap, b"\x1b]4;1;rgb:00/00/00\x07").is_empty());
    }

    #[test]
    fn a_non_file_osc7_uri_is_ignored() {
        let mut tap = Tap::default();
        assert!(feed(&mut tap, b"\x1b]7;ftp://host/dir\x07").is_empty());
        assert!(feed(&mut tap, b"\x1b]7;relative/path\x07").is_empty());
    }

    #[test]
    fn empty_and_root_file_uris() {
        let mut tap = Tap::default();
        assert_eq!(
            feed(&mut tap, b"\x1b]7;file:///\x07"),
            vec![OscEvent::Cwd { host: None, path: "/".into() }]
        );
    }

    #[test]
    fn an_empty_osc_dispatches_nothing() {
        let mut tap = Tap::default();
        assert!(feed(&mut tap, b"\x1b]\x07").is_empty());
    }

    #[test]
    fn command_finished_without_a_code() {
        let mut tap = Tap::default();
        assert_eq!(
            feed(&mut tap, b"\x1b]133;D\x07"),
            vec![OscEvent::CommandFinished(None)]
        );
        assert_eq!(
            feed(&mut tap, b"\x1b]133;D;\x07"),
            vec![OscEvent::CommandFinished(None)]
        );
        assert_eq!(
            feed(&mut tap, b"\x1b]133;D;not-a-number\x07"),
            vec![OscEvent::CommandFinished(None)]
        );
    }
}
