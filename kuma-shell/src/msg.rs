//! The MSG protocol: the socket the CLI talks to, in one module; the
//! request enum, the accept loop, the client sender, and the reply.
//!
//! This module exists because the nostr offer became the second socket
//! verb. ADR-0005 deferred the extraction until exactly that arrival:
//! with one verb, a module was a pass-through shim; with the offer
//! carrying a URI through the wire, substring matching on the request
//! line stopped being enough to be honest. Parsing is serde now, the
//! wire shape is unchanged (`{"Launcher":"Toggle"}`), and the CLI is a
//! verb → request mapping.

use anyhow::Context;

/// What the MSG socket can ask the running shell to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Launcher,
    Notifications,
    /// The scheme handler's nostrconnect:// link: the URI rides the
    /// request, the shell opens the Pair pane with it as an offer.
    /// Argv form end to end: the URI's `&` and `?` never meet a shell.
    Nostr(String),
    /// Open the Nostr Signer panel with no offer attached; the
    /// keybind's and the app grid's road. An offer must come with a
    /// URI; a hand coming from a keybind has none.
    NostrPanel,
    /// The bar gear's and the settings keybind's road: toggle the
    /// settings panel. The road out of the KUMA_DEBUG_OPEN_SETTINGS
    /// scaffolding the panel was built behind.
    Settings,
    /// The audio and brightness keybinds' road: applied in-process, the
    /// widgets and the OSD react at once instead of at the next poll.
    Volume(VolumeCmd),
    Brightness(BrightnessCmd),
    MicMute,
}

/// The volume keybind's direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeCmd {
    Up,
    Down,
    Mute,
}

/// The brightness keybind's direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrightnessCmd {
    Up,
    Down,
}

impl Request {
    fn key(&self) -> &'static str {
        match self {
            Request::Launcher => "Launcher",
            Request::Notifications => "Notifications",
            Request::Nostr(_) => "Nostr",
            Request::NostrPanel => "NostrPanel",
            Request::Settings => "Settings",
            Request::Volume(_) => "Volume",
            Request::Brightness(_) => "Brightness",
            Request::MicMute => "Mic",
        }
    }

    fn value(&self) -> String {
        match self {
            Request::Launcher => "Toggle".into(),
            Request::Notifications => "Dnd".into(),
            Request::Nostr(uri) => uri.clone(),
            Request::NostrPanel => "Open".into(),
            Request::Settings => "Toggle".into(),
            Request::Volume(cmd) => match cmd {
                VolumeCmd::Up => "Up".into(),
                VolumeCmd::Down => "Down".into(),
                VolumeCmd::Mute => "Mute".into(),
            },
            Request::Brightness(cmd) => match cmd {
                BrightnessCmd::Up => "Up".into(),
                BrightnessCmd::Down => "Down".into(),
            },
            Request::MicMute => "Mute".into(),
        }
    }
}

/// The reply the running shell sends, one line.
fn reply(handled: bool) -> &'static str {
    if handled {
        "{\"Ok\":\"Handled\"}\n"
    } else {
        "{\"Err\":\"unknown request\"}\n"
    }
}

/// The wire: `{"Key":"Value"}`, the shape the first verb shipped with,
/// kept byte-compatible so the two CLI spellings (`kuma-shell
/// launcher-toggle` and the noctalia-compatible `kuma-shell msg
/// launcher-toggle`) keep working across this refactor.
fn to_line(request: &Request) -> String {
    format!(
        "{{\"{}\":\"{}\"}}",
        request.key(),
        request.value().replace('\\', "\\\\").replace('"', "\\\"")
    )
}

/// The same wire, read back. Case-insensitive on the key (the old
/// dispatch lowercased the whole line), and an unrecognized key is
/// None; the reply says so.
pub fn parse(line: &str) -> Option<Request> {
    let doc: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let map = doc.as_object()?;
    let (key, value) = map.iter().next()?;
    let value = value.as_str()?;
    match key.to_lowercase().as_str() {
        "launcher" => Some(Request::Launcher),
        "notifications" => Some(Request::Notifications),
        "nostr" => Some(Request::Nostr(value.to_string())),
        "nostrpanel" => Some(Request::NostrPanel),
        "settings" => Some(Request::Settings),
        "volume" => match value.to_lowercase().as_str() {
            "up" => Some(Request::Volume(VolumeCmd::Up)),
            "down" => Some(Request::Volume(VolumeCmd::Down)),
            "mute" => Some(Request::Volume(VolumeCmd::Mute)),
            _ => None,
        },
        "brightness" => match value.to_lowercase().as_str() {
            "up" => Some(Request::Brightness(BrightnessCmd::Up)),
            "down" => Some(Request::Brightness(BrightnessCmd::Down)),
            _ => None,
        },
        "mic" => match value.to_lowercase().as_str() {
            "mute" => Some(Request::MicMute),
            _ => None,
        },
        _ => None,
    }
}

