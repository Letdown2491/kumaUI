//! The sway adapter: translates sway's i3-style IPC socket into the
//! neutral session events and commands (`session.rs`). Nothing above
//! this module may mention sway.
//!
//! The wire protocol is the i3 framing: a 14-byte header (magic, u32
//! LE payload length, u32 LE message type; events have the high bit
//! set) followed by JSON. sway sends no state on subscribe, so the
//! adapter fetches an initial snapshot and then keeps the mirror
//! current from events; workspace changes refetch their snapshot
//! because sway's flags (visible, focused) are authoritative and the
//! events alone cannot clear a workspace the mirror already holds.

use std::{
    io::{BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
};

use anyhow::{Context as _, Result, bail};
use gpui::{App, Entity};
use serde::Deserialize;
use serde_json::Value;
use smol::channel::Sender;

use crate::session::{SessionEvent, SessionState, SessionWindow, Workspace};

const MAGIC: &[u8; 6] = b"i3-ipc";
const RUN_COMMAND: u32 = 0;
const GET_WORKSPACES: u32 = 1;
const SUBSCRIBE: u32 = 2;
const GET_TREE: u32 = 4;
const EVENT_WORKSPACE: u32 = 0x8000_0000;
const EVENT_WINDOW: u32 = 0x8000_0003;

pub fn connect(state: &Entity<SessionState>, cx: &mut App) {
    crate::session::mirror(state, "sway", cx, run_event_stream);
}

pub(crate) fn socket_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("SWAYSOCK") {
        return Ok(PathBuf::from(path));
    }
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    std::fs::read_dir(&runtime_dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("sway-ipc.") && name.ends_with(".sock"))
        })
        .context("no sway IPC socket found in XDG_RUNTIME_DIR")
}

/// One open sway IPC connection: writes go to the raw stream, reads
/// come through a persistent buffered reader (a per-read BufReader
/// would drop whatever it prefetched past the message it answered).
struct Ipc {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    /// Events read while a reply was expected, dispatched later.
    stashed: Vec<(u32, Value)>,
}

impl Ipc {
    fn connect() -> Result<Self> {
        let path = socket_path()?;
        let stream = UnixStream::connect(&path).context("failed to connect to sway IPC socket")?;
        let writer = stream.try_clone().context("failed to clone sway socket")?;
        Ok(Self {
            writer,
            reader: BufReader::new(stream),
            stashed: Vec::new(),
        })
    }

    fn write_message(&mut self, kind: u32, payload: &[u8]) -> Result<()> {
        let mut message = Vec::with_capacity(14 + payload.len());
        message.extend_from_slice(MAGIC);
        message.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        message.extend_from_slice(&kind.to_le_bytes());
        message.extend_from_slice(payload);
        self.writer.write_all(&message)?;
        self.writer.flush()?;
        Ok(())
    }

    fn read_message(&mut self) -> Result<(u32, Value)> {
        let mut header = [0u8; 14];
        self.reader.read_exact(&mut header)?;
        if &header[..6] != MAGIC {
            bail!("bad sway IPC magic");
        }
        let length = u32::from_le_bytes(header[6..10].try_into()?);
        let kind = u32::from_le_bytes(header[10..14].try_into()?);
        let mut payload = vec![0u8; length as usize];
        self.reader.read_exact(&mut payload)?;
        let value = serde_json::from_slice(&payload).context("bad sway IPC payload")?;
        Ok((kind, value))
    }

    /// Send a request and wait out its reply. Events that arrive in
    /// the meantime are stashed and come back from `next_event`.
    fn request(&mut self, kind: u32, payload: &[u8]) -> Result<Value> {
        self.write_message(kind, payload)?;
        loop {
            let (message_kind, value) = self.read_message()?;
            if message_kind & 0x8000_0000 != 0 {
                self.stashed.push((message_kind, value));
                continue;
            }
            return Ok(value);
        }
    }

    /// The next event to dispatch, popping stashed ones first.
    fn next_event(&mut self) -> Result<(u32, Value)> {
        if let Some(stashed) = self.stashed.pop() {
            return Ok(stashed);
        }
        loop {
            let (kind, value) = self.read_message()?;
            if kind & 0x8000_0000 != 0 {
                return Ok((kind, value));
            }
        }
    }
}

fn run_command(ipc: &mut Ipc, command: &str) -> Result<()> {
    let reply = ipc.request(RUN_COMMAND, command.as_bytes())?;
    let replies = reply.as_array().context("sway run command reply was not a list")?;
    if replies
        .iter()
        .all(|entry| entry["success"].as_bool().unwrap_or(false))
    {
        Ok(())
    } else {
        bail!("sway rejected {command:?}: {replies:?}")
    }
}

