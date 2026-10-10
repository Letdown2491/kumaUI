# Changelog

Entries land with the change they describe; the next tag takes this
section as its release notes. Say what changed and what a reader has to
do differently. Why it changed belongs in the commit that made it. The
three lines under a release heading name each binary's version in that
release: an app whose line did not move rides unchanged.

## Unreleased

## v0.2.0 (2026-10-10)

kuma-shell 0.2.0
kuma-files 0.2.0
kuma-term 0.2.0

### Added

- **Everything reports its own version.** All three binaries answer
  `--version` and `-V` with the crate version plus the commit build.sh
  stamped in (`kuma-shell 0.2.0 (gaab28b8)`); the shell also takes it as
  a CLI verb, before any session, running instance or IPC, and logs the
  same line at boot. A build without the stamp falls back to `unknown`,
  so a plain cargo run still prints. A bug report now pins an exact
  build, and Koguma's places sidebar signs off with the same tag.

- **The shell's settings gain an About page.** Name, version tag, the
  host distro read from os-release's `PRETTY_NAME`, and links to the
  repo and the issue tracker (info.svg joins the icon set). Nothing to
  configure; the page is static on purpose.

### Changed

- **Koguma's keybinding cheatsheet is a modal.** The sidebar's
  collapsible Keys block became a Keybindings button that opens a
  two-column card; escape, the ×, or a click outside the card closes
  it, and while it is up the key dispatcher ignores everything else.
  Every hint row was re-checked against the dispatcher and regrouped by
  task (cursor moves, clipboard, tabs, view, zoom). The saved-state
  `keys=` line is gone: old state files read it as unknown and drop it
  silently, so nothing to migrate.

- **Koguma's Network section always shows.** Connect to Server moves
  into it as the always-present tail row (the plus.svg affordance), so
  the section is the entry point even with no mounts to list; section
  rows indent slightly under their headers.

## v0.1.0 (2026-10-09)

kuma-shell 0.1.0
kuma-files 0.1.0
kuma-term 0.1.0

The first release: eight days from an empty repo to the whole desktop.
Everything here is new; a reader on kumaOS picks all of it up through
the image's kumaui pin.

### Added

- **The desktop is kuma-shell.** One process paints the bar and its
  widgets, the launcher, the drawer panels, the OSD cards and the
  settings panel, all layer-shell surfaces on the vendored gpui. Bar
  widgets are placeable and toggleable from the settings' Widgets and
  Ordering pages, geometry applies live, and the dock pins favorites
  beside running windows on its own surface. niri's keybinds drive the
  shell through `kuma-shell msg` verbs (volume, brightness, media,
  launcher, notifications) that prefer the socket and fall back to
  wpctl and brightnessctl standalone.

- **The theme derives from the wallpaper.** The shell extracts a
  palette from the wallpaper and publishes it to the runtime dir;
  Koguma reads it opportunistically and recolors on wallpaper changes,
  with an accent picker for the times the extraction guesses wrong.
  Wallpaper rotation rides a slideshow with quiet hours.

- **Session state is compositor-neutral.** The workspaces, window
  title and dock read a session mirror with niri and sway adapters
  (ADR-0012), so the shell runs end to end on sway, and the
  displayless smoke exercises the whole surface lifecycle headlessly.

- **The system monitors are widgets with detail panels.** Memory, cpu
  temperature, disk, microphone and the power profile poll behind an
  adapter seam; the volume panel carries per-app rows; battery,
  bluetooth, internet and weather panels hang under the bar with
  geocoding and a forecast. Audio and brightness changes ride a
  serialized request queue, and OSD cards name whichever change a key
  or an outside tool made, do-not-disturb and power profile included,
  with quiet hours folding into the DND card.

- **The desktop locks, and the greeter is kuma's own.** The lock
  screen takes logind's Lock signal on every display at once, one
  shared password field, PAM through a chain that skips uninstalled
  services (ADR-0008); the idle timeouts live in the settings panel.
  kuma-greeter speaks greetd, discovers Wayland sessions, and rides the
  same gpui build as the shell, showing the wallpaper and skipping
  niri's hotkey overlay; boot-phase timings priced the shell-side
  login handoff at 16 ms.

- **Displays are set over niri's IPC.** The settings' Displays page
  applies modes and transforms and persists only the delta into
  local.kdl (#26), and a failed apply persists nothing (#28).

- **Koguma grew out of a DnD spike into the desktop's file manager.**
  Tabs, the places sidebar (Home, Recent and Trash up top, then places
  and mounts), icon and details views, rubber-band select, sortable
  columns, an op queue with undo and a keyboard-first conflict dialog,
  rename, compress and extract, open-with over desktop entries, single
  instance, session restore, and the info rail docked right or bottom.
  Type-to-filter searches the subtree from a debounced background
  walk; thumbnails decode three at a time into an atlas that returns
  its tiles when entries evict.

- **Koguma browses the network.** gvfs and udisks2 mounts under
  Places, Connect to Server with a protocol picker whose pump relays
  gio's passwords and host-key questions, disconnect and eject from
  the right-click menu, and stale bookmarks that stay listed dimmed
  until a click reconnects them.

- **Quick Look became a reader.** Space peeks a file: text, csv and
  markdown read whole; epubs scroll as one continuous document with
  chapter typography and covers; pdfs page through; videos show a
  poster frame and duration; images zoom, pan and flip with the
  arrows. Rename opens with the name selected, so typing replaces it.

- **Koguma is the desktop's opener.** Images and PDFs open standalone
  from Enter or xdg-open, the arrows walk the containing directory's
  files of the same kind, and the video support grew teeth: libav
  decodes in-process (feature-gated on the sys crate, a named state
  when a codec is missing from the system's build), posters and a
  muted inline preview ride the listing, and the thumbnail pool gained
  a disk cache.

- **Higuma is the terminal.** alacritty_terminal behind an engine
  seam, the grid painted as one canvas element, vector box drawing,
  powerline and braille glyphs, tabs with kitty's jump chords, mouse
  reporting and selection, scrollback search with a regex toggle,
  DECSCUSR shapes and blink, a kitty-compatible config at the XDG
  path with the noctalia pastels built in, a prompt bar tapped from
  OSC 133/7 with a git segment, `KUMA_TERM=1` for dotfiles, the `-e`
  argv contract, and URL click-to-open.

- **Everything installs as a desktop app.** build.sh builds in the
  podman dev container (the host has no toolchain) and installs the
  shell, Koguma and Higuma with their menu entries, png app icons and
  the directory-handler binding; the desktop templates ship from the
  tree, one template, two substitutions.

- **Polkit prompts wear the lock screen's shape.** The shell registers
  as the polkit authentication agent (#65/#68): one centered card,
  exclusive keyboard, the identity resolved from unix-user and
  unix-group. The launcher honors OnlyShowIn/NotShowIn and Hidden with
  glib's exact semantics (#67), and the app icons paint full-color SVG
  through the renderer instead of alpha-masked silhouettes.
