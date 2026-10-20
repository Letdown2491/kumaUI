# Changelog

Entries land with the change they describe; the next tag takes this
section as its release notes. Say what changed and what a reader has to
do differently. Why it changed belongs in the commit that made it.
Bullets land under Added, Changed, or Fixed; `scripts/release.sh` reads
those headings to infer the bump (any Added or Removed: minor; only
Changed and Fixed: patch). The version lines under a release heading
name each binary's version in that release: an app whose line did not
move rides unchanged.

## Unreleased

### Changed

- Idle cost, all four apps: the shell's bar no longer repaints on a
  sysmon, tray, or vault poll that found nothing moved, and a wedged
  PipeWire can no longer stall the shell's poll for the session (the
  volume and stream reads ride timeout now, and the vault widget's
  poll backs off to a minute when nothing answers). Kamuy skips its
  repaint while an equal beat applies and while the window cannot
  paint. Koguma's listing is windowed to the viewport: a directory
  with tens of thousands of entries builds the rows and cells on
  screen instead of all of them every frame, renders from a shared
  snapshot instead of deep-cloning every entry, memoizes the filter
  order per keystroke, and lands epub chapters without recopying the
  chapters before them; a rescanned directory reads its listing off
  the UI thread. kuma-term coalesces a pty output burst into one
  repaint, and its row cache can no longer be wiped by one scroll
  burst.

- kuma-term: OSC 52 clipboard writes now ask like kitty's, a small
  card at the prompt with a content preview, y to allow, n or escape
  to deny, where before any output reaching the screen could silently
  rewrite the clipboard. The card denies itself after ten seconds, so
  an unanswered prompt never holds the keyboard hostage. URLs open on
  ctrl+click, matching kitty, alacritty, and gnome-terminal; the
  underline and pointing-hand cursor only show while ctrl is held,
  and a stray plain click no longer fires the browser.

### Fixed

- kuma-files: epub internals are bounded now, so a hostile or
  oversized book cannot own the RAM: each zip entry stops reading at
  32 MiB, the glance text chain stops at a budget with a marker, and
  a chapter entity split across a multi-byte character no longer
  panics the scanner. Archive extraction, compression, and the PDF
  page-count and preview tools ride `timeout` as well, so a wedged
  child cannot hold the op queue forever, and a corrupted state file
  restores at most 32 deduplicated tabs.

- kuma-shell: every subprocess read the poll and panel paths make
  (volume, media, network, bluetooth, power, brightness, the nostr
  CLI) is bounded by `timeout` now, so one hung CLI costs seconds
  instead of a subsystem's updates for the rest of the session. The
  sway and greetd readers also refuse to allocate what a length
  prefix alone claims, the greeter's auth exchange is capped, and a
  workspace name with a quote can no longer break out of the sway
  command it rides in.

- kuma-monitor: one unreadable sensor entry under /sys/class/hwmon no
  longer wipes out temperature discovery for the whole session; the
  entry is skipped instead of aborting the search.

- kuma-term: pasting clipboard text that carries the end-of-bracketed-
  paste marker no longer hands the shell a command to run; the
  markers are stripped from the payload. Input writes also moved to a
  writer thread: a large paste into a program that is not reading
  (a stopped job, a wedged ssh) used to freeze every tab on the UI
  thread's blocking write.

- kuma-shell: joining a Wi-Fi network no longer puts the network
  password on nmcli's command line, where any process could read it
  in /proc/<pid>/cmdline for the whole connect; it rides a 0600 file
  in the runtime directory through nmcli's passwd-file. The polkit
  PAM helper exchange and the per-device bluetoothctl reads are
  bounded by timeout now, so a hung PAM stack or device cannot pin a
  worker thread (and an unreaped setuid child) forever.

- kuma-term: a tab whose shell is gone no longer crashes or freezes
  the terminal. An orphaned background process holding the terminal
  kept its reader alive after the tab closed, and its events indexed a
  removed tab (a panic) or landed on the tab that slid into the old
  slot; events are routed by session identity now, and closing a tab
  whose child ignores SIGHUP waits for that child on a reaper thread
  instead of the UI thread, so every tab no longer freezes with it.

- kuma-files: extracting a short-named archive (say, a file called
  `1.tgz`) crashed with an arithmetic underflow in the folder-name
  computation, and names like `abc.tgz` extracted into the listing
  directory itself; the extraction now strips the suffix that actually
  matched.

- kuma-shell: the keybind socket could be wedged or drained by one
  local peer. The listener read request lines with no timeout and no
  length ceiling, so a process that connected and sent nothing (or
  bytes without a newline) hung every keybind verb until the shell
  restarted; reads are bounded in time and in size now.

- kuma-shell: the session mirror survives a dead compositor stream. A
  dropped socket, a restarting compositor, or a reshaped event payload
  used to end the mirror for the rest of the session (frozen
  workspaces and window titles until the shell restarted); the stream
  now reconnects after three seconds, and an event it cannot parse is
  skipped with a journal line instead of costing the stream.

- kuma-files: Koguma's listing scrolls with the wheel again. The row
  wrapping the sidebar and the folder column had no min-height floor
  of zero, so it grew to the folder's full content height instead of
  the window's: the listing never overflowed and a folder with more
  files than fit the window had no way to reach the rest.

