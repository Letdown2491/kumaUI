# ADR-0004: Panels are layer-shell drawers with a scrim; keyboard mode per panel

Status: accepted (2026-10-01)

## Context

Panels (settings, launcher, later: notifications) hang flush under the Bar,
centered, with a concave cove silhouette. They must close on Esc and on
click-outside, and some need the keyboard (launcher search) while others must
never steal it (settings).

## Decision

- Every panel renders the shared `drawer_silhouette` (cove + body + rounded
  bottom as one SVG path) and receives its `PanelGeometry` from the Panel
  host; views never hardcode their size.
- A fullscreen transparent **scrim** surface maps below every open panel
  (same Overlay layer, mapped first). Clicks outside land on the scrim and
  dismiss all panels.
- Keyboard: `Exclusive` for the launcher (it must own typing), `OnDemand` for
  settings (click-to-focus, Esc after focus).
- The host is a gpui Global reached through free functions
  (`toggle_panel`/`close_panels`); the global is borrowed in short blocks,
  never across a call that also needs `cx`.

## Consequences

- A new panel = a `PanelKind` variant + geometry table row + a view. The
  scrim, dismissal, centering, and silhouette are the host's job.
- Panels cannot outlive the host's knowledge: views close via
  `close_panels`, never `remove_window` on themselves.
- The cove corners are click-through (input region = body rect); a future
  per-corner input shape is a refinement, not a blocker.
