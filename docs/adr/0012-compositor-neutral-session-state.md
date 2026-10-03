# ADR-0012: Session state is compositor-neutral; niri is an adapter

Status: accepted (2026-10-03)

## Context

The shell's runtime floor is already compositor-generic: `wlr-layer-shell`,
`ext-session-lock`, logind, PAM, DBus. The smoke harness has run the whole
surface lifecycle under sway since its first day (niri cannot start in the
test container), and the shell survives there; only the niri-fed widgets
(workspaces, window title, dock's window awareness) go blank, because
`NiriState` and the focus commands were welded to niri's IPC. Making those
widgets work on sway and hyprland starts with naming the seam.

## Decision

The mirror of session state lives in `session.rs` and speaks no
compositor's name:

- `SessionState` (workspaces, windows, focused ids) with the accessors
  the widgets already used, and `apply(SessionEvent)`.
- `SessionEvent`, the six event kinds every desktop compositor can
  produce (workspace list and activation, window snapshot, open, close,
  focus change).
- `connect(state, cx)` picks an adapter at startup by which IPC socket
  exists, and the focus commands (`session.rs::focus_workspace`,
  `focus_workspace_index`, `focus_window`) route to the active adapter.

`niri.rs` keeps only niri's side: socket discovery, the event stream, the
JSON translation into `SessionEvent`, and its action requests. The niri
JSON shapes deserialize straight into the neutral types, so the adapter
is translation, not duplication. Widgets, dock, and `SurfaceDeps` take
`Entity<SessionState>` and never import `niri`.

## Consequences

- A sway or hyprland adapter is one module plus a detection arm: it must
  produce the six events and answer the two commands, nothing else.
- Sway maps 1:1 (per-output numbered workspaces). Hyprland's workspace
  model (one global list, per-monitor current) needs a rendering
  decision in the workspace widget when its adapter lands.
- A session with no known compositor leaves the state empty; session-fed
  widgets should hide rather than render stale data.
- The state-apply tests moved with the state into `session.rs`; the
  niri-side tests cover only its wire translation.