- kuma-files: folders render in full now. The old 200-entry render
  cap hid everything past entry 200 even with scrolling working (the
  icon grid queues thumbnail decodes only for the viewport's
  neighborhood, so a huge folder no longer queues a decode per cell),
  and the keyboard cursor (arrows, Home, End) brings its row into
  view.

## v0.4.0 (2026-10-17)

kuma-shell 0.4.0
kuma-files 0.4.0
kuma-term 0.3.0
kuma-monitor 0.2.0

### Added

- kuma-monitor: Kamuy is single-instance now, the family's convention:
  launching it while it runs hands a bare poke over the activation
  socket and exits, and the running window comes forward instead of a
  twin. The socket itself (the handoff, the stale-socket recovery, the
  activation pump) graduated into kuma-platform, Koguma's same
  machinery keyed by app id; Koguma rides it unchanged.

- kuma-term, kuma-files: Higuma and Koguma wear the family's titlebar
  now, the one Kamuy piloted: the drag region, the centered name
  (Higuma is the terminal's display name; the binary and app id stay
  kuma-term), and the minimize, maximize, and close buttons the
  compositor backs.

### Changed

- kuma-term: Higuma's titlebar now sits one step above the terminal
  background instead of wearing it outright, pulled toward the theme's
  own foreground so a pasted kitty theme dresses the bar too. This is
  the bar treatment Koguma and Kamuy already use, and it reads as a
  distinct strip on desktops without the wallpaper palette. Nothing to
  do differently; the lift follows your theme either way.

- kuma-term, kuma-files, kuma-monitor: the titlebar now folds away
  while the window is tiled and returns when it floats; fullscreen and
  maximize report the same way and fold too. Nothing to do
  differently.

### Fixed

- kuma-shell: the settings Ordering page now lets you reorder widgets in
  every section, left and center included; before, the right column
  claimed every drop. gpui delivers drag-move events to all three
  columns' handlers on each move regardless of hover, and the
  rightmost handler ran last, so it kept overwriting the insertion
  preview. Each handler now only answers when the cursor is inside its
  own bounds.

- kuma-files: overwriting an image in place (same path, new contents) now
  refreshes Koguma's grid thumbnail on the next paint; before, the
  in-memory thumb cache keyed by path alone and served the old decode
  until the app restarted. Each cached thumb now carries the mtime it
  decoded from, the same stamp the disk cache's key already had.

- kuma-shell: the polkit agent's auth prompt now survives polkitd restarts
  and registers for the logind display session instead of trusting the
  inherited `XDG_SESSION_ID`, which carries greetd's stale session id for
  user services; before, one polkitd restart left the machine with no
  working polkit prompt until the shell restarted.

## v0.3.0 (2026-10-12)

kuma-shell 0.3.0
kuma-files 0.3.0
kuma-term 0.2.0
kuma-monitor 0.1.0

### Added

- **Koguma's Quick Look plays videos.** Opening a video now runs the
  file through libav in-process (the same decoder Celluloid uses) and
  presents it frame by frame instead of cycling a short silent loop.
  Sound plays too, through the sound server (PipeWire's pulse-simple
  entry point, dlopen'd; systems without it play muted), and the
  video syncs to the audio clock. Space pauses and resumes a playing
  clip, and replays one that ran out. The transport lives in a footer
  strip under the video, the players' convention: elapsed time, a
  full-width seek bar (click or drag), total time. The Quick Look
  header slims down to the name, one dim glance (position in the
  listing, then the file's own facts), the pager, and two buttons;
  the key-hint line is gone. The keys modal opens with F1 or the
  header's ? chip (it now also covers Quick Look's rungs), the
  volume and seek keys stay: J/L seek by ten seconds, +/- steps the
  volume, M mutes; F11 fullscreens the window while a video is up
  and escape unwinds it before closing the pane.
  The standalone viewer plays videos too, with the same engine and
  the same transport: `kuma-files clip.mp4` opens a playing surface,
  and the desktop file now claims `video/mp4`, `video/webm`,
  `video/x-matroska`, `video/quicktime`, and `video/x-msvideo` (audio
  files wait for the viewer's audio pane and stay with Celluloid).
  Decode attempts VAAPI hardware first and falls back to software
  wherever the driver is missing; `KOGUMA_NO_HWACCEL=1` forces
  software for A/B checks.

- **Koguma renders SVG files.** The grid shows a true-color thumbnail
  (rasterized in-process at the fit size, so small icons stay crisp),
  Quick Look glances them, and the standalone viewer opens one with
  zoom and rotate, the arrows walking sibling SVGs. The desktop claim
  `image/svg+xml` moves to Koguma, so Enter, double-click, and
  `xdg-open` all land in it. avif stays unclaimed; it is the last
  kind with no decoder here.

### Fixed

- **Apps installed mid-session get their icons without a shell restart.**
  The shell's icon index walked the XDG icon roots once per process, so
  an app installed while the shell ran (a flatpak, say) resolved no
  icon in the dock and launcher, and the dock showed its window as a
  raw entry with the app_id for a label. A named lookup that misses now
  re-walks the trees when they changed underneath (rate limited to one
  walk per 5 seconds), and the dock rescans the desktop files when a
  window matches nothing.

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
