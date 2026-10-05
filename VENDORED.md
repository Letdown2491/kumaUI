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
PRs, not to kumaUI development. Treat the vendored tree as read-only except for
local patches, which should be recorded here.

## Local patches

- `crates/gpui/src/app.rs`: a `kuma-debug: slab REMOVE window` info line
  in the window drop path (debugging aid; grep-able in the shell's log).
- `crates/gpui_linux/src/linux/wayland/window.rs`: a `closed` flag on
  `WaylandWindowStatePtr`, set as the first act of `WaylandWindow::drop`.
  Clones of the pointer outlive the drop (the client's window map
  unregisters asynchronously, and the input dispatch retains the focused
  window's clone), so the compositor can deliver a `wl_pointer` leave
  (and enter) for a surface gpui has already removed, and forwarding it
  logged `window not found: WindowId(..)` twice per panel close (input +
  hover on the dead window). The forwarding methods (`handle_input`,
  `set_focused`, `set_hovered`, `report_visibility`, `set_appearance`,
  `set_button_layout`) now return early on a closed window.
- `crates/gpui_linux/src/linux/wayland/client.rs`: `GlobalRemove` retires
  the bound `wl_output` (registry names tracked in `output_global_names`).
  A compositor removes the global on unplug or output disable, the inert
  object sees no event of its own, and the upstream TODO left
  `cx.displays()` reporting outputs that are gone for the rest of the
  session. With ghosts, the shell's surfaces watch would keep creating
  layer surfaces the compositor immediately closes (a 2s create/close
  spin against a displayless compositor); retired outputs make
  `cx.displays()` truthful so the watch can idle.
- `crates/gpui_linux/src/linux/wayland/client.rs`: outbound drag
  (`start_external_drag`) advertises `DndAction::Copy` only, where
  upstream advertises `Copy | Move`. Real targets (Nautilus) pick Move
  when given the choice, so a dragged file silently left its folder on
  drop; a file manager's outbound drags default to the non-destructive
  action. Found by the kuma-files DnD spike (issue #25).
- `crates/gpui_linux/src/linux/wayland/client.rs`: `start_external_drag`
  attaches a drag icon surface where upstream passes `None`. With no
  icon, the compositor shows nothing while a drag is live (the ghost
  view renders inside the window only), so outbound drags look broken.
  The icon is a 128x32 Argb8888 rounded pill, CPU-rendered into a
  memfd-backed `wl_shm` pool, attached to a `wl_surface` created from
  the shared `wl_compositor`. The `DragIcon` object (file, pool,
  buffer, surface) rides on `ExternalDrag` and its `Drop` destroys all
  four when the drag finishes or cancels. The pill carries the file
  name (or an "N items" count), rasterized CPU-side with zed-font-kit's
  freetype loader, one A8 canvas per glyph; that adds two dependencies
  to `gpui_linux` (`font-kit`, already in the build via gpui_wgpu, and
  `pathfinder_geometry`, both from the vendored workspace). Found by
  the kuma-files DnD spike (issue #25).
- `crates/gpui_linux/src/linux/wayland/window.rs`: `set_size_and_scale`
  skips `wp_viewport::set_destination` when the size is zero. Layer
  surfaces start at zero size and get their real size from the layer
  configure, but a scale event can land first (niri announces
  `preferred_buffer_scale 2` before the configure on a 162-DPI panel),
  and `set_destination(0, 0)` is a viewporter protocol violation niri
  answers by killing the client. Found by the greeter rehearsal
  (docs/greeter-deploy.md); the lock screen never met it because the
  session niri is configured with scale 1.
