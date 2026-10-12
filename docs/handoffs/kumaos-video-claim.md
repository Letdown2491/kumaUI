# Handoff: kumaOS video and audio claims move to Koguma

For a kumaOS session. kumaUI (this repo) never edits kumaOS; the owner
lands this change there. The prompt below is the whole job.

## Why

Issue #56's bar: inline playback must beat Open in Celluloid before it
replaces it. The owner's verdict is in: Koguma's Quick Look plays
video and sound with the seek bar, volume, and F11 fullscreen, and
decode attempts VA-API first. The image still pins video and audio
MIME types to Celluloid, so Enter and double-click leave for another
app. The claim moves to Koguma.

## Preconditions (all must be true on the kumaUI side before the flip)

- [ ] The standalone viewer plays video: `kuma-files <video>` must land
  in a playing surface, not a card. Without this the flip trades
  Celluloid for a picture of a filmstrip. (kumaUI work, tracked below;
  reuses the Quick Look player in `kuma-files/src/video.rs`.)
- [ ] `kuma-files/packaging/kuma-files.desktop.in` declares the same
  MIME types the claim pins (the list is below). It lands in the same
  kumaUI commit as the viewer's video pane.
- [ ] The image's kumaUI pin is bumped past that commit, so the
  installed desktop file actually carries the new `MimeType=` line.

The flip and the pin bump are one image change: a claim pointing at a
build without the pane or the declaration is a regression for every
video on the disk.

## The change (kumaOS, blocks.rs)

The MIMEAPPS block at `blocks.rs` lines 2562 to 2569 pins these types
to `io.github.celluloid_player.Celluloid.desktop`:

    video/mp4, video/webm, video/ogg, video/x-matroska,
    audio/mpeg, audio/flac, audio/ogg, audio/webm

Repoint that same list at `kuma-files.desktop`. Nothing else in the
hunk moves:

- Celluloid stays shipped (its Flathub declaration stays: niri.toml
  line 81, cosmic.toml line 43, vm.kuma.toml line 36). The claim moves;
  the app does not leave the image.
- The freeworld VA-API driver block (lines 360 to 371) stays as it is;
  its version-lag guard traces to the greeter segfault (kumaOS#36).
- If the image/svg+xml claim from issue #25 is still pending, the same
  hunk can take it; the owner decides whether the two move together.

## Verify (in the kumaOS session, after the image lands)

    xdg-mime query default video/mp4     # kuma-files.desktop
    xdg-mime query default audio/flac    # kuma-files.desktop

Then on the running system: Enter on a video in Koguma's listing opens
Koguma's viewer playing; double-click from another manager does the
same; `xdg-open <video>` from a terminal too.

## Rollback

Revert the hunk (the types point back at Celluloid's desktop file) and
bump the pin. No other state to clean: the claim is the whole change.
