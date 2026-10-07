# ADR-0015: Panel surfaces span the whole output; the drawer positions at render

Status: accepted (2026-10-06), revised same day (full-output surfaces
replace axis-stretched ones)

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

The first revision of this decision stretched panel surfaces along their
placement axis (a top bar's panel anchored TOP|LEFT|RIGHT with the bar's
inner face as a margin, a dock menu stretched along its dock's edge) and
computed only the long-axis offset at render. That shape had two flaws.
A bar position change still flipped the surface's anchor edge, so an
open panel kept its stale anchor until it was closed and reopened (the
surface's anchors are fixed at creation). And the measured-height views
still resized their surface at render: with both left and right edges
anchored, that set_size fires before the surface's first configure (the
creation hint's nonzero width defeats the bar's viewport guard), and
niri kills the surface with a wp_viewport protocol error. Every
measured-height panel (the widget mini-panels) died within a second of
opening.

## Decision

- Panel surfaces anchor to all four edges, carry no margins, and span
  the whole output: the scrim's shape (exactly, including the
  `exclusive_zone: -1` and Layer.Overlay). The surface carries no
  placement at all.
- The drawer's full origin (x and y) is computed at render by
  `drawer_origin` (panel.rs) from the live placement, bar geometry, and
  viewport: centered on the bar content (Bar) or the anchor x (Widget),
  hung just off the bar's inner face (below a top bar, above a bottom
  bar), or hung off the dock cell per the dock's edge, clamped to the
  screen. `chrome` positions the silhouette, the content, and the input
  region from that value.
- The client never resizes a panel surface. A size on an all-edge
  anchored surface is the compositor's to assign, and niri kills
  surfaces that set_size before their first configure. The
  measured-height views only refine their view's geometry height and
  notify; the drawer div reads that height at render.
- The bar's change-detection tuple includes content_x, so align changes
  re-report geometry; when the reported geometry changes and a panel is
  open, the bar asks the host to refresh the panel's windows
  (`refresh_open_panels`, deferred like every cross-window update), so
  the drawer re-anchors on the next frame. This now also covers bar
  position flips: nothing about the surface depends on the bar's edge,
  so a live flip just re-renders the drawer at its new origin.

## Consequences

- Resolution, align, width, offset, and bar position changes all
  reposition an open panel live; stale placement is structurally
  impossible rather than patched, and no surface is ever recreated or
  resized to follow the bar.
- While open, a panel window is a full-output transparent surface.
  Input stays confined to the drawer body, so clicks outside still reach
  the scrim; the cost is one full-output surface of extra compositing
  while a panel is up.
- Panel view state survives geometry changes: no close-and-reopen.
- Toasts and the OSD still bake their edge offset at open. They are
  screen-centered and short-lived, and stay out of scope.
