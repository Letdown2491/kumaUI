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
}
