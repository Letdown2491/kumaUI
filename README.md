# kumaUI

A Wayland desktop shell built on [gpui](https://github.com/zed-industries/zed) (Zed's GPU-accelerated UI framework), written for kumaOS, an atomic Fedora-based system running the [niri](https://github.com/YaLTeR/niri) compositor.

The binary is `kuma-shell`: a top bar, an app launcher, drawer panels, and a session lock screen, all of them layer-shell surfaces rendered by gpui. Beside it, `kuma-files` (Koguma) is the desktop's own file manager, `kuma-term` (Higuma) the terminal, and `kuma-monitor` (Kamuy) the system monitor.

![The kuma-shell bar (workspaces, clock, widget cluster) and the app dock over the wallpaper](docs/kumaUI.png)

## What's in the shell

- **Bar**: a top strip of widgets (workspaces, window title, apps, cpu, memory, temperature, disk, volume, microphone, brightness, power profile, media, battery, clock, bluetooth, internet, notifications, system tray, nostr signer, weather, Kuma Updates), each enable/disable and placeable from the settings panel, each with its own mode dial (icon / icon+text / text where they fit; the clock's is date+time / time / date; a chip that makes no sense as bare text offers no such pill). User-made script widgets list beside them: a widget is a directory with a manifest and a script whose stdout is the label ([docs/widgets.md](docs/widgets.md)). Geometry applies live; the layer surface is always a full-width transparent window. A fresh install ships apps and workspaces on the left, the clock alone in the center, and the status cluster (system tray, notifications, volume, internet, battery) on the right.
- **Kuma Updates**: a built-in chip (on by default, right cluster) fed by kumaOS's update tooling: hidden while the base is current, accented while security updates wait on you, and a panel with per-package severity glyphs, an Update now button that stages via polkit while the build's step lines stream into the panel, and a Restart now button once staged. Where the `kuma` CLI is absent (any non-kumaOS distro) it stays out of the bar, the scheduler, and the settings list entirely.
- **Dock**: pinned favorites plus running windows on its own layer-shell surface at a screen edge. Click cycles an app's windows (or focuses its first), right-click pins or unpins, and position and visibility come from the settings panel live. The dock ships off; enabling it starts from first-run pins (a terminal and a browser).
- **Launcher**: an app-list panel with search-as-you-type, fuzzy scoring, and most-used-first ordering from per-app usage counts.
- **Panels**: drawer surfaces hanging flush under the bar (concave cove silhouette), opened with a scrim click-catcher. Panels with tabs use the shared panel kit: an icon-only rail with hover tooltips and the tab header over the right pane; settings carries the Widgets and Ordering pages.
- **Lock screen**: opened by logind's session `Lock` signal. Opaque wallpaper-backed surfaces on every display, exclusive keyboard, one shared password field, and PAM authentication through a service chain (`kuma-lock` → `swaylock` → `vlock`; the first installed service wins, see [ADR-0008](docs/adr/0008-pam-chain-skips-uninstalled-services.md)).
- **System monitors**: polled snapshots (battery, volume, mic, brightness, cpu, bluetooth, network, media, power profile) behind an adapter seam, with a serialized request queue for audio and brightness changes.
- **OSD**: a centered toast card under the bar for volume, mute, microphone, and brightness changes, whichever surface they come from (the shell, a keybind, or an outside tool like wpctl).
- **msg CLI**: `kuma-shell msg <verb>`, thin client subcommands (volume up/down/mute, mic-mute, brightness up/down, media, launcher, settings, updates, notifications, nostr, workspace) that niri keybinds spawn, riding a unix socket into the running shell. Audio and brightness verbs prefer the socket and fall back to standalone `wpctl`/`brightnessctl` when no shell is running.

## What's in the file manager

`kuma-files` (Koguma) is the workspace's second app: kuma's own file manager, replacing Thunar ([issue #25](https://github.com/Letdown2491/kumaUI/issues/25)). Distro-agnostic by construction: every kumaOS integration is opportunistic and falls back to built-ins. Phases 1 (local browsing) and 2 (network places) are complete:

- **Browsing**: tabs, a places sidebar in sections (Home, Recent, and Trash up top; then Places: XDG user dirs and GTK bookmarks, drag-ordered; Network: gvfs bridges for samba and friends, plus stale network bookmarks that reconnect; Removable: USB drives, phones, and cameras), trash and recent-files sources, in-place rename, compress and extract riding file-roller/tar/unzip, open-with over desktop entries, terminal spawn, an info rail with text, PDF, image, and SVG previews.
- **Network**: mounted volumes and remote shares list under the sidebar's Network section: gvfs-FUSE bridges get human labels like `host/share (smb)`, udisks2 mounts keep their volume labels, and mounts appear and disappear with the 2s places tick. Connect to Server opens a dialog with a protocol picker (SFTP, FTP, SMB) whose fields reshape per protocol (an optional password pre-answers gio's first password prompt), then spawns `gio mount` and relays its prompt protocol (passwords masked, host-key questions included). Pasting a server URI fills the form in one go. Right-click disconnects a network share or ejects a removable drive (`gio mount -u`; Koguma steps its own tab out of the mount first), and a network bookmark whose mount is gone stays listed dimmed as "(not connected)": clicking it reopens the connect dialog prefilled with the share's URI.
- **File operations**: one serialized op queue with a byte-counting progress line (`copying 3 of 500, 4.2 GiB / 9.1 GiB`) and a Cancel button while it runs (the stop lands between files, between directory entries, and between copy chunks; what already landed stays and undoes), undo, a keyboard-first conflict dialog, drags in both directions (outbound Copy-only, per the DnD spike finding).
- **Search**: type-to-filter over the listing (nucleo-scored) plus a debounced background subtree walk appending deep hits. The banner's chip flips that walk to names + contents: files whose bytes carry the query stream into the same result list, each row showing its folder and the first matching line. Names sort naturally (file2 before file10), and Size and Modified are joined by a Type sort key and column (folder, image, text, archive, and friends).
- **Sizes and kinds**: the info rail measures a selected folder's tree in the background and caches the total (keyed by the folder's mtime); the listing task sniffs the first 4 KB of files whose extension says nothing, so an extensionless png shows as an image and a jpeg wearing a `.txt` name keeps its bytes.
- **Theming**: reads the shell's palette opportunistically and recolors on wallpaper changes; built-ins everywhere else. Accent picker.
- **Installed**: `build.sh` puts the binary in `~/.local/bin`, the menu entry and app icon in the user XDG dirs, and registers Koguma as the directory handler.

**Open surface**: Koguma is also the desktop's default opener for images, PDFs, SVGs, and videos. Enter or double-click opens such a file in a standalone viewer window: the Quick Look peek's renderer underneath, with the real viewer behaviors added (zoom, pan, rotate for images and SVGs; page navigation for PDFs; video playback with the footer transport: space pauses and resumes, J/L seek by ten seconds, +/- volume, M mute, F11 fullscreen), and the arrow keys walk the containing directory's files of the same kind. `xdg-open` from anywhere (a browser download, a terminal) lands in the same window. The claim follows the surface: a type the viewer cannot render yet stays unclaimed, so audio files (and video/ogg) keep handing off to the system's default (Celluloid on the appliance) until the viewer grows an audio pane, and avif stays unclaimed until a decoder exists.


A fresh start opens at 125% zoom with the info rail docked at the bottom; the state file keeps whatever you change.

## What's in the terminal

`kuma-term` (Higuma) works with any shell out of the box; no shell integration is required. It exports `KUMA_TERM=1` into the shell's environment so dotfiles can recognize it (the fish prompt and greeting in the kumaOS image key on it), and nothing else. `kuma-term -e <command...>` runs the command in place of the shell (the kitty/alacritty convention): everything after `-e` is the command, executed directly, and the terminal closes when it exits; this is how kuma-launch runs its verb scripts. Its theme is a line-based key value config at `$XDG_CONFIG_HOME/kuma-term/kuma-term.conf` with kitty-compatible key names, so a kitty theme's colors block pastes in unchanged, and it honors `kuma-shell`'s published wallpaper palette when the shell is present. The built-in theme, used when no conf exists, is the shipped pastel palette (the ANSI 16 and the chrome colors).

Everyday terminal features: scrollback search (`ctrl+shift+f`, kitty's binding: a floating bar that never reflows the grid, case-insensitive matching unless the query has uppercase, `ctrl+enter` toggles regex mode with an inline "bad pattern" status, enter and the arrows cycle matches through history, `esc` closes), URL click-to-open through the desktop handler, vector-drawn box drawing and powerline glyphs, and font zoom (`ctrl+=` and `ctrl+-` step a point, `ctrl+0` returns to the configured size; the program on screen reflows the way a window resize reflows it). A right-click opens a small Copy / Paste menu at the pointer, and `copy_on_select true` lands every finished selection on the clipboard by itself. The terminal speaks the kitty keyboard protocol too: a program that pushes the disambiguation mode gets `shift+enter`, `ctrl+tab`, and `ctrl+letter` as distinct escapes instead of legacy bytes, and without the push every keystroke encodes exactly as it always did. Clipboard writes through OSC 52 ask first: a small card at the prompt shows a content preview and waits for y to allow or n or escape to deny, so a stray `printf` can no longer silently rewrite the clipboard.

## What's in the monitor

`kuma-monitor` (Kamuy) is the desktop's own system monitor. It watches first and acts second: the arrow keys walk the table (wrapping, the view following), `ctrl+k` sends the cursor row's process one SIGTERM, and a second `ctrl+k` arms a confirm card whose Enter sends SIGKILL, since a process that ignored the polite ask is killed on purpose or not at all; it renices nothing. One window: the CPU graph, a row of vitals cards (cpu, memory, temperature when a sensor exists, runnable tasks), and the process table under them. The table speaks in apps, not processes: every process an app owns folds under one row (the windowed process and its helpers together), a click unfolds the member roster in place, and the session's background plumbing hides behind one summary line. The memory column is private pages, not RSS. Type-to-filter, sparklines behind the rows, and drop-down cards for per-core bars, memory anatomy, disks, and interfaces fill it out; the sort and filter persist across launches in a small state file under `~/.config/kuma-monitor`. Like the terminal it wears the shell's published palette and a translucent background, recoloring when the wallpaper changes.

The titlebar says Kamuy, the Ainu spirit that dwells in the bear: the family's watcher. The binary and app id stay `kuma-monitor`; `build.sh` installs it like the rest, binary, menu entry, and app icon.

## Building

```
./scripts/build.sh build --release
```

kumaOS is atomic and has no host toolchain, so the script builds inside a podman container (`localhost/kuma-dev-rust`, built on first use from `containers/`) and installs: `kuma-shell` to `~/.local/bin/kuma-shell`, `kuma-term` (Higuma) to `~/.local/bin/kuma-term`, and `kuma-files` (Koguma) and `kuma-monitor` (Kamuy) to `~/.local/bin` when their binaries were built, each with its menu entry and app icon in the user XDG dirs, plus Koguma's directory-handler binding. Builds are incremental; rustup and the cargo registry live in named podman volumes.

On a regular Linux box, a plain `cargo build --release` works too; you need a Rust toolchain (see `rust-toolchain.toml`) and the dev headers the containerfile installs: `pkgconf-pkg-config fontconfig-devel freetype-devel libxkbcommon-devel pam-devel`.

Runtime requirements: a Wayland compositor with `wlr-layer-shell` and `ext-session-lock`, and PAM. niri is the primary target; sway works end to end (the bar's workspaces, window title, and dock ride a compositor-neutral session mirror, [ADR-0012](docs/adr/0012-compositor-neutral-session-state.md)). Hyprland speaks the same protocols but is untested. Koguma's requirements are lighter: any Wayland compositor or X11, no layer-shell.

## Video and libav

Koguma's video posters, Quick Look tiles, and in-process video playback (Quick Look and the standalone viewer) use libav (the FFmpeg libraries) through the `ffmpeg-sys-next` bindings, enabled by the default `video` cargo feature. The libraries are linked, not dlopen'd: whatever soname generation your distro's libavcodec carries at build time is the one the binary wants, so a distro that moves to a different soname generation needs a rebuild (there is no vendoring and no shim). The image ships Fedora's libav: if a codec's decoder is missing from the system's libav, Koguma says so ("codec unavailable in this system's libav") instead of failing the listing. Building with `--no-default-features` drops libav entirely; the browser then shows a generic icon for videos and everything else works.

## Vendored gpui

kuma-shell depends on gpui via a path dependency into `vendor/zed/` (gitignored, regenerated from a pinned zed `main` commit). The crates.io `gpui` release (0.2.2) predates the Wayland layer-shell API this shell is built on. See [VENDORED.md](VENDORED.md) for the why and the re-vendoring script.

## Repository notes

- [`CHANGELOG.md`](CHANGELOG.md): the release history, kumaOS style, with each binary's version on three lines under the release heading.
- [`CONTEXT.md`](CONTEXT.md): domain glossary (the names of the seams in the code, one line each) and the four-rule ethos that governs them.
- [`docs/adr/`](docs/adr/): architecture decision records, including two about lock-screen failure modes that only ever show up as "PAM rejects the password the user typed correctly".
- [`docs/widgets.md`](docs/widgets.md): how to write a bar widget: the manifest, the stdout contract, installing and placing.
- [`AGENTS.md`](AGENTS.md): how AI agents should build, run, and debug in this repo.
- [`kuma-platform/`](kuma-platform/): the shared crate: version reporting, the shell's palette handshake, the HSL color space, and the niri IPC envelope ([ADR-0017](docs/adr/0017-kuma-platform-crate.md)).
- `kuma-shell/icons/`: the widget and panel icons, hand-made SVGs.
- `widgets/`: the widget pool directory the image ships to `/usr/share/kuma-shell/widgets/` (currently empty; the Kuma Updates chip is built in); what kumaOS does with it is [widgets-deploy.md](docs/widgets-deploy.md).
- `kuma-files/icons/`: the in-app icons, hand-made SVGs (16px stroke style).
- `kuma-files/packaging/`: the menu entry, the app icon generator, and the app icon.
- `kuma-monitor/packaging/`: the same trio for Kamuy (48-unit grid, drawn at 4x, downscaled to the committed 256px PNG).

## Status

Personal project, developed on a single machine against kumaOS 44 + niri. Not packaged for general distribution yet.

## License

[MIT](LICENSE)
