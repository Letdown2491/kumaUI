# Phase-zero audit: what breaks when a second bar exists, 2026-11-20

The audit #72 asks for before per-output surfaces land: every path that
assumes exactly one bar surface. Read against today's tree
(b9181b5), with the lock screen as the in-house per-output reference
(`lock.rs:561-571`: enumerate `cx.displays()`, one window per display,
`display_id` in the options) and the vendored gpui's `display_id` ->
`wl_output` threading (client.rs:1410) as the targeting seam.

## What breaks

**F1. `SurfaceHost` holds one wallpaper and one bar** (`surfaces.rs:62-63`,
single `Option<AnyWindowHandle>` each). This is the finding the phase is
named after: the handles become maps keyed by output name, and
`wallpaper_options`/`bar_window_options` take the display id. Mechanics
are known; not discussed further.

**F2. `PanelHost` is a singleton with last-reporter-wins geometry**
(`panel.rs:546-572`): one `bar: BarGeometry` that each bar overwrites at
every render (`bar.rs:196-208`), one `open: Option<OpenPanel>`, one
`placement`. With two bars, the panel a click on bar B opens would center
on whichever bar rendered last. Toasts hang under `PanelHost::bar()` and
would anchor to the wrong monitor too. The fix shape: geometry keyed by
output (the bar reports its output alongside its rect), the toggle
targeting the toggling bar's output, and the geometry read choosing an
output (the toggler's for clicks, the focused output for toasts).

**F3. `PanelHost::bar_view` is one registration** (`panel.rs:562-564`,
`set_bar`): tooltip suppression after a panel transition reaches only the
registered `ShellBar`, so the other bar's tooltips resurrect on compositor
re-entry. Rides F2's per-output keying.

**F4. msg verbs and background panel toggles have no output**:
`toggle_panel(kind, cx)` (`panel.rs:673`) centers on "the bar". The msg
socket verbs (`launcher`, `notificationspanel`, `settings`, `updates`)
and any programmatic toggle need a target output, which is the focused
output (the bar on the focused monitor), falling back to any bar when the
session mirror has no focus yet.

**F5. The dock opens one window** (`dock.rs:311`, `DockHost.window` one
`Option<WindowHandle<DockView>>`). Its sync-probe-recreate loop is already
the right shape; the handle becomes a per-output map, and `#72`'s
"optionally" applies per output: a dock on the laptop panel and none on
the external is the same map with a missing entry. Where the per-output
dock policy (enabled where?) lives is a settings question #72 leaves to
the default: dock enabled means one per display, as the lock screen
already means one per display.

## What does not break

- **Window title**: reads `niri.focused_window()` from the mirror
  (`bar.rs:443-449`); both bars showing the focused window's title is
  correct-by-design, and whether a bar shows its *own* output's focused
  window is content policy (#17 phase 2), not a break.
- **Workspace clicks and focus routing**: clicks route compositor-global
  commands (`session.rs::focus_workspace`); no bar-local state.
- **Tooltips and pointer snapshots**: tooltips render inside the bar's
  own window (the surface reserves tooltip room, `bar.rs:155-166`) and
  `window.mouse_position()` is per-window. No cross-bar state.
- **OSD**: its surface already targets the focused output
  (`osd.rs:285` gates on displays); #72 pins this as unchanged.
- **Lock screen**: already per-output, both in surfaces and in decoding
  one wallpaper per display.

## How a bar learns its output

The reconcile opens each surface for a display it enumerated, so the
output identity is known at construction: `surfaces.rs` hands the
display's name (gpui carries the compositor's own output name,
`gpui_linux/.../display.rs:16,32-35`) into the `ShellBar`/`DockView`
constructor, and the view reports it with its geometry (F2). No window
introspection needed; the layer surface's own output binding is the
creation-time fact.

## Verdict: two landings, in this order

1. **Surfaces**: `SurfaceHost` maps + display-targeted options + the
   dock's per-output map (F1, F5). Single-output machines see no
   behavior change; the bar exists before panels route, so nothing is
   worse.
2. **Panel routing**: `PanelHost` per-output geometry, per-output
   suppression, toggle targeting, and msg/toast focused-output routing
   (F2, F3, F4). Landing it second keeps the surface work reviewable
   and the routing work honest about which bar is which.

Both before the multi-bar ship: with only landing 1, a click on either
bar's widget opens a panel centered on an arbitrary bar, which is the
kind of wrong that looks right on a single monitor and maddens on two.
