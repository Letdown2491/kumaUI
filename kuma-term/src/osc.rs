//! Shell-integration markers scanned from the raw PTY byte stream.
//!
//! The vte parser behind the engine seam has no hook for arbitrary OSC
//! sequences (everything unknown lands in its `unhandled` bucket), so OSC
//! 133 (semantic prompts) and OSC 7 (cwd) are tapped from the bytes the
//! child writes before they reach the parser. The scanner is a pure
//! streaming state machine: feed it chunks as they arrive, it yields
//! events and always passes the bytes through unchanged.

use std::time::Instant;

/// What the shell announced. OSC 133 marks the prompt lifecycle; OSC 7
/// carries the working directory as a file:// URI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Marker {
    /// OSC 133;A: the shell started drawing its prompt.
    PromptStart,
    /// OSC 133;B: the prompt ended, command input begins.
    CommandStart,
    /// OSC 133;C: the user pressed enter, the command is running.
    /// Carries the command line when the shell reports it (OSC 133;C;cmd).
    OutputStart(Option<String>),
    /// OSC 133;D;code: the command finished. The code is the raw decimal
    /// the shell sent (empty when the shell omitted it).
    CommandDone(Option<i32>),
    /// OSC 7;file://host/path: the shell's working directory. The host is
    /// kept so a remote cwd (ssh) can be told apart from a local one.
    Cwd { host: String, path: String },
}

/// Scanner states. OSC sequences start with ESC ], end with BEL or ESC \,
/// and can be split across any number of read chunks.
#[derive(Default)]
enum State {
    #[default]
    Ground,
    /// saw ESC
    Esc,
    /// saw ESC ] (inside an OSC), collecting the payload
    Osc {
        payload: Vec<u8>,
    },
    /// inside an OSC, saw ESC (candidate terminator ESC \)
    OscEsc {
        payload: Vec<u8>,
    },
}

pub struct Scanner {
    state: State,
}

impl Scanner {
    pub fn new() -> Self {
        Self { state: State::Ground }
    }

    /// Feed bytes from the PTY; returns the markers found in this chunk,
    /// in order. Bytes are never consumed: the caller forwards the same
    /// chunk to the terminal parser.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Marker> {
        let mut markers = Vec::new();
        let mut state = std::mem::take(&mut self.state);
        for &byte in bytes {
            state = match state {
                State::Ground => {
                    if byte == 0x1b {
                        State::Esc
                    } else {
                        State::Ground
                    }
                },
                State::Esc => {
                    if byte == b']' {
                        State::Osc { payload: Vec::new() }
                    } else if byte == 0x1b {
                        State::Esc
                    } else {
                        State::Ground
                    }
                },
                State::Osc { mut payload } => {
                    match byte {
                        0x07 => {
                            // BEL terminator
                            if let Some(marker) = parse_osc(&payload) {
                                markers.push(marker);
                            }
                            State::Ground
                        },
                        0x1b => State::OscEsc { payload },
                        _ => {
                            if payload.len() < 4096 {
                                // the cap keeps a pathological shell from
                                // growing the buffer without bound; real
                                // markers are tiny
                                payload.push(byte);
                            }
                            State::Osc { payload }
                        },
                    }
                },
                State::OscEsc { payload } => {
                    if byte == b'\\' {
                        if let Some(marker) = parse_osc(&payload) {
                            markers.push(marker);
                        }
                        State::Ground
                    } else {
                        // ESC inside the payload was not a terminator: it
                        // was payload content (rare but legal), so both
                        // bytes belong to the payload
                        let mut payload = payload;
                        if payload.len() < 4096 {
                            payload.push(0x1b);
                            payload.push(byte);
                        }
                        State::Osc { payload }
                    }
                },
            };
        }
        self.state = state;
        markers
    }
}

