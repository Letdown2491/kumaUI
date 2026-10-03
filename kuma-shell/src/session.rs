//! The compositor-neutral mirror of session state: workspaces, windows,
//! and keyboard focus, however the running compositor spells them.
//! Widgets read this type and never the adapter underneath; `niri.rs`
//! is the first adapter, and sway and hyprland slot in beside it (the
//! adapters are picked at startup by which IPC socket exists).

use std::collections::HashMap;

use anyhow::Result;
use gpui::{App, Entity};
use log::debug;
use serde::Deserialize;

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
pub struct SessionWindow {
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

/// The compositor's session events, whatever compositor sent them.
/// Adapters translate their wire format into this enum; the state
/// applies it blind.
#[derive(Debug)]
pub enum SessionEvent {
    Workspaces(Vec<Workspace>),
    WorkspaceActivated { id: u64, focused: bool },
    Windows(HashMap<u64, SessionWindow>),
    WindowOpenedOrChanged(SessionWindow),
    WindowClosed(u64),
    WindowFocusChanged(Option<u64>),
}

#[derive(Default)]
pub struct SessionState {
    workspaces: Vec<Workspace>,
    windows: HashMap<u64, SessionWindow>,
    focused_window: Option<u64>,
}

impl SessionState {
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

    pub fn focused_window(&self) -> Option<&SessionWindow> {
        let id = self.focused_window?;
        self.windows.get(&id)
    }

    /// All windows in a stable order (by id); the dock iterates them.
    pub fn windows(&self) -> Vec<&SessionWindow> {
        let mut windows: Vec<&SessionWindow> = self.windows.values().collect();
        windows.sort_by_key(|window| window.id);
        windows
    }

