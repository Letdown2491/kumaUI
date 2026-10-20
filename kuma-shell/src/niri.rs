//! The niri adapter: translates niri's IPC socket into the neutral
//! session events and commands (`session.rs`). Nothing above this
//! module may mention niri.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
};

use anyhow::{Context as _, Result, bail};
use gpui::{App, Entity};
use log::error;
use serde::Deserialize;
use serde_json::{Value, json};
use smol::channel::Sender;

use crate::session::{SessionEvent, SessionState};

#[derive(Deserialize)]
struct WorkspaceActivatedEvent {
    id: u64,
    focused: bool,
}

#[derive(Deserialize)]
struct WindowClosedEvent {
    id: u64,
}

#[derive(Deserialize)]
struct WindowFocusChangedEvent {
    id: Option<u64>,
}

pub fn connect(state: &Entity<SessionState>, cx: &mut App) {
    crate::session::mirror(state, "niri", cx, run_event_stream);
}

pub(crate) fn socket_path() -> Result<PathBuf> {
    kuma_platform::niri::socket_path()
}

async fn run_event_stream(event_tx: Sender<SessionEvent>) -> Result<()> {
    let path = socket_path()?;
    let stream = UnixStream::connect(&path).context("failed to connect to niri IPC socket")?;

    let mut writer = &stream;
    writer.write_all(b"\"EventStream\"\n")?;

    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    if !reply.starts_with("{\"Ok\"") {
        bail!("unexpected niri IPC reply: {reply}");
    }

    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            bail!("niri closed the event stream");
        }
        // One bad line must not cost the stream: a reshaped event in
        // some future niri would otherwise end the mirror for the
        // whole session. Skip it and let the next event re-sync. EOF
        // and socket errors above still end the attempt; the mirror
        // reconnects (session.rs).
        match parse_event(&line) {
            Ok(Some(event)) => event_tx.send(event).await?,
            Ok(None) => {}
            Err(err) => error!("niri event skipped, parse failed: {err:#}"),
        }
    }
}

fn parse_event(line: &str) -> Result<Option<SessionEvent>> {
    let value: Value = serde_json::from_str(line)?;
    let Some((key, payload)) = value
        .as_object()
        .and_then(|object| object.into_iter().next())
    else {
        return Ok(None);
    };
    let event = match key.as_str() {
        "WorkspacesChanged" => SessionEvent::Workspaces(serde_json::from_value(
            payload["workspaces"].clone(),
        )?),
        "WorkspaceActivated" => {
            let event: WorkspaceActivatedEvent = serde_json::from_value(payload.clone())?;
            SessionEvent::WorkspaceActivated {
                id: event.id,
                focused: event.focused,
            }
        }
        "WindowsChanged" => {
            let windows: Vec<crate::session::SessionWindow> =
                serde_json::from_value(payload["windows"].clone())?;
            SessionEvent::Windows(
                windows
                    .into_iter()
                    .map(|window| (window.id, window))
                    .collect(),
            )
        }
        "WindowOpenedOrChanged" => SessionEvent::WindowOpenedOrChanged(serde_json::from_value(
            payload["window"].clone(),
        )?),
        "WindowClosed" => SessionEvent::WindowClosed(
            serde_json::from_value::<WindowClosedEvent>(payload.clone())?.id,
        ),
        "WindowFocusChanged" => SessionEvent::WindowFocusChanged(
            serde_json::from_value::<WindowFocusChangedEvent>(payload.clone())?.id,
        ),
        _ => return Ok(None),
    };
    Ok(Some(event))
}

/// Send one request line and return niri's Ok payload. The error path
/// keeps niri's own answer text, which is what makes IPC failures
/// diagnosable in the journal. The envelope lives in the platform
/// crate; this stays the app's seam (nothing above this module
/// mentions niri).
pub(crate) fn request(request: Value) -> Result<Value> {
    kuma_platform::niri::request(&request)
}

/// Send one action request and check niri's reply envelope.
fn send_action(action: Value) -> Result<()> {
    request(json!({"Action": action}))?;
    Ok(())
}

/// Focus a workspace by its internal id: the bar's click action.
pub fn focus_workspace(id: u64) -> Result<()> {
    send_action(json!({"FocusWorkspace": {"reference": {"Id": id}}}))
}

/// Focus a workspace by its 1-based number, the MSG CLI path, where the
/// human (or keybind) speaks in workspace numbers, not niri's internal ids.
pub fn focus_workspace_index(index: u32) -> Result<()> {
    send_action(json!({"FocusWorkspace": {"reference": {"Index": index}}}))
}

/// Focus one window: the dock's click action.
pub fn focus_window(id: u64) -> Result<()> {
    send_action(json!({"FocusWindow": {"id": id}}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_workspaces_changed_event() {
        let event = parse_event(
            r#"{"WorkspacesChanged":{"workspaces":[{"id":4,"idx":3,"name":null,"output":"eDP-1","is_urgent":false,"is_active":false,"is_focused":false,"active_window_id":null}]}}"#,
        )
        .unwrap();
        match event {
            Some(SessionEvent::Workspaces(workspaces)) => {
                assert_eq!(workspaces.len(), 1);
                assert_eq!(workspaces[0].id, 4);
                assert_eq!(workspaces[0].idx, 3);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_focus_and_close_events() {
        assert!(matches!(
            parse_event(r#"{"WindowFocusChanged":{"id":null}}"#).unwrap(),
            Some(SessionEvent::WindowFocusChanged(None))
        ));
        assert!(matches!(
            parse_event(r#"{"WindowClosed":{"id":12}}"#).unwrap(),
            Some(SessionEvent::WindowClosed(12))
        ));
    }

    #[test]
    fn ignores_unknown_events() {
        assert!(matches!(
            parse_event(r#"{"CastStopped":{"stream_id":9}}"#).unwrap(),
            None
        ));
        assert!(parse_event("not json").is_err());
    }

    #[test]
    fn reshaped_payloads_are_parse_errors_the_stream_skips() {
        // A missing required field in a known event (say, a future
        // niri reshaping its payloads) parses as an error; the stream
        // loop logs and moves on instead of ending the mirror. The
        // niri #4044 class of breakage, one new event variant killing
        // every strict client, must not happen here twice.
        assert!(parse_event(r#"{"WorkspacesChanged":{"workspaces":{}}}"#).is_err());
        assert!(parse_event(r#"{"WindowClosed":{}}"#).is_err());
    }
}
