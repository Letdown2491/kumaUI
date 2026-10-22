//! The compositor-neutral mirror of session state: workspaces, windows,
//! and keyboard focus, however the running compositor spells them.
//! Widgets read this type and never the adapter underneath; `niri.rs`
//! is the first adapter, and sway and hyprland slot in beside it (the
//! adapters are picked at startup by which IPC socket exists).

use std::{collections::HashMap, future::Future, time::Duration};

use anyhow::Result;
use gpui::{App, AppContext, Entity};
use log::{debug, error, warn};
use serde::Deserialize;
use smol::channel::{Sender, unbounded};

/// How long a terminated stream waits before reopening. Long enough
/// that a restarting compositor is not hammered, short enough that a
/// bar goes stale for seconds rather than for the rest of the session.
const RECONNECT_AFTER: Duration = Duration::from_secs(3);

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

/// A screencast the compositor knows is running: a portal screen
/// share, a wlr-screencopy capture. Niri reports these first-party
/// (26.04), which is the honest signal the shell half of #66 asked
/// for; no pipewire-graph guessing.
#[derive(Clone, Debug, Deserialize)]
pub struct Cast {
    /// Stream ID: uniquely identifies the cast; the state's key.
    pub stream_id: u64,
    /// Session ID: the stop unit. One session can carry several
    /// streams, and only PipeWire sessions answer a stop.
    pub session_id: u64,
    pub kind: CastKind,
    pub target: CastTarget,
    /// False while the consumer paused the stream (an OBS scene
    /// switch, for one). A paused cast is still a cast.
    #[serde(default)]
    pub is_active: bool,
}

/// How the cast is fed. Niri's wire names are "PipeWire" and
/// "WlrScreencopy"; serde's unit-variant spelling matches as is.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub enum CastKind {
    /// A portal screencast, typically xdg-desktop-portal-gnome.
    PipeWire,
    /// A direct capture: wf-recorder and friends.
    WlrScreencopy,
}

impl CastKind {
    /// Whether the compositor accepts a stop for this kind: the
    /// sharing indicator's click only ends PipeWire sessions.
    pub fn stoppable(self) -> bool {
        matches!(self, CastKind::PipeWire)
    }
}

/// What the cast is pointed at. Window casts carry niri's window id,
/// which the bar resolves to a title against the session mirror.
#[derive(Clone, Debug, Deserialize)]
pub enum CastTarget {
    Nothing {},
    Output { name: String },
    Window { id: u64 },
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
    Casts(Vec<Cast>),
    CastStartedOrChanged(Cast),
    CastStopped(u64),
}

#[derive(Default)]
pub struct SessionState {
    workspaces: Vec<Workspace>,
    windows: HashMap<u64, SessionWindow>,
    focused_window: Option<u64>,
    casts: HashMap<u64, Cast>,
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

    /// The live screencasts, in stable order (by stream id): what the
    /// bar's sharing indicator reads.
    pub fn casts(&self) -> Vec<Cast> {
        let mut casts: Vec<Cast> = self.casts.values().cloned().collect();
        casts.sort_by_key(|cast| cast.stream_id);
        casts
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
            SessionEvent::Casts(casts) => {
                self.casts = casts
                    .into_iter()
                    .map(|cast| (cast.stream_id, cast))
                    .collect();
            }
            SessionEvent::CastStartedOrChanged(cast) => {
                self.casts.insert(cast.stream_id, cast);
            }
            SessionEvent::CastStopped(stream_id) => {
                self.casts.remove(&stream_id);
            }
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

/// Spawn an adapter's event stream and apply what it reports to the
/// mirror. Both halves of the plumbing live here; only the stream
/// function is the adapter's own. `name` is what a broken stream gets
/// to be called in the log.
pub(crate) fn mirror<Stream, Fut>(
    state: &Entity<SessionState>,
    name: &'static str,
    cx: &mut App,
    stream: Stream,
) where
    Stream: Fn(Sender<SessionEvent>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let state = state.downgrade();
    let (event_tx, event_rx) = unbounded::<SessionEvent>();

    // One connection is one attempt, not the mirror's life: a
    // compositor restart or a dropped socket ends the attempt, and the
    // loop opens a fresh one. State survives the gap and heals on
    // reconnect, because every attempt re-seeds it (niri snapshots on
    // connect, sway fetches after subscribe).
    cx.background_spawn(async move {
        loop {
            if let Err(err) = stream(event_tx.clone()).await {
                error!("{name} event stream terminated: {err:#}; retrying in {RECONNECT_AFTER:?}");
            } else {
                warn!("{name} event stream ended without error; retrying in {RECONNECT_AFTER:?}");
            }
            smol::Timer::after(RECONNECT_AFTER).await;
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

/// Route a command to whichever adapter's socket exists right now. The
/// probe happens at call time on purpose: the environment owns this
/// fact, and asking is cheaper than caching it wrong. niri first is a
/// target-platform preference, not semantics; the two sockets never
/// coexist in a real session.
fn with_adapter<T>(
    niri: impl FnOnce() -> Result<T>,
    sway: impl FnOnce() -> Result<T>,
) -> Result<T> {
    if crate::niri::socket_path().is_ok() {
        niri()
    } else {
        sway()
    }
}

/// Focus a workspace by its internal id: the bar's click action.
pub fn focus_workspace(id: u64) -> Result<()> {
    with_adapter(
        || crate::niri::focus_workspace(id),
        || crate::sway::focus_workspace(id),
    )
}

/// Focus a workspace by its 1-based number: the MSG CLI path, where the
/// human (or keybind) speaks in workspace numbers, not internal ids.
pub fn focus_workspace_index(index: u32) -> Result<()> {
    with_adapter(
        || crate::niri::focus_workspace_index(index),
        || crate::sway::focus_workspace_index(index),
    )
}

/// Focus one window: the dock's click action.
pub fn focus_window(id: u64) -> Result<()> {
    with_adapter(
        || crate::niri::focus_window(id),
        || crate::sway::focus_window(id),
    )
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

    fn cast(stream_id: u64, session_id: u64) -> Cast {
        Cast {
            stream_id,
            session_id,
            kind: CastKind::PipeWire,
            target: CastTarget::Output {
                name: "eDP-1".into(),
            },
            is_active: true,
        }
    }

    #[test]
    fn casts_replace_upsert_and_stop() {
        let mut state = SessionState::default();
        state.apply(SessionEvent::Casts(vec![cast(1, 10), cast(2, 10)]));
        assert_eq!(state.casts().len(), 2);
        state.apply(SessionEvent::CastStopped(1));
        assert_eq!(state.casts().len(), 1);
        // an upsert of the survivor's stream id refreshes it in place
        state.apply(SessionEvent::CastStartedOrChanged(cast(2, 10)));
        assert_eq!(state.casts().len(), 1);
        // the full replacement drops what it does not name
        state.apply(SessionEvent::Casts(vec![cast(3, 11)]));
        assert_eq!(state.casts().len(), 1);
        assert_eq!(state.casts()[0].session_id, 11);
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
