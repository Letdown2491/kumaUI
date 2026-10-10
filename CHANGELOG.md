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