    pub(crate) fn apply(&mut self, event: SessionEvent) {
        let before = self.summary();
        match event {
            SessionEvent::Workspaces(workspaces) => self.workspaces = workspaces,
            SessionEvent::WorkspaceActivated { id, focused } => {
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
            SessionEvent::Windows(windows) => {
                self.windows = windows;
                self.focused_window = self
                    .windows
                    .values()
                    .find(|window| window.is_focused)
                    .map(|window| window.id);
            }
            SessionEvent::WindowOpenedOrChanged(window) => {
                if window.is_focused {
                    self.focused_window = Some(window.id);
                }
                self.windows.insert(window.id, window);
            }
            SessionEvent::WindowClosed(id) => {
                self.windows.remove(&id);
                if self.focused_window == Some(id) {
                    self.focused_window = None;
                }
            }
            SessionEvent::WindowFocusChanged(id) => self.focused_window = id,
        }
        // transition-only: the mirror's own telemetry, and the smoke's
        // assertion surface. A summary that kept logging would spam
        // every alt-tab; a summary that never logs leaves nothing to
        // diagnose a dead adapter with.
        let after = self.summary();
        if after != before {
            log::info!(
                "session: workspaces {} (focused {}), windows {} (focused {})",
                after.0,
                after.1.map(|id| id.to_string()).unwrap_or_else(|| "none".into()),
                after.2,
                after.3.map(|id| id.to_string()).unwrap_or_else(|| "none".into()),
            );
        }
    }

    /// (workspace count, focused workspace id, window count, focused
    /// window id): what changed between two applies.
    fn summary(&self) -> (usize, Option<u64>, usize, Option<u64>) {
        (
            self.workspaces.len(),
            self.workspaces
                .iter()
                .find(|workspace| workspace.is_focused)
                .map(|workspace| workspace.id),
            self.windows.len(),
            self.focused_window,
        )
    }
}

/// Wire the session state to whatever compositor this session runs:
/// detect the adapter by its IPC socket, then feed the mirror. A
/// session with no known compositor just leaves the state empty; the
/// session-fed widgets hide rather than render stale data.
pub fn connect(state: &Entity<SessionState>, cx: &mut App) {
    if crate::niri::socket_path().is_ok() {
        log::info!("session: niri compositor detected");
        crate::niri::connect(state, cx);
    } else if crate::sway::socket_path().is_ok() {
        log::info!("session: sway compositor detected");
        crate::sway::connect(state, cx);
    } else {
        debug!("no compositor session source found; session widgets stay empty");
    }
}

/// Focus a workspace by its internal id: the bar's click action.
pub fn focus_workspace(id: u64) -> Result<()> {
    if crate::niri::socket_path().is_ok() {
        crate::niri::focus_workspace(id)
    } else {
        crate::sway::focus_workspace(id)
    }
}

/// Focus a workspace by its 1-based number: the MSG CLI path, where the
/// human (or keybind) speaks in workspace numbers, not internal ids.
pub fn focus_workspace_index(index: u32) -> Result<()> {
    if crate::niri::socket_path().is_ok() {
        crate::niri::focus_workspace_index(index)
    } else {
        crate::sway::focus_workspace_index(index)
    }
}

/// Focus one window: the dock's click action.
pub fn focus_window(id: u64) -> Result<()> {
    if crate::niri::socket_path().is_ok() {
        crate::niri::focus_window(id)
    } else {
        crate::sway::focus_window(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn window(id: u64, workspace_id: u64, focused: bool) -> SessionWindow {
        SessionWindow {
            id,
            title: Some(format!("win-{id}")),
            app_id: None,
            workspace_id: Some(workspace_id),
            is_focused: focused,
            is_urgent: false,
        }
    }

    #[test]
    fn workspace_activation_updates_state() {
        let mut state = SessionState::default();
        state.apply(SessionEvent::Workspaces(vec![
            workspace(1, 1, "eDP-1", true, true),
            workspace(2, 2, "eDP-1", false, false),
        ]));
        state.apply(SessionEvent::WorkspaceActivated {
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

    #[test]
    fn activation_on_another_output_keeps_focus_where_it_was() {
        let mut state = SessionState::default();
        state.apply(SessionEvent::Workspaces(vec![
            workspace(1, 1, "eDP-1", true, true),
            workspace(2, 1, "DP-1", true, true),
        ]));
        // DP-1 activates a workspace without taking keyboard focus
        state.apply(SessionEvent::WorkspaceActivated {
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
        let mut state = SessionState::default();
        state.apply(SessionEvent::Workspaces(vec![
            workspace(1, 1, "eDP-1", true, true),
            workspace(2, 2, "eDP-1", false, false),
            workspace(3, 1, "DP-1", true, true),
        ]));
        state.apply(SessionEvent::WorkspaceActivated {
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
        let mut state = SessionState::default();
        state.apply(SessionEvent::Workspaces(vec![workspace(
            1, 1, "eDP-1", true, true,
        )]));
        state.apply(SessionEvent::WorkspaceActivated {
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
        let mut state = SessionState::default();
        state.apply(SessionEvent::Workspaces(vec![
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
        let mut state = SessionState::default();
        state.apply(SessionEvent::Windows(HashMap::from([
            (1, window(1, 1, false)),
            (2, window(2, 1, true)),
        ])));
        assert_eq!(state.focused_window().unwrap().id, 2);

        // a full snapshot replaces the map and re-derives focus
        state.apply(SessionEvent::Windows(HashMap::from([
            (1, window(1, 1, true)),
        ])));
        assert_eq!(state.focused_window().unwrap().id, 1);
        assert!(state.windows.get(&2).is_none());
    }

    #[test]
    fn window_open_and_focus_changes_track_focus() {
        let mut state = SessionState::default();
        state.apply(SessionEvent::WindowOpenedOrChanged(window(1, 1, true)));
        assert_eq!(state.focused_window().unwrap().id, 1);
        // an unfocused open must not steal focus
        state.apply(SessionEvent::WindowOpenedOrChanged(window(2, 1, false)));
        assert_eq!(state.focused_window().unwrap().id, 1);

        state.apply(SessionEvent::WindowFocusChanged(Some(2)));
        assert_eq!(state.focused_window().unwrap().id, 2);
        state.apply(SessionEvent::WindowFocusChanged(None));
        assert!(state.focused_window().is_none());
    }

    #[test]
    fn closing_the_focused_window_clears_focus() {
        let mut state = SessionState::default();
        state.apply(SessionEvent::Windows(HashMap::from([
            (1, window(1, 1, false)),
            (2, window(2, 1, true)),
        ])));
        state.apply(SessionEvent::WindowClosed(1)); // unrelated close keeps focus
        assert_eq!(state.focused_window().unwrap().id, 2);
        state.apply(SessionEvent::WindowClosed(2));
        assert!(state.focused_window().is_none());
        assert!(state.windows.get(&2).is_none());
    }
}