/// The client half: connect, send one line, read the reply.
fn transmit(request: &Request) -> anyhow::Result<String> {
    use std::io::{BufRead, BufReader, Write};
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    let path = std::path::PathBuf::from(runtime_dir).join("kuma-shell.sock");
    let stream = std::os::unix::net::UnixStream::connect(&path)
        .with_context(|| format!("is kuma-shell running? (connect to {path:?} failed)"))?;
    let mut writer = &stream;
    writer.write_all(to_line(request).as_bytes())?;
    writer.write_all(b"\n")?;
    let mut reader = BufReader::new(stream);
    let mut reply_line = String::new();
    reader.read_line(&mut reply_line)?;
    Ok(reply_line)
}

/// Send one request and print the reply.
pub fn send(request: &Request) -> anyhow::Result<()> {
    print!("{}", transmit(request)?);
    Ok(())
}

/// Send one request without printing the reply: the audio and
/// brightness verbs have no terminal to show JSON on.
pub fn send_quiet(request: &Request) -> anyhow::Result<()> {
    transmit(request).map(|_| ())
}

/// The server half: one thread, accept loop, parse, reply, forward.
/// The channel hands the request to the GUI main loop, which owns the
/// entities; the accept thread never touches state.
pub fn spawn_listener(txs: smol::channel::Sender<Request>) {
    let Some(runtime_dir) = std::env::var("XDG_RUNTIME_DIR").ok() else {
        return;
    };
    std::thread::spawn(move || {
        use std::io::{BufRead, BufReader, Write};

        let path = std::path::PathBuf::from(runtime_dir).join("kuma-shell.sock");
        let _ = std::fs::remove_file(&path);
        let Ok(listener) = std::os::unix::net::UnixListener::bind(&path) else {
            log::error!("failed to bind IPC socket at {path:?}");
            return;
        };
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let mut writer = &stream;
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let request = parse(&line);
            let _ = writer.write_all(reply(request.is_some()).as_bytes());
            if let Some(request) = request {
                let _ = smol::block_on(txs.send(request));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_round_trips_through_the_first_verbs_exact_bytes() {
        // the bytes the first CLI shipped, parsed back honestly
        assert_eq!(parse("{\"Launcher\":\"Toggle\"}"), Some(Request::Launcher));
        assert_eq!(
            parse("{\"Notifications\":\"Dnd\"}"),
            Some(Request::Notifications)
        );
        // and the new verb carries its URI whole
        let uri = "nostrconnect://pubkey?relay=wss%3A%2F%2Frelay&name=Signet";
        assert_eq!(
            parse(&to_line(&Request::Nostr(uri.into()))),
            Some(Request::Nostr(uri.into()))
        );
    }

    #[test]
    fn the_panel_request_carries_no_offer() {
        // the keybind's road: open, and nothing lands
        assert_eq!(
            parse(&to_line(&Request::NostrPanel)),
            Some(Request::NostrPanel)
        );
        assert_eq!(to_line(&Request::NostrPanel), "{\"NostrPanel\":\"Open\"}");
    }

    #[test]
    fn the_settings_verb_toggles_the_panel() {
        // launcher semantics: the same press that opened closes
        assert_eq!(parse(&to_line(&Request::Settings)), Some(Request::Settings));
        assert_eq!(to_line(&Request::Settings), "{\"Settings\":\"Toggle\"}");
    }

    #[test]
    fn a_garbage_line_is_none_and_the_reply_says_so() {
        assert_eq!(parse("hello"), None);
        assert_eq!(parse("{\"Nostr\": 3}"), None);
        assert_eq!(parse("{\"Bogus\":\"x\"}"), None);
        assert_eq!(reply(false), "{\"Err\":\"unknown request\"}\n");
    }

    #[test]
    fn the_audio_and_brightness_verbs_round_trip() {
        assert_eq!(
            parse(&to_line(&Request::Volume(VolumeCmd::Up))),
            Some(Request::Volume(VolumeCmd::Up))
        );
        assert_eq!(
            to_line(&Request::Volume(VolumeCmd::Down)),
            "{\"Volume\":\"Down\"}"
        );
        assert_eq!(
            parse(&to_line(&Request::Volume(VolumeCmd::Mute))),
            Some(Request::Volume(VolumeCmd::Mute))
        );
        assert_eq!(
            parse(&to_line(&Request::Brightness(BrightnessCmd::Up))),
            Some(Request::Brightness(BrightnessCmd::Up))
        );
        assert_eq!(
            to_line(&Request::Brightness(BrightnessCmd::Down)),
            "{\"Brightness\":\"Down\"}"
        );
        assert_eq!(parse(&to_line(&Request::MicMute)), Some(Request::MicMute));
        assert_eq!(to_line(&Request::MicMute), "{\"Mic\":\"Mute\"}");
        // a verb with a bogus direction is not a request
        assert_eq!(parse("{\"Volume\":\"Sideways\"}"), None);
    }
}
