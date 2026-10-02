use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
};

use anyhow::{Context as _, Result, bail};
use gpui::{App, AppContext, Entity};
use log::error;
use serde::Deserialize;
use serde_json::{Value, json};
use smol::channel::{Sender, unbounded};

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Workspace {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub idx: u8,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub is_focused: bool,
    #[serde(default)]
    pub is_urgent: bool,
    #[serde(default)]
    pub active_window_id: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[allow(dead_code)]
pub struct NiriWindow {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<u64>,
    #[serde(default)]
    pub is_focused: bool,
    #[serde(default)]
    pub is_urgent: bool,
}

#[derive(Debug, Deserialize)]
pub enum Event {
    Workspaces(Vec<Workspace>),
    WorkspaceActivated { id: u64, focused: bool },
    Windows(HashMap<u64, NiriWindow>),
    WindowOpenedOrChanged(NiriWindow),
    WindowClosed(u64),
    WindowFocusChanged(Option<u64>),
}

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

#[derive(Default)]
pub struct NiriState {
    workspaces: Vec<Workspace>,
    windows: HashMap<u64, NiriWindow>,
    focused_window: Option<u64>,
}

impl NiriState {
    pub fn focused_output(&self) -> Option<&str> {
        self.workspaces
            .iter()
            .find(|workspace| workspace.is_focused)
            .and_then(|workspace| workspace.output.as_deref())
    }

    pub fn workspaces_on(&self, output: &str) -> Vec<&Workspace> {
        let mut workspaces: Vec<_> = self
            .workspaces
            .iter()
            .filter(|workspace| workspace.output.as_deref() == Some(output))
            .collect();
        workspaces.sort_by_key(|workspace| workspace.idx);
        workspaces
    }

    pub fn focused_window(&self) -> Option<&NiriWindow> {
        let id = self.focused_window?;
        self.windows.get(&id)
    }

    /// All windows in a stable order (by id); the dock iterates them.
    pub fn windows(&self) -> Vec<&NiriWindow> {
        let mut windows: Vec<&NiriWindow> = self.windows.values().collect();
        windows.sort_by_key(|window| window.id);
        windows
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Workspaces(workspaces) => self.workspaces = workspaces,
            Event::WorkspaceActivated { id, focused } => {
                let output = self
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == id)
                    .and_then(|workspace| workspace.output.clone());
                for workspace in &mut self.workspaces {
                    if workspace.id == id {
                        workspace.is_active = true;
                        workspace.is_focused = focused || workspace.is_focused;
                    } else if workspace.output == output {
                        workspace.is_active = false;
                        if focused {
                            workspace.is_focused = false;
                        }
                    }
                }
            }
            Event::Windows(windows) => {
                self.windows = windows;
                self.focused_window = self
                    .windows
                    .values()
                    .find(|window| window.is_focused)
                    .map(|window| window.id);
            }
            Event::WindowOpenedOrChanged(window) => {
                if window.is_focused {
                    self.focused_window = Some(window.id);
                }
                self.windows.insert(window.id, window);
            }
            Event::WindowClosed(id) => {
                self.windows.remove(&id);
                if self.focused_window == Some(id) {
                    self.focused_window = None;
                }
            }
            Event::WindowFocusChanged(id) => self.focused_window = id,
        }
    }
}

pub fn focus_workspace(id: u64) -> Result<()> {
    let path = socket_path()?;
    let stream = UnixStream::connect(&path).context("failed to connect to niri IPC socket")?;

    let request = json!({"Action": {"FocusWorkspace": {"reference": {"Id": id}}}});
    let mut writer = &stream;
    writer.write_all(request.to_string().as_bytes())?;
    writer.write_all(b"\n")?;

    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    if !reply.starts_with("{\"Ok\"") {
        bail!("focus-workspace request failed: {reply}");
    }
    Ok(())
}

/// Focus a workspace by its 1-based number, the MSG CLI path, where the
/// human (or keybind) speaks in workspace numbers, not niri's internal ids.
pub fn focus_workspace_index(index: u32) -> Result<()> {
    let path = socket_path()?;
    let stream = UnixStream::connect(&path).context("failed to connect to niri IPC socket")?;

    let request = json!({"Action": {"FocusWorkspace": {"reference": {"Index": index}}}});
    let mut writer = &stream;
    writer.write_all(request.to_string().as_bytes())?;
    writer.write_all(b"\n")?;

    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    if !reply.starts_with("{\"Ok\"") {
        bail!("focus-workspace request failed: {reply}");
    }
    Ok(())
}

