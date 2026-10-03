# ADR-0011: The Shell returns memory to the OS at two seams

Status: accepted (2026-10-03)

## Context

The Shell is a weeks-long process on a laptop that suspends nightly.
glibc's per-thread arenas never shrink: every page a panel open, a toast,
or an icon decode ever touched stays mapped, and each suspend cycle swaps
the untouched ones out for good. Thirteen hours of a real session measured
619 MB swapped across ten 64 MB arena mappings, on a process whose working
set was a tenth of that. Separately, gpui's `img` element paints an image
into its window's atlas and never drops the tile; a notification's icon
that leaves the history would keep its atlas slot until the window dies.
A container probe (`scripts/memory-probe.sh`) confirmed both shapes:
growth plateaus (no unbounded leak), but the plateau sits far above the
working set, and icon-heavy notifications retain their decoded pixels and
atlas tiles past any use.

An allocator swap (mimalloc, as zed's own binary offers) was measured and
rejected: the plateau did not shrink and the startup baseline grew.

## Decision

Memory is returned at two seams, both inside kuma-shell, no vendor patch:

- **Icon death drops atlas tiles.** Wherever a notification leaves the
  history (dismiss, clear, cap eviction, id replacement with a different
  image), its raster icon's tiles are dropped via `App::drop_image`
  (`notifications.rs::drop_icon`). The atlas page then empties and the
  next image reuses it instead of pushing a new one. `insert_notification`
  returns the entries the cap pushed out so the callers own the drop.
- **A 30 s background trim.** A detached task calls `malloc_trim(0)`,
  which walks every glibc arena and madvises the free pages away. It is
  cheap when there is nothing to release, and it uses the background
  executor's own timer, the same driver as the bar clock. This is the
  Shell's one deliberate force: glibc will not shrink an arena on
  request, so a recurring rake is the measured price of a working-set
  footprint.

## Consequences

- The Shell's steady footprint tracks its working set (history pixels
  stay: at most `MAX_HISTORY` icons), not its lifetime high-water.
- Dismissing a large notification batch can show a visible RSS drop
  within 30 s; that is the trim working, not a crash.
- The atlas page count is bounded by the concurrent image set, not the
  lifetime image set. Emoji and glyph tiles still live forever (a
  bounded, zed-level design choice; not touched).
