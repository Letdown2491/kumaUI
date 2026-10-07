# ADR-0015: Panel surfaces stretch along their placement axis; the drawer positions at render

Status: accepted (2026-10-06)

## Context

Panels were positioned by layer-shell margins computed once, at window
creation, from the bar geometry reported in that instant. Any geometry
change while a panel stayed open left the drawer floating at stale
coordinates: switching the display mode, or changing the bar's width,
stranded an open panel. Separately, the bar's change detection compared a
tuple that omitted content_x, so an align change never re-reported
geometry at all: panels kept centering on the old alignment even after
reopening. Bar placement also clamped only the left edge, so a narrow
right-aligned bar could push a panel past the screen's right edge.

The bar itself already solves this class of problem: its surface is
stretched full-width and transparent, and width, align, and offset are
applied by the content div at render.

## Decision

- Panel surfaces stretch along their placement axis. Bar and Widget
  panels anchor to the edge the bar hangs from (`BarPosition`: a top bar
  anchors TOP|LEFT|RIGHT, a bottom bar BOTTOM|LEFT|RIGHT, and the drawer
  opens away from the bar, so a bottom bar's panels open upward); a dock
  menu stretches along its dock's edge. Only the cross-axis offset (the
  bar's inner face, or the gap to the dock) stays a fixed margin.
- The drawer's offset along the stretched axis is computed at render by
  `drawer_axis` (panel.rs) from the live bar geometry and viewport,
  clamped to both screen edges. `chrome` positions the silhouette, the
  content, and the input region from that value.
- The bar's change-detection tuple includes content_x, so align changes
  re-report geometry; when the reported geometry changes and a panel is
  open, the bar asks the host to refresh the panel's windows
  (`refresh_open_panels`, deferred like every cross-window update), so
  the drawer re-anchors on the next frame.

## Consequences

- Resolution, align, and bar width changes all reposition an open panel
  live; stale placement is structurally impossible rather than patched.
- While open, a panel window is a full-output-wide (or tall) transparent
  surface. Input stays confined to the drawer body, so clicks outside
  still reach the scrim; the cost is one mostly transparent surface of
  extra compositing while a panel is up.
- Panel view state survives geometry changes: no close-and-reopen.
- Toasts and the OSD still bake panel_top at open. They are
  screen-centered and short-lived, and stay out of scope.