fn fetch_workspaces(ipc: &mut Ipc) -> Result<Vec<Workspace>> {
    let reply = ipc.request(GET_WORKSPACES, b"")?;
    let workspaces: Vec<SwayWorkspace> =
        serde_json::from_value(reply).context("bad sway workspace list")?;
    Ok(workspaces.iter().map(workspace_to_session).collect())
}

/// All windows in the tree, walking outputs, workspaces, and the
/// floating layer for leaf containers.
fn fetch_windows(ipc: &mut Ipc) -> Result<std::collections::HashMap<u64, SessionWindow>> {
    let reply = ipc.request(GET_TREE, b"")?;
    let tree: SwayContainer = serde_json::from_value(reply).context("bad sway tree")?;
    Ok(tree
        .windows()
        .into_iter()
        .map(|window| (window.id, window))
        .collect())
}

async fn run_event_stream(event_tx: Sender<SessionEvent>) -> Result<()> {
    let mut ipc = Ipc::connect()?;
    let subscribed = ipc.request(SUBSCRIBE, br#"["workspace","window"]"#)?;
    if !subscribed["success"].as_bool().unwrap_or(false) {
        bail!("sway rejected the event subscription");
    }

    // sway sends no state on subscribe: seed the mirror with a full
    // snapshot so a shell starting mid-session sees the session
    event_tx
        .send(SessionEvent::Workspaces(fetch_workspaces(&mut ipc)?))
        .await?;
    event_tx
        .send(SessionEvent::Windows(fetch_windows(&mut ipc)?))
        .await?;

    loop {
        let (kind, value) = ipc.next_event()?;
        match kind {
            EVENT_WORKSPACE => {
                // the workspace object's flags are the truth: refetch
                // rather than patch, so visible and focused stay exact
                let workspaces = fetch_workspaces(&mut ipc)?;
                event_tx.send(SessionEvent::Workspaces(workspaces)).await?;
            }
            EVENT_WINDOW => {
                if let Some(event) = window_event(&value) {
                    event_tx.send(event).await?;
                }
            }
            _ => {}
        }
    }
}

#[derive(Deserialize)]
struct SwayWorkspace {
    id: u64,
    #[serde(default)]
    num: i32,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    visible: bool,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    urgent: bool,
}

fn workspace_to_session(workspace: &SwayWorkspace) -> Workspace {
    Workspace {
        id: workspace.id,
        // sway marks unnumbered workspaces with num = -1; the neutral
        // index is 1-based, and such a workspace carries its own name
        idx: workspace.num.clamp(0, u8::MAX as i32) as u8,
        name: workspace.name.clone(),
        output: workspace.output.clone(),
        is_active: workspace.visible,
        is_focused: workspace.focused,
        is_urgent: workspace.urgent,
        active_window_id: None,
    }
}

#[derive(Deserialize)]
struct WindowProperties {
    #[serde(default)]
    class: Option<String>,
}

#[derive(Deserialize)]
struct SwayContainer {
    id: u64,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    window_properties: Option<WindowProperties>,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    urgent: bool,
    #[serde(default)]
    nodes: Vec<SwayContainer>,
    #[serde(default)]
    floating_nodes: Vec<SwayContainer>,
}

impl SwayContainer {
    /// A leaf window: something with an app id (native Wayland) or
    /// window properties (XWayland). Containers, layouts, and the
    /// internal nodes have neither.
    fn is_window(&self) -> bool {
        self.app_id.is_some() || self.window_properties.is_some()
    }

    fn to_session(&self) -> SessionWindow {
        SessionWindow {
            id: self.id,
            title: self.name.clone(),
            // sway leaves app_id empty on XWayland windows; the WM
            // class is what the dock groups by, niri-style
            app_id: self
                .app_id
                .clone()
                .or_else(|| self.window_properties.as_ref().and_then(|p| p.class.clone())),
            workspace_id: None,
            is_focused: self.focused,
            is_urgent: self.urgent,
        }
    }

    fn windows(&self) -> Vec<SessionWindow> {
        let mut windows: Vec<SessionWindow> = Vec::new();
        let mut stack: Vec<&SwayContainer> = vec![self];
        while let Some(node) = stack.pop() {
            if node.is_window() {
                windows.push(node.to_session());
            }
            stack.extend(node.nodes.iter().chain(node.floating_nodes.iter()));
        }
        windows
    }
}

/// A window event's container maps straight onto the neutral events:
/// new, title, and urgent carry the full window, close and focus only
/// need the id. Everything else (move, fullscreen, floating) changes
/// nothing the widgets read.
fn window_event(value: &Value) -> Option<SessionEvent> {
    let change = value["change"].as_str()?;
    let container: SwayContainer =
        serde_json::from_value(value["container"].clone()).ok()?;
    match change {
        "new" | "title" | "urgent" => {
            Some(SessionEvent::WindowOpenedOrChanged(container.to_session()))
        }
        "close" => Some(SessionEvent::WindowClosed(container.id)),
        "focus" => Some(SessionEvent::WindowFocusChanged(Some(container.id))),
        _ => None,
    }
}

/// Focus a workspace by the neutral state's id: sway addresses
/// workspaces by number or name, so resolve the id first.
pub fn focus_workspace(id: u64) -> Result<()> {
    let mut ipc = Ipc::connect()?;
    let reply = ipc.request(GET_WORKSPACES, b"")?;
    let workspaces: Vec<SwayWorkspace> =
        serde_json::from_value(reply).context("bad sway workspace list")?;
    let workspace = workspaces
        .iter()
        .find(|workspace| workspace.id == id)
        .context("no such sway workspace")?;
    if workspace.num >= 0 {
        run_command(&mut ipc, &format!("workspace number {}", workspace.num))
    } else {
        let name = workspace
            .name
            .as_deref()
            .context("sway workspace has neither number nor name")?;
        run_command(&mut ipc, &format!("workspace \"{name}\""))
    }
}

/// Focus a workspace by its 1-based number: the MSG CLI path.
pub fn focus_workspace_index(index: u32) -> Result<()> {
    let mut ipc = Ipc::connect()?;
    run_command(&mut ipc, &format!("workspace number {index}"))
}

/// Focus one window: the dock's click action.
pub fn focus_window(id: u64) -> Result<()> {
    let mut ipc = Ipc::connect()?;
    run_command(&mut ipc, &format!("[con_id={id}] focus"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn workspace_maps_visible_and_focused() {
        let workspace: SwayWorkspace = serde_json::from_value(json!({
            "id": 7, "num": 3, "name": "3", "output": "HEADLESS-1",
            "visible": true, "focused": true, "urgent": false
        }))
        .unwrap();
        let workspace = workspace_to_session(&workspace);
        assert_eq!(workspace.idx, 3);
        assert_eq!(workspace.output.as_deref(), Some("HEADLESS-1"));
        assert!(workspace.is_active);
        assert!(workspace.is_focused);
    }

    #[test]
    fn window_event_maps_open_close_and_focus() {
        let container = json!({
            "id": 12, "name": "btop", "app_id": "foot", "focused": true,
            "urgent": false, "type": "con"
        });
        let Some(SessionEvent::WindowOpenedOrChanged(window)) =
            window_event(&json!({"change": "new", "container": container}))
        else {
            panic!("expected an open");
        };
        assert_eq!(window.id, 12);
        assert_eq!(window.title.as_deref(), Some("btop"));
        assert_eq!(window.app_id.as_deref(), Some("foot"));
        assert!(window.is_focused);

        let event = window_event(&json!({"change": "focus", "container": container})).unwrap();
        assert!(matches!(event, SessionEvent::WindowFocusChanged(Some(12))));

        let event = window_event(&json!({"change": "close", "container": container})).unwrap();
        assert!(matches!(event, SessionEvent::WindowClosed(12)));

        // a move changes nothing the widgets read
        assert!(window_event(&json!({"change": "move", "container": container})).is_none());
    }

    #[test]
    fn xwayland_windows_fall_back_to_the_wm_class() {
        let container: SwayContainer = serde_json::from_value(json!({
            "id": 3, "name": "Chromium", "app_id": null,
            "window_properties": {"class": "chromium"},
            "focused": false, "urgent": false, "type": "con"
        }))
        .unwrap();
        let window = container.to_session();
        assert_eq!(window.app_id.as_deref(), Some("chromium"));
    }

    #[test]
    fn tree_walk_collects_leaf_windows_only() {
        let tree: SwayContainer = serde_json::from_value(json!({
            "id": 1, "name": "root", "type": "root",
            "nodes": [{
                "id": 2, "name": "HEADLESS-1", "type": "output",
                "nodes": [{
                    "id": 3, "name": "1", "type": "workspace",
                    "nodes": [
                        {"id": 4, "name": "term", "app_id": "foot", "type": "con",
                         "focused": true, "urgent": false},
                        // a split container holding two more windows
                        {"id": 5, "type": "con", "nodes": [
                            {"id": 6, "name": "editor", "app_id": "foot", "type": "con"},
                            {"id": 7, "name": "web", "app_id": "foot", "type": "con"}
                        ]}
                    ],
                    "floating_nodes": [
                        {"id": 8, "name": "calc", "app_id": "foot", "type": "floating_con"}
                    ]
                }]
            }]
        }))
        .unwrap();
        let windows = tree.windows();
        let mut ids: Vec<u64> = windows.iter().map(|w| w.id).collect();
        ids.sort_unstable();
        // the root, output, workspace, and split container are not windows
        assert_eq!(ids, vec![4, 6, 7, 8]);
        assert!(windows.iter().find(|w| w.id == 4).unwrap().is_focused);
    }
}
