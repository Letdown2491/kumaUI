# kumaUI

A Wayland desktop shell built on [gpui](https://github.com/zed-industries/zed) (Zed's GPU-accelerated UI framework), written for kumaOS, an atomic Fedora-based system running the [niri](https://github.com/YaLTeR/niri) compositor.

The binary is `kuma-shell`: a top bar, an app launcher, drawer panels, and a session lock screen, all of them layer-shell surfaces rendered by gpui. Beside it, `kuma-files` (Koguma) is the desktop's own file manager.

![The kuma-shell bar (workspaces, clock, widget cluster) and the app dock over the wallpaper](docs/kumaUI.png)

## What's in the shell

- **Bar**: a top strip of widgets (workspaces, window title, apps, cpu, volume, brightness, media, battery, clock, bluetooth, internet, notifications, system tray, nostr signer), each enable/disable and placeable from the settings panel. Geometry applies live; the layer surface is always a full-width transparent window.
- **Dock**: pinned favorites plus running windows on its own layer-shell surface at a screen edge. Click cycles an app's windows (or focuses its first), right-click pins or unpins, and position and visibility come from the settings panel live.
- **Launcher**: an app-list panel with search-as-you-type, fuzzy scoring, and most-used-first ordering from per-app usage counts.
- **Panels**: drawer surfaces hanging flush under the bar (concave cove silhouette), opened with a scrim click-catcher. Panels with tabs use the shared panel kit: an icon-only rail with hover tooltips and the tab header over the right pane; settings carries the Widgets and Ordering pages.
- **Lock screen**: opened by logind's session `Lock` signal. Opaque wallpaper-backed surfaces on every display, exclusive keyboard, one shared password field, and PAM authentication through a service chain (`kuma-lock` → `swaylock` → `vlock`; the first installed service wins, see [ADR-0008](docs/adr/0008-pam-chain-skips-uninstalled-services.md)).
- **System monitors**: polled snapshots (battery, volume, mic, brightness, cpu, bluetooth, network, media, power profile) behind an adapter seam, with a serialized request queue for audio and brightness changes.
- **OSD**: a centered toast card under the bar for volume, mute, microphone, and brightness changes, whichever surface they come from (the shell, a keybind, or an outside tool like wpctl).
- **msg CLI**: `kuma-shell msg <verb>`, thin client subcommands (volume up/down/mute, mic-mute, brightness up/down, media, launcher, notifications, nostr, workspace) that niri keybinds spawn, riding a unix socket into the running shell. Audio and brightness verbs prefer the socket and fall back to standalone `wpctl`/`brightnessctl` when no shell is running.

## What's in the file manager

Koguma (`kuma-files`) is the workspace's second app: kuma's own file manager, replacing Thunar ([issue #25](https://github.com/Letdown2491/kumaUI/issues/25)). Distro-agnostic by construction: every kumaOS integration is opportunistic and falls back to built-ins. Phase 1 (local browsing) is complete:

- **Browsing**: tabs, places sidebar (XDG user dirs plus the GTK bookmarks file, drag-ordered), trash and recent-files sources, in-place rename, compress and extract riding file-roller/tar/unzip, open-with over desktop entries, terminal spawn, an info rail with text, PDF, and image previews.
- **File operations**: one serialized op queue, undo, a keyboard-first conflict dialog, drags in both directions (outbound Copy-only, per the DnD spike finding).
- **Search**: type-to-filter over the listing (nucleo-scored) plus a debounced background subtree walk appending deep hits.
- **Theming**: reads the shell's palette opportunistically and recolors on wallpaper changes; built-ins everywhere else. Accent picker.
- **Installed**: `build.sh` puts the binary in `~/.local/bin`, the menu entry and app icon in the user XDG dirs, and registers Koguma as the directory handler.

## Building

```
./scripts/build.sh build --release
```

kumaOS is atomic and has no host toolchain, so the script builds inside a podman container (`localhost/kuma-dev-rust`, built on first use from `containers/`) and installs: the shell binary to `~/.local/bin/kuma-shell`, and Koguma to `~/.local/bin/kuma-files` plus its menu entry, app icon, and directory-handler binding in the user XDG dirs. Builds are incremental; rustup and the cargo registry live in named podman volumes.

On a regular Linux box, a plain `cargo build --release` works too; you need a Rust toolchain (see `rust-toolchain.toml`) and the dev headers the containerfile installs: `pkgconf-pkg-config fontconfig-devel freetype-devel libxkbcommon-devel pam-devel`.

Runtime requirements: a Wayland compositor with `wlr-layer-shell` and `ext-session-lock`, and PAM. niri is the primary target; sway works end to end (the bar's workspaces, window title, and dock ride a compositor-neutral session mirror, [ADR-0012](docs/adr/0012-compositor-neutral-session-state.md)). Hyprland speaks the same protocols but is untested. Koguma's requirements are lighter: any Wayland compositor or X11, no layer-shell.

## Vendored gpui

kuma-shell depends on gpui via a path dependency into `vendor/zed/` (gitignored, regenerated from a pinned zed `main` commit). The crates.io `gpui` release (0.2.2) predates the Wayland layer-shell API this shell is built on. See [VENDORED.md](VENDORED.md) for the why and the re-vendoring script.

## Repository notes

- [`CONTEXT.md`](CONTEXT.md): domain glossary (the names of the seams in the code, one line each) and the four-rule ethos that governs them.
- [`docs/adr/`](docs/adr/): architecture decision records, including two about lock-screen failure modes that only ever show up as "PAM rejects the password the user typed correctly".
- [`AGENTS.md`](AGENTS.md): how AI agents should build, run, and debug in this repo.
- `kuma-shell/icons/`: the widget and panel icons, hand-made SVGs.
- `kuma-files/icons/`: the in-app icons, hand-made SVGs (16px stroke style).
- `kuma-files/packaging/`: the menu entry, the app icon generator, and the app icon.

## Status

Personal project, developed on a single machine against kumaOS 44 + niri. Not packaged for general distribution yet.

## License

[MIT](LICENSE)
