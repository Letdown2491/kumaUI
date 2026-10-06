# ADR-0013: Koguma is standards over wiring; kumaOS things are defaults, not dependencies

Status: accepted (2026-10-06)

## Context

Issue #25 chose to build kuma's own file manager rather than keep
Thunar. Koguma must be deeply integrated with kumaOS (that is its
reason to exist, versus a generic manager) and simultaneously portable
by construction, because it doubles as the desktop's advertisement on
every other distro. Integration usually means coupling; this ADR
records the shape that resolves the tension, which phase 1 has now
demonstrated in practice.

## Decision

Every integration point is a standard first, opportunistic when
kumaOS-specific, and never required:

- **Single instance**: one Unix socket at
  `$XDG_RUNTIME_DIR/kuma-files.sock`; the first instance listens,
  later launches write their directory over and exit. Standard
  session-runtime location, no daemon.
- **Places and recents**: XDG user dirs plus the GTK bookmarks file
  (`~/.config/gtk-3.0/bookmarks`) for places, `recently-used.xbel`
  for recents. Koguma's sidebar and any GTK file dialog stay one
  list without a sync protocol.
- **Trash**: the freedesktop trash spec via the `trash` crate;
  cross-drive failures say "deleted" versus "trashed on drive"
  honestly.
- **Theme**: the shell's palette file
  (`$XDG_RUNTIME_DIR/kuma-shell/palette`) is read opportunistically
  on a 2s tick; absent means built-in colors, not broken. This is the
  only place the string "kuma-shell" appears in Koguma's source.
- **App identity**: `WindowOptions.app_id = "kuma-files"` so the
  shell dock matches the desktop-file stem; the app icon ships as a
  256px PNG because the vendored `svg()` element tints with theme
  text color (see VENDORED.md).
- **Menu entry and handler**: build.sh installs the desktop entry
  (Name=Koguma, GenericName and Keywords so launcher search finds it)
  and registers `inode/directory` with `xdg-mime`, user-local. The
  image-level swap away from Thunar stays gated by the release rule
  (kumaOS#29): Thunar leaves only after a release has ridden Koguma.
- **Protocols ride system tools**: archives via file-roller/tar/unzip,
  PDF previews via pdftocairo, network places (phase 2) via gvfs-FUSE
  and `gio mount`. Koguma writes no protocol code.

## Consequences

- The binary runs on any Wayland/X11 distro. A source audit (2026-10-06)
  found no kumaOS dependency beyond the palette path with its
  fallback; it has not been executed on another distro yet.
- The resource profile is strictly bounded and measured: thumbnail
  cache 300 entries, undo 1000 operations, recents 1000 lines, search
  200 deep rows; idle CPU 0.0 percent, RSS about 89 MiB, dominated by
  the GPU driver, not our data.
- Future Koguma integrations (devices, snapshots, blossom) follow the
  same shape: standard if one exists (udisks2 DBus, btrfs snapshots
  as plain directories, NIP-46), opportunistic if kumaOS-specific
  (the bunker), never required.
