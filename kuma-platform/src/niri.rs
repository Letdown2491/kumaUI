//! The niri IPC envelope. ADR-0003 chose a hand-rolled client over a
//! crate: one request line, one reply line, niri's Ok envelope
//! unwrapped. kuma-shell's adapter is the original; this is the
//! envelope both it and the monitor share. Events, actions, and
//! response types stay in the apps: the platform carries no session
//! policy.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use serde_json::Value;

/// The socket's path: NIRI_SOCKET when the session set it, else the
/// well-known socket in XDG_RUNTIME_DIR.
pub fn socket_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("NIRI_SOCKET") {
        return Ok(PathBuf::from(path));
    }
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    std::fs::read_dir(&runtime_dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("niri.") && name.ends_with(".sock"))
        })
        .context("no niri IPC socket found in XDG_RUNTIME_DIR")
}

/// Send one request line and return niri's Ok payload. The error path
/// keeps niri's own answer text, which is what makes IPC failures
/// diagnosable in the journal.
pub fn request(request: &Value) -> Result<Value> {
    exchange(request, None)
}

/// The same exchange under a read/write timeout: a caller on a
/// polling beat keeps a stuck socket from stalling its loop.
pub fn request_with_timeout(request: &Value, timeout: Duration) -> Result<Value> {
    exchange(request, Some(timeout))
}

fn exchange(request: &Value, timeout: Option<Duration>) -> Result<Value> {
    let path = socket_path()?;
    let stream = UnixStream::connect(&path).context("failed to connect to niri IPC socket")?;
    if let Some(timeout) = timeout {
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
    }

    let mut writer = &stream;
    writer.write_all(request.to_string().as_bytes())?;
    writer.write_all(b"\n")?;

    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    unwrap_envelope(&reply)
}

/// One reply line in, niri's Ok payload out. The `{"Err": ...}`
/// envelope (or anything that is not Ok) fails with niri's own text.
fn unwrap_envelope(reply: &str) -> Result<Value> {
    if !reply.starts_with("{\"Ok\"") {
        bail!("niri request failed: {reply}");
    }
    let envelope: Value = serde_json::from_str(reply.trim())
        .with_context(|| format!("niri reply not JSON: {reply}"))?;
    Ok(envelope["Ok"].clone())
}

/// One niri window, the fields the family's own-window checks need.
/// `is_floating` stays an Option twice over: a niri older than the
/// field parses as None, and None means unknown, which every caller
/// reads as not tiled (the bar stays).
#[derive(Debug, Deserialize)]
pub struct OwnWindow {
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub is_floating: Option<bool>,
}

/// The niri windows this process owns: niri tags every window with
/// the pid that opened its Wayland connection, and an app's windows
/// are exactly the ones carrying its own. Err when there is no niri
/// to ask (any other desktop): callers treat that as unknown and
/// leave their bars alone.
pub fn own_windows() -> Result<Vec<OwnWindow>> {
    let payload = request_with_timeout(&serde_json::json!("Windows"), Duration::from_secs(2))?;
    own_from_payload(&payload)
}

fn own_from_payload(payload: &Value) -> Result<Vec<OwnWindow>> {
    let windows: Vec<OwnWindow> = serde_json::from_value(payload["Windows"].clone())
        .context("niri's Windows reply did not parse")?;
    let pid = std::process::id();
    Ok(windows
        .into_iter()
        .filter(|window| window.pid == Some(pid))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_ok_envelope_unwraps_to_its_payload() {
        let reply = r#"{"Ok":{"Windows":[{"id":2,"pid":2298}]}}"#;
        let payload = unwrap_envelope(reply).unwrap();
        assert_eq!(payload, json!({"Windows": [{"id": 2, "pid": 2298}]}));
    }

    #[test]
    fn the_err_envelope_fails_with_niris_own_text() {
        let err = unwrap_envelope(r#"{"Err":"some mistake"}"#).unwrap_err();
        assert!(err.to_string().contains("some mistake"), "{err:#}");
        assert!(unwrap_envelope("not json at all").is_err());
        assert!(unwrap_envelope("").is_err());
    }

    #[test]
    fn own_windows_keeps_this_pids_windows_only() {
        let mine = std::process::id();
        let payload = serde_json::json!({"Windows": [
            {"id": 1, "pid": mine, "title": "Higuma", "is_floating": false},
            {"id": 2, "pid": 2298, "title": "other", "is_floating": true},
            {"id": 3, "pid": mine, "title": "Koguma", "is_floating": true},
        ]});
        let own = own_from_payload(&payload).unwrap();
        assert_eq!(own.len(), 2, "only this process's windows");
        assert!(own.iter().all(|w| w.pid == Some(mine)));
    }

    #[test]
    fn own_windows_reads_a_reply_without_the_floating_field() {
        // an older niri: the field is absent, the parse holds, and the
        // unknown reads as not tiled
        let payload = serde_json::json!({"Windows": [
            {"id": 2, "pid": std::process::id(), "title": "kuma-term"},
        ]});
        let own = own_from_payload(&payload).unwrap();
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].is_floating, None);
    }
}
