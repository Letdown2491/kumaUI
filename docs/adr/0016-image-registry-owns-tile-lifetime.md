# ADR-0016: The image registry owns tile lifetime

Status: accepted (2026-10-08)

## Context

The 2026-10-08 RAM-focused architecture review (all file:line claims verified
against the tree at 76bf810) found that ADR-0011's guarantee, "the atlas page
count is bounded by the concurrent image set", holds in exactly one place:
`notifications.rs::drop_icon`, then and still the only `drop_image` call in the
repo. gpui paints a `RenderImage` as an atlas tile keyed by that instance's
`image_id`, and nothing but an explicit `drop_image` ever frees it. Three
surfaces never learned the rule:

- **The tray** re-decoded every item's icon into a fresh `RenderImage` on every
  2s poll (`tray.rs` built its own theme index and called `decode_icon_file`
  per poll), minting a new tile per poll on the bar's persistent atlas. A 24px
  pixmap item leaks roughly 95 MiB/day; larger pixmaps, proportionally more.
- **Wallpaper rotation** replaced the decoded wallpaper without dropping the
  old tile: one full-screen tile (8.3 MB at 1080p, 33 MB at 4K) per rotation.
- **Koguma's thumbnails** are FIFO-capped at 300 in the heap, but evicted
  thumbs' tiles stayed in the Koguma window's atlas for the window's life: the
  heap's honesty did not extend to the atlas.

Underneath the three leaks sits one shape: the memory rule (decode, paint,
drop) is enforced by hand at each call site. `imaging.rs` already owns decode,
byte order, theme indexing, and the process-wide icon cache, and its module
comment claims "every path into a RenderImage goes through here", yet its
interface says nothing about lifetime. A rule callers must remember is a rule
callers forget. Separately, the launcher re-decoded every installed app's icon
at native size (a 512px PNG is about 1 MB decoded) to paint 22px cells, per
open, through a third private index walk; the probe's launcher phase documents
this as the heaviest single open in the shell.

## Decision

`imaging.rs` becomes the **image registry**: the one seam that owns decoded
pixels and their atlas tiles. Two functions carry the whole interface:

- `resolve(key, size) -> Option<Arc<IconImage>>`: looks up the cache, decodes
  on miss (rasters downscaled to `size`, SVGs stored once, size-independent),
  and returns a **stable shared Arc**. Stability is the leak fix for painted
  icons: the same Arc repaints to the same `image_id`, so one tile serves the
  icon's whole life no matter how many polls or window recreations paint it.
  The cache is LRU-capped at 256 (name, size) entries with negative caching
  kept; `cached_icon` and `resolve_icon` retire in favor of it.
- `release(image, cx)`: the explicit tile drop, the formalized
  `drop_icon` pattern, for caller-owned images whose asset is leaving: the
  tray's pixmap items (cached per item id, released on replacement), the
  wallpaper (released when a rotation's replacement decode lands), and
  Koguma's evicted thumbnails.

Registry eviction deliberately does not drop tiles: eviction only sheds the
registry's own reference, is bounded by the 256-entry cap, and avoided
threading `cx` into background decode paths. An evicted entry whose tile is
still painted keeps that tile until its owner releases; the bound is the cap.

Wallpapers and other full-file unique decodes (the greeter's, the wallpaper
picker's previews, blob previews) keep `decode_file` and friends: they have no
key, no reuse, and no business inside an icon cache.

## Consequences

- The bar's tray atlas is bounded by concurrent items, not polls; the
  wallpaper surface by one image per display; the Koguma window's tile set by
  the 300-thumb working set. ADR-0011's consequence sentence is now true at
  every image call site, not just notifications.
- The launcher's open cost drops to a one-time decode of 44px thumbnails
  shared with the dock's pipeline. The acceptance gate for the drops
  themselves is unit tests (the tray's pin-and-release, thumb eviction,
  registry bounds): the probe's container has no tray items and no wallpaper
  rotation, and its run-to-run noise swamps per-phase deltas at this effect
  size (the same binary measured 1.6 and 2.2 GB at settle across
  back-to-back runs on 2026-10-08). The probe stays in the loop for gross
  regressions, read as ranges across repeated runs, never as single-run
  before/after tables.
- Three icon pipelines (registry cache, tray's private index, launcher's and
  the slider panel's per-open walks) become one.
- Explicit release remains a per-caller act; the registry cannot force it. The
  review's deferred refinement (Weak-based auto-release inside the seam) can
  replace the explicit form later without touching callers.