/// Focus one window: the dock's click action.
pub fn focus_window(id: u64) -> Result<()> {
    let path = socket_path()?;
    let stream = UnixStream::connect(&path).context("failed to connect to niri IPC socket")?;

    let request = json!({"Action": {"FocusWindow": {"id": id}}});
    let mut writer = &stream;
    writer.write_all(request.to_string().as_bytes())?;
    writer.write_all(b"\n")?;

    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    if !reply.starts_with("{\"Ok\"") {
        bail!("focus-window request failed: {reply}");
    }
    Ok(())
}

pub fn connect(state: &Entity<NiriState>, cx: &mut App) {
    let state = state.downgrade();
    let (event_tx, event_rx) = unbounded::<Event>();

    cx.background_spawn(async move {
        if let Err(err) = run_event_stream(event_tx).await {
            error!("niri event stream terminated: {err:#}");
        }
    })
    .detach();

    cx.spawn(async move |cx| {
        while let Ok(event) = event_rx.recv().await {
            if state
                .update(cx, |state, cx| {
                    state.apply(event);
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
}

fn socket_path() -> Result<PathBuf> {
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

async fn run_event_stream(event_tx: Sender<Event>) -> Result<()> {
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

fn parse_event(line: &str) -> Result<Option<Event>> {
    let value: Value = serde_json::from_str(line)?;
    let Some((key, payload)) = value
        .as_object()
        .and_then(|object| object.into_iter().next())
    else {
        return Ok(None);
    };
    let event = match key.as_str() {
        "WorkspacesChanged" => {
            Event::Workspaces(serde_json::from_value(payload["workspaces"].clone())?)
        }
        "WorkspaceActivated" => {
            let event: WorkspaceActivatedEvent = serde_json::from_value(payload.clone())?;
            Event::WorkspaceActivated {
                id: event.id,
                focused: event.focused,
            }
        }
        "WindowsChanged" => {
            let windows: Vec<NiriWindow> = serde_json::from_value(payload["windows"].clone())?;
            Event::Windows(
                windows
                    .into_iter()
                    .map(|window| (window.id, window))
                    .collect(),
            )
        }
        "WindowOpenedOrChanged" => {
            Event::WindowOpenedOrChanged(serde_json::from_value(payload["window"].clone())?)
        }
        "WindowClosed" => {
            Event::WindowClosed(serde_json::from_value::<WindowClosedEvent>(payload.clone())?.id)
        }
        "WindowFocusChanged" => Event::WindowFocusChanged(
            serde_json::from_value::<WindowFocusChangedEvent>(payload.clone())?.id,
        ),
        _ => return Ok(None),
    };
    Ok(Some(event))
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
            Some(Event::Workspaces(workspaces)) => {
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
            Some(Event::WindowFocusChanged(None))
        ));
        assert!(matches!(
            parse_event(r#"{"WindowClosed":{"id":12}}"#).unwrap(),
            Some(Event::WindowClosed(12))
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
    fn workspace_activation_updates_state() {
        let mut state = NiriState::default();
        state.apply(Event::Workspaces(vec![
            Workspace {
                id: 1,
                idx: 1,
                name: None,
                output: Some("eDP-1".into()),
                is_active: true,
                is_focused: true,
                is_urgent: false,
                active_window_id: None,
            },
            Workspace {
                id: 2,
                idx: 2,
                name: None,
                output: Some("eDP-1".into()),
                is_active: false,
                is_focused: false,
                is_urgent: false,
                active_window_id: None,
            },
        ]));
        state.apply(Event::WorkspaceActivated {
            id: 2,
            focused: true,
        });
        assert!(
            state
                .workspaces
                .iter()
                .find(|w| w.id == 2)
                .unwrap()
                .is_focused
        );
        assert!(
            !state
                .workspaces
                .iter()
                .find(|w| w.id == 1)
                .unwrap()
                .is_active
        );
        assert_eq!(state.focused_output(), Some("eDP-1"));
    }

    fn workspace(id: u64, idx: u8, output: &str, active: bool, focused: bool) -> Workspace {
        Workspace {
            id,
            idx,
            name: None,
            output: Some(output.into()),
            is_active: active,
            is_focused: focused,
            is_urgent: false,
            active_window_id: None,
        }
    }

    fn window(id: u64, workspace_id: u64, focused: bool) -> NiriWindow {
        NiriWindow {
            id,
            title: Some(format!("win-{id}")),
            app_id: None,
            workspace_id: Some(workspace_id),
            is_focused: focused,
            is_urgent: false,
        }
    }

    #[test]
    fn activation_on_another_output_keeps_focus_where_it_was() {
        let mut state = NiriState::default();
        state.apply(Event::Workspaces(vec![
            workspace(1, 1, "eDP-1", true, true),
            workspace(2, 1, "DP-1", true, true),
        ]));
        // DP-1 activates a workspace without taking keyboard focus
        state.apply(Event::WorkspaceActivated {
            id: 2,
            focused: false,
        });
        assert!(
            state
                .workspaces
                .iter()
                .find(|w| w.id == 1)
                .unwrap()
                .is_focused
        );
        assert!(
            state
                .workspaces
                .iter()
                .find(|w| w.id == 2)
                .unwrap()
                .is_active
        );
        assert_eq!(state.focused_output(), Some("eDP-1"));
    }

    #[test]
    fn focused_activation_switches_focus_within_an_output_only() {
        let mut state = NiriState::default();
        state.apply(Event::Workspaces(vec![
            workspace(1, 1, "eDP-1", true, true),
            workspace(2, 2, "eDP-1", false, false),
            workspace(3, 1, "DP-1", true, true),
        ]));
        state.apply(Event::WorkspaceActivated {
            id: 2,
            focused: true,
        });
        assert!(
            !state
                .workspaces
                .iter()
                .find(|w| w.id == 1)
                .unwrap()
                .is_focused
        );
        assert!(
            !state
                .workspaces
                .iter()
                .find(|w| w.id == 1)
                .unwrap()
                .is_active
        );
        assert!(
            state
                .workspaces
                .iter()
                .find(|w| w.id == 2)
                .unwrap()
                .is_focused
        );
        // the other output's focus is untouched
        assert!(
            state
                .workspaces
                .iter()
                .find(|w| w.id == 3)
                .unwrap()
                .is_focused
        );
    }

    #[test]
    fn activation_of_unknown_workspace_is_a_no_op() {
        let mut state = NiriState::default();
        state.apply(Event::Workspaces(vec![workspace(
            1, 1, "eDP-1", true, true,
        )]));
        state.apply(Event::WorkspaceActivated {
            id: 99,
            focused: true,
        });
        assert!(
            state
                .workspaces
                .iter()
                .find(|w| w.id == 1)
                .unwrap()
                .is_focused
        );
        assert!(
            state
                .workspaces
                .iter()
                .find(|w| w.id == 1)
                .unwrap()
                .is_active
        );
    }

    #[test]
    fn workspaces_on_filters_and_sorts_by_index() {
        let mut state = NiriState::default();
        state.apply(Event::Workspaces(vec![
            workspace(3, 3, "eDP-1", false, false),
            workspace(1, 1, "eDP-1", true, true),
            workspace(2, 1, "DP-1", true, false),
        ]));
        let on_edp: Vec<u64> = state.workspaces_on("eDP-1").iter().map(|w| w.id).collect();
        assert_eq!(on_edp, vec![1, 3]);
        assert!(state.workspaces_on("HDMI-0").is_empty());
    }

    #[test]
    fn windows_snapshot_replaces_and_refocuses() {
        let mut state = NiriState::default();
        state.apply(Event::Windows(HashMap::from([
            (1, window(1, 1, false)),
            (2, window(2, 1, true)),
        ])));
        assert_eq!(state.focused_window().unwrap().id, 2);

        // a full snapshot replaces the map and re-derives focus
        state.apply(Event::Windows(HashMap::from([(1, window(1, 1, true))])));
        assert_eq!(state.focused_window().unwrap().id, 1);
        assert!(state.windows.get(&2).is_none());
    }

    #[test]
    fn window_open_and_focus_changes_track_focus() {
        let mut state = NiriState::default();
        state.apply(Event::WindowOpenedOrChanged(window(1, 1, true)));
        assert_eq!(state.focused_window().unwrap().id, 1);
        // an unfocused open must not steal focus
        state.apply(Event::WindowOpenedOrChanged(window(2, 1, false)));
        assert_eq!(state.focused_window().unwrap().id, 1);

        state.apply(Event::WindowFocusChanged(Some(2)));
        assert_eq!(state.focused_window().unwrap().id, 2);
        state.apply(Event::WindowFocusChanged(None));
        assert!(state.focused_window().is_none());
    }

    #[test]
    fn closing_the_focused_window_clears_focus() {
        let mut state = NiriState::default();
        state.apply(Event::Windows(HashMap::from([
            (1, window(1, 1, false)),
            (2, window(2, 1, true)),
        ])));
        state.apply(Event::WindowClosed(1)); // unrelated close keeps focus
        assert_eq!(state.focused_window().unwrap().id, 2);
        state.apply(Event::WindowClosed(2));
        assert!(state.focused_window().is_none());
        assert!(state.windows.get(&2).is_none());
    }
}
