//! The minimal niri IPC client: one request, one reply line, the Ok
//! envelope unwrapped. Copied per the freeze (#33): the shell's
//! adapter (kuma-shell/src/niri.rs) is the original; the spike adds
//! only the pid field the shell's SessionWindow never needed.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

/// One window niri knows about: the join's right half. The window's
/// own id drops out: the join keys on pid, and nothing here focuses
/// or closes windows.
#[derive(Deserialize)]
pub(crate) struct NiriWindow {
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub is_focused: bool,
}

pub(crate) fn socket_path() -> Result<PathBuf> {
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

/// Ask niri for its window list. The reply's shape is
/// `{"Ok":{"Windows":[...]}}`, the Response enum nested in the Ok
/// envelope. Read timeouts keep a stuck socket from stalling the
/// sample tick: a beat that cannot ask degrades to unjoined rows
/// instead of freezing the view.
pub(crate) fn windows() -> Result<Vec<NiriWindow>> {
    let path = socket_path()?;
    let stream = UnixStream::connect(&path).context("failed to connect to niri IPC socket")?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    let mut writer = &stream;
    writer.write_all(json!("Windows").to_string().as_bytes())?;
    writer.write_all(b"\n")?;

    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    if !reply.starts_with("{\"Ok\"") {
        bail!("niri request failed: {reply}");
    }
    let envelope: Value = serde_json::from_str(reply.trim())
        .with_context(|| format!("niri reply not JSON: {reply}"))?;
    let windows = envelope["Ok"]["Windows"].clone();
    Ok(serde_json::from_value(windows)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The envelope the live socket answers: one line, Ok wrapping
    /// the Response enum's tagged variant.
    #[test]
    fn parses_the_windows_reply() {
        let reply = r#"{"Ok":{"Windows":[{"id":2,"pid":2298,"app_id":"kuma-term","title":"kuma-term","workspace_id":{"Id":1},"is_focused":true,"is_urgent":false,"layout":null}]}}"#;
        let envelope: Value = serde_json::from_str(reply).unwrap();
        let windows: Vec<NiriWindow> =
            serde_json::from_value(envelope["Ok"]["Windows"].clone()).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].pid, Some(2298));
        assert_eq!(windows[0].app_id.as_deref(), Some("kuma-term"));
        assert!(windows[0].is_focused);
    }

    #[test]
    fn tolerates_missing_fields() {
        let windows: Vec<NiriWindow> = serde_json::from_value(json!([
            {"id": 7},
            {"id": 8, "pid": 42, "app_id": null, "title": null, "is_focused": true},
        ]))
        .unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].pid, None);
        assert_eq!(windows[0].app_id, None);
        assert!(!windows[0].is_focused);
        assert!(windows[1].is_focused);
    }
}
