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
use serde::Serialize;

/// What the MSG socket can ask the running shell to do.
#[derive(Clone, Debug, PartialEq, Serialize)]
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
}

impl Request {
    fn key(&self) -> &'static str {
        match self {
            Request::Launcher => "Launcher",
            Request::Notifications => "Notifications",
            Request::Nostr(_) => "Nostr",
            Request::NostrPanel => "NostrPanel",
        }
    }

    fn value(&self) -> String {
        match self {
            Request::Launcher => "Toggle".into(),
            Request::Notifications => "Dnd".into(),
            Request::Nostr(uri) => uri.clone(),
            Request::NostrPanel => "Open".into(),
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
        _ => None,
    }
}

/// The client half: connect, send one line, read the reply, print it.
pub fn send(request: &Request) -> anyhow::Result<()> {
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
    print!("{reply_line}");
    Ok(())
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
    fn a_garbage_line_is_none_and_the_reply_says_so() {
        assert_eq!(parse("hello"), None);
        assert_eq!(parse("{\"Nostr\": 3}"), None);
        assert_eq!(parse("{\"Bogus\":\"x\"}"), None);
        assert_eq!(reply(false), "{\"Err\":\"unknown request\"}\n");
    }
}