/// Decode one OSC payload (`133;A`, `7;file://host/path`, ...) into a
/// marker. Unknown sequences yield None; the scanner stays passive for
/// everything it does not own.
fn parse_osc(payload: &[u8]) -> Option<Marker> {
    let text = String::from_utf8_lossy(payload);
    let (code, rest) = text.split_once(';').unwrap_or((text.as_ref(), ""));
    match code {
        "133" => match rest {
            "A" => Some(Marker::PromptStart),
            "B" => Some(Marker::CommandStart),
            _ => {
                if let Some(cmd) = rest.strip_prefix("C") {
                    let cmd = cmd.strip_prefix(';').unwrap_or("");
                    let cmd = (!cmd.is_empty()).then(|| cmd.to_string());
                    Some(Marker::OutputStart(cmd))
                } else if let Some(code) = rest.strip_prefix("D") {
                    let code = code.strip_prefix(';').unwrap_or("");
                    let code = code.parse::<i32>().ok();
                    Some(Marker::CommandDone(code))
                } else {
                    None
                }
            },
        },
        "7" => {
            let uri = rest.strip_prefix("file://")?;
            let (host, path) = uri.split_once('/').map(|(h, p)| (h.to_string(), format!("/{p}")))?;
            Some(Marker::Cwd { host, path })
        },
        _ => None,
    }
}

/// The bar's data model, maintained by the view from the marker stream.
#[derive(Clone, Debug, Default)]
pub struct BarState {
    /// last OSC 7 cwd (local only: remote hosts are ignored)
    pub cwd: Option<String>,
    /// cwd abbreviated fish-style: home becomes ~, last two components kept
    pub cwd_short: Option<String>,
    /// exit code of the last finished command (None = none reported yet)
    pub last_exit: Option<i32>,
    /// how long the last command ran
    pub last_duration: Option<std::time::Duration>,
    /// the command is running right now
    pub running: bool,
    /// when the running command started
    pub started_at: Option<Instant>,
}

impl BarState {
    /// Apply one marker. Duration math uses `now` so tests can drive time.
    pub fn apply(&mut self, marker: &Marker, now: std::time::Instant) {
        match marker {
            Marker::PromptStart => {
                self.running = false;
            },
            Marker::OutputStart(_) => {
                self.running = true;
                self.started_at = Some(now);
            },
            Marker::CommandDone(code) => {
                self.last_exit = *code;
                self.last_duration = self.started_at.take().map(|s| now - s);
                self.running = false;
            },
            Marker::Cwd { host, path } => {
                // a remote cwd (ssh with OSC 7 forwarding) would lie about
                // local paths; only trust this machine's own hostname
                let local = local_hostname().unwrap_or_default();
                if host.is_empty() || host.eq_ignore_ascii_case(&local) {
                    self.cwd = Some(path.clone());
                    self.cwd_short = Some(abbreviate_cwd(path));
                }
            },
            Marker::CommandStart => {},
        }
    }
}

/// This machine's hostname, or None if the OS call fails.
fn local_hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: buf outlives the call; gethostname NUL-terminates within len
    let res = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if res != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    std::str::from_utf8(&buf[..end]).ok().map(str::to_string)
}

