//! The monitor's half of the niri join: the window list request and
//! the response type. The envelope (connect, one line, the Ok unwrap)
//! lives in kuma-platform's niri module; the shell's adapter is the
//! original (ADR-0003). The spike adds only the pid field the shell's
//! SessionWindow never needed.

use std::time::Duration;

use anyhow::{Context as _, Result};
use kuma_platform::niri;
use serde::Deserialize;
use serde_json::json;

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

/// Ask niri for its window list. The reply's shape is
/// `{"Ok":{"Windows":[...]}}`; the envelope lives in the platform.
/// The timeout keeps a stuck socket from stalling the sample loop
/// for good: a beat that cannot ask degrades to unjoined rows
/// instead of freezing the table.
pub(crate) fn windows() -> Result<Vec<NiriWindow>> {
    let payload = niri::request_with_timeout(&json!("Windows"), Duration::from_secs(2))?;
    serde_json::from_value(payload["Windows"].clone())
        .context("niri's Windows reply did not parse")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The envelope the live socket answers: one line, Ok wrapping
    /// the Response enum's tagged variant. The envelope's unwrap is
    /// the platform's; this pins that the reply's own shape feeds
    /// the monitor's window type.
    #[test]
    fn parses_the_windows_reply() {
        let reply = r#"{"Ok":{"Windows":[{"id":2,"pid":2298,"app_id":"kuma-term","title":"kuma-term","workspace_id":{"Id":1},"is_focused":true,"is_urgent":false,"layout":null}]}}"#;
        let envelope: serde_json::Value = serde_json::from_str(reply).unwrap();
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
