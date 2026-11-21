# ADR-0018: Output hotplug is a hint-and-reconcile, not a compositor event

Status: accepted (2026-11-20)

## Context

Multi-monitor phase one (#111) wants event-driven hotplug: when a monitor
arrives or leaves, the shell's per-output surfaces (bar, wallpaper, dock)
follow without waiting for a poll. But niri's IPC event stream has no
output event in any version kumaOS has shipped (verified across v25.08
and v26.04; the investigation lives in
`docs/research/niri-output-events.md`). Sway, by contrast, defines a
native `output` event. The session layer (ADR-0012) needs one
compositor-neutral story that both adapters can produce.

## Decision

Output hotplug rides one new session event, produced two ways:

- `SessionEvent::OutputsChanged`: emitted when the set of usable outputs
  changes. Niri derives it from `WorkspacesChanged` behind a set-compare
  gate: hotplug always reassigns workspaces (niri gives every mapped
  monitor at least one workspace, and a disabled output carries none),
  so the union of `Workspace.output` names is the usable-output set.
  The gate compares that set against the last seen set and emits only
  on a real change, because `WorkspacesChanged` also fires for ordinary
  reshuffles. Sway maps its native output event (a subscription-list
  change plus the adapter's established refetch discipline).
- The event is a trigger, never a command. The handler reconciles:
  it diffs the desired per-output surface map against `cx.displays()`
  and the live surface handles, and the reconcile is written as a
  diff, not a rebuild. Doing nothing is the normal outcome; a spurious
  hint costs one no-op diff.
- `cx.displays()` is the truth source. The workspace-derived set is
  only the hint; when the two disagree, surfaces follow the displays,
  because that is the layer surface's own binding level. This covers
  the open edge of whether a config-disabled output's `wl_output`
  global persists.
- The existing 2s output watch demotes to a heartbeat. It exists to
  catch a missed event, not to be the hotplug path; a hotplug missed
  while the event stream is down is also caught on reconnect, since a
  new event-stream subscriber receives replicated initial state.

## Consequences

- Mode, scale, and rotation changes fire nothing and need nothing:
  they are not workspace-shaped, and the compositor re-anchors and
  resizes layer surfaces itself.
- False alarms are no-ops by construction; false negatives are caught
  by the heartbeat. The failure directions are asymmetric in the
  right way.
- Focused-output changes have no event either (on niri); they already
  reach the shell as `WorkspaceActivated`, and phase two's per-output
  content policy (#73) builds on that mirror, not on a new event.
- The niri side is a shim over an absent event. An upstream feature
  request for a real `OutputsChanged` event would retire the
  set-compare gate someday; the shim is correct without it.
- The sway adapter gains one subscription entry; both adapters then
  produce the same event, and the surfaces stay adapter-blind per
  ADR-0012.
