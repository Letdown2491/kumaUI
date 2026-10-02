# Vendored gpui

kuma-shell depends on gpui via a path dependency into `vendor/zed/` (gitignored).

## Why not crates.io?

The published `gpui` crate (0.2.2) is an old snapshot of the code that predates the
Wayland layer-shell API (`WindowKind::LayerShell`, `gpui::layer_shell`). Layer-shell
support only exists on zed `main`, and several layer-shell sizing fixes are still
open PRs there. Vendoring gives us the API and lets us patch locally if those bugs
bite us.

## Re-vendoring

```
./scripts/vendor-gpui.sh            # pinned commit
./scripts/vendor-gpui.sh <sha>      # specific commit
```

Pinned commit: `40180d9c40e2d20eb63d388bff920818f2910b53` (zed main, 2026-09-30)

Note: `vendor/zed/AGENTS.md` belongs to the zed repo; its rules apply to upstream
PRs, not to kumaui development. Treat the vendored tree as read-only except for
local patches, which should be recorded here.

## Local patches

- `crates/gpui/src/app.rs`: `kuma-debug: slab REMOVE/INSERT window` info
  lines in the window slab paths (debugging aid; grep-able in the shell's
  log).
- `crates/gpui_linux/src/linux/wayland/window.rs`: a `closed` flag on
  `WaylandWindowStatePtr`, set as the first act of `WaylandWindow::drop`.
  Clones of the pointer outlive the drop — the client's window map
  unregisters asynchronously, and the input dispatch retains the focused
  window's clone — so the compositor can deliver a `wl_pointer` leave
  (and enter) for a surface gpui has already removed, and forwarding it
  logged `window not found: WindowId(..)` twice per panel close (input +
  hover on the dead window). The forwarding methods (`handle_input`,
  `set_focused`, `set_hovered`, `report_visibility`, `set_appearance`,
  `set_button_layout`) now return early on a closed window.