/// Abbreviate a path fish-style: home becomes ~, the last two components
/// are kept past that.
fn abbreviate_cwd(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let (base, tail) = match path.strip_prefix(&home) {
        _ if home.is_empty() => (String::new(), path.to_string()),
        Some("") => return "~".to_string(),
        Some(rest) if rest.starts_with('/') => ("~".to_string(), rest.to_string()),
        _ => (String::new(), path.to_string()),
    };
    let mut parts: Vec<&str> = tail.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() > 2 {
        parts = parts.split_off(parts.len() - 2);
    }
    if base.is_empty() {
        format!("/{}", parts.join("/"))
    } else {
        format!("{base}/{}", parts.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(text: &str) -> Vec<Marker> {
        let mut scanner = Scanner::new();
        scanner.feed(text.as_bytes())
    }

    #[test]
    fn plain_bytes_produce_no_markers() {
        assert!(feed_all("hello world \x1b[31mred\x1b[0m").is_empty());
    }

    #[test]
    fn prompt_lifecycle_markers_across_chunks() {
        // the whole lifecycle, split byte by byte: chunk boundaries must
        // not matter
        let text = "\x1b]133;A\x1b\\\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;0\x07";
        let mut scanner = Scanner::new();
        let mut markers = Vec::new();
        for chunk in text.as_bytes().chunks(3) {
            markers.extend(scanner.feed(chunk));
        }
        assert_eq!(
            markers,
            vec![
                Marker::PromptStart,
                Marker::CommandStart,
                Marker::OutputStart(None),
                Marker::CommandDone(Some(0)),
            ]
        );
    }

    #[test]
    fn command_done_carries_exit_code() {
        assert_eq!(feed_all("\x1b]133;D;130\x07"), vec![Marker::CommandDone(Some(130))]);
        // shell may omit the code
        assert_eq!(feed_all("\x1b]133;D\x07"), vec![Marker::CommandDone(None)]);
    }

    #[test]
    fn cwd_marker_parses_the_file_uri() {
        assert_eq!(
            feed_all("\x1b]7;file://motherbox/home/martin/D/kumaui\x07"),
            vec![Marker::Cwd {
                host: "motherbox".to_string(),
                path: "/home/martin/D/kumaui".to_string(),
            }]
        );
    }

    #[test]
    fn unknown_osc_passes_without_markers() {
        // OSC 0 (title), OSC 8 (hyperlink), OSC 52 (clipboard): not ours
        assert!(feed_all("\x1b]0;my title\x07").is_empty());
        assert!(feed_all("\x1b]8;;https://example.com\x07").is_empty());
        assert!(feed_all("\x1b]52;c;AAAA\x1b\\").is_empty());
    }

    #[test]
    fn esc_inside_payload_does_not_end_the_sequence() {
        // an ESC that is not followed by \\ stays payload
        let markers = feed_all("\x1b]7;file://h/pa\x1bth\x07");
        assert_eq!(
            markers,
            vec![Marker::Cwd { host: "h".to_string(), path: "/pa\x1bth".to_string() }]
        );
    }

    #[test]
    fn oversized_payload_is_dropped_not_grown() {
        let mut text = String::from("\x1b]7;file://h/");
        text.push_str(&"a".repeat(5000));
        text.push('\x07');
        let markers = feed_all(&text);
        // the scanner survives; the marker may or may not parse, but the
        // buffer never exceeded the cap and the state machine resets
        assert!(matches!(markers.as_slice(), [] | [Marker::Cwd { .. }]));
        // and it still recognizes the next marker afterwards
        assert_eq!(feed_all("\x1b]133;A\x07"), vec![Marker::PromptStart]);
    }

    #[test]
    fn bar_state_tracks_the_lifecycle() {
        let t0 = std::time::Instant::now();
        let mut bar = BarState::default();
        let host = local_hostname().unwrap_or_default();
        bar.apply(&Marker::Cwd {
            host,
            path: std::env::var("HOME").unwrap(),
        }, t0);
        bar.apply(&Marker::PromptStart, t0);
        bar.apply(&Marker::OutputStart(None), t0);
        assert!(bar.running);
        let later = t0 + std::time::Duration::from_millis(2500);
        bar.apply(&Marker::CommandDone(Some(1)), later);
        assert!(!bar.running);
        assert_eq!(bar.last_exit, Some(1));
        assert_eq!(bar.last_duration, Some(std::time::Duration::from_millis(2500)));
    }

    #[test]
    fn remote_cwd_is_ignored() {
        let t0 = std::time::Instant::now();
        let mut bar = BarState::default();
        bar.apply(&Marker::Cwd {
            host: "faraway".to_string(),
            path: "/remote/path".to_string(),
        }, t0);
        assert_eq!(bar.cwd, None);
    }

    #[test]
    fn cwd_abbreviation_matches_fish_shape() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            abbreviate_cwd(&format!("{home}/Documents/kumaui")),
            "~/Documents/kumaui"
        );
        // deep paths keep the last two components
        assert_eq!(abbreviate_cwd(&format!("{home}/a/b/c/d")), "~/c/d");
        assert_eq!(abbreviate_cwd("/"), "/");
        assert_eq!(abbreviate_cwd("/usr/local"), "/usr/local");
    }
}
