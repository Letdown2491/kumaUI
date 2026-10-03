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
        if let Some(event) = parse_event(&line)? {
            event_tx.send(event).await?;
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

/// Send one action request and check niri's reply envelope.
fn send_action(action: Value) -> Result<()> {
    let path = socket_path()?;
    let stream = UnixStream::connect(&path).context("failed to connect to niri IPC socket")?;

    let request = json!({"Action": action});
    let mut writer = &stream;
    writer.write_all(request.to_string().as_bytes())?;
    writer.write_all(b"\n")?;

    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    if !reply.starts_with("{\"Ok\"") {
        bail!("niri action request failed: {reply}");
    }
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
}
