# ADR-0017: kuma-platform, the shared crate's scope

Status: accepted (2026-10-15)

## Context

Four apps live in the workspace (kuma-shell, kuma-files, kuma-term,
kuma-monitor), and several pieces of wiring were copy-paste identical in
two or more of them:

- the `--version` flag and the version line it prints (every app), plus
  the changelog guard test (shell, files, term);
- the shell's palette handshake: locate `$XDG_RUNTIME_DIR/kuma-shell/palette`,
  notice its mtime moving, parse the key=value lines into a ten-color
  struct (kuma-files and kuma-monitor byte-identical, kuma-term reading
  the same file through its own path walk);
- the HSL color space the palettes derive from (`rgb_to_hsl`/`hsl_to_rgb`,
  byte-identical in three crates);
- the niri IPC envelope (ADR-0003's hand-rolled client: one request line,
  one reply line, niri's Ok unwrapped), duplicated by the monitor with a
  read timeout added.

Issue #33 is the extraction. The freeze note in the copied code said what
this ADR records: the copies were allowed to land so a spike could move
fast, and the platform crate is where they come home.

## Decision

One workspace crate, `kuma-platform`, path-dependency of every app. A
module lands when a second app needs it; apps declare what they are, the
platform owns how. The crate carries five modules:

- `version`: the line, the in-app tag, the changelog guard helper, and
  the flag predicate. The shell's CLI dispatcher keeps its bare-word
  `version` spelling to itself.
- `palette`: the published file's path, its mtime (the change signal),
  the text, the ten-color `Palette` struct (the file's documented
  contract, mirrored field for field), the parser with its alpha fold,
  and the hex color grammar (public: koguma's config parser reads the
  user's accent line through it). Each app keeps its own theme
  application, because the slots each app derives from these ten colors
  are its own.
- `color`: the HSL space.
- `niri`: the envelope, `socket_path` plus `request` and a timed variant.
  Events, actions, and response types stay in the apps: the platform
  carries no session policy, and each app's adapter remains the only
  module above the wire that names niri.
- `titlebar`: the family's client-side title bar (added 2026-10-15).
  gpui claims client-side decorations on Wayland and draws nothing
  itself; every app was left to invent its own bar or live with a bare
  window. The platform owns the behavior and the geometry (the drag via
  `start_window_move`, double-click maximize, right-click
  `show_window_menu`, the buttons wired to `minimize_window`,
  `zoom_window`, and close, the cluster trimmed to what the
  compositor's `wm_capabilities` backs). The app owns the look: it
  hands over its theme colors and its glyph asset paths, so the bar
  wears each app's skin without the platform growing a palette or an
  asset set. A module may add `pub fn` seams later (content slots
  beside the title, a fold-away when the compositor tiles the window);
  those land when an app asks.

Dependencies stay in the stratum the apps already carried: `serde_json`,
`anyhow`, and now `gpui` (the titlebar's primitives: every app depends
on it already, so the tree grows nothing).

## Alternatives

- Leave the copies. Cheap, and honest while the apps were three, but the
  monitor made four and the copies were drifting (three palette readers,
  two wire clients). A contract fix would have had to land three times.
- A `util` module inside each app. Does not fix anything: the copies stay
  copies, just with worse provenance.
- A macro-only or no-dependency crate. The envelope wants `serde_json`
  and `anyhow`; pretending otherwise produces traits instead of code.

## Consequences

- A contract fix lands once. The palette parse, the HSL math, and the
  envelope unwrap are tested in one place (13 platform tests) and the
  apps test their own mapping on top.
- Adding a dependency to kuma-platform adds it to every app's tree. The
  crate stays dependency-light on purpose; a module that wants a heavy
  dependency is a signal to reconsider the seam.
- The shell remains the only palette publisher; consumers read or fall
  back to built-ins (Ask, don't assume).
- Known next candidates, deliberately deferred: a shared desktop-entry
  parser (koguma's open-with does not honor `OnlyShowIn`/`NotShowIn`,
  the shell's launcher does; the comment in `browser.rs` points here),
  and any unification of the shell's sysmon parsers with the monitor's
  finer-grained ones. Divergence there is measured, not accidental, but
  neither caller has asked for the other's shape yet.
