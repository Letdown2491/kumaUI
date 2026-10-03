# ADR-0003: Hand-rolled niri IPC types instead of the niri-ipc crate

Status: accepted (2026-10-01)

## Context

niri's IPC socket speaks JSON with a documented, backwards-compatible wire
format. The `niri-ipc` crate implements these types, but its version tracks
niri's release cycle, it pulls in niri's dependency tree, and the shell only
needs a small subset (workspaces, focused window, focus/close events, focus
action, connectivity of the event stream).

## Decision

Hand-roll the subset as serde structs in `niri.rs`. Unknown events and unknown
fields are ignored gracefully (serde defaults + first-key dispatch), per the
IPC backwards-compatibility contract in the niri wiki.

## Consequences

- No dependency on niri's release cycle; the shell works across niri versions
  as long as fields are added, never removed.
- New niri features (new events, new request replies) mean adding structs by
  hand. If the shell ever needs broad IPC coverage (e.g. full window layouts,
  screencast management), revisit: switching to the crate should be cheap
  because all wire parsing concentrates in `parse_event` + the request helpers.
- (2026-10-03) The same choice was made for sway's i3-style IPC in
  `sway.rs`: a hand-rolled 14-byte header and JSON subset, no `swayipc`
  crate, for the same reasons.
