# kumaui

A Wayland desktop shell built on [gpui](https://github.com/zed-industries/zed) (Zed's GPU-accelerated UI framework), written for kumaOS, an atomic Fedora-based system running the [niri](https://github.com/YaLTeR/niri) compositor.

The binary is `kuma-shell`: a top bar, an app launcher, drawer panels, and a session lock screen, all of them layer-shell surfaces rendered by gpui.

## What's in the shell

- **Bar**: a top strip of widgets (workspaces, window title, apps, cpu, volume, brightness, media, battery, clock, bluetooth, internet, notifications, system tray, nostr signer), each enable/disable and placeable from the settings panel. Geometry applies live; the layer surface is always a full-width transparent window.
- **Launcher**: an app-list panel with search-as-you-type, fuzzy scoring, and most-used-first ordering from per-app usage counts.
- **Panels**: drawer surfaces hanging flush under the bar (concave cove silhouette), opened with a scrim click-catcher. Panels with tabs use the shared panel kit: an icon-only rail with hover tooltips and the tab header over the right pane; settings carries the Widgets and Ordering pages.
- **Lock screen**: opened by logind's session `Lock` signal. Opaque wallpaper-backed surfaces on every display, exclusive keyboard, one shared password field, and PAM authentication through a service chain (`kuma-lock` → `swaylock` → `vlock`; the first installed service wins, see [ADR-0008](docs/adr/0008-pam-chain-skips-uninstalled-services.md)).
- **System monitors**: polled snapshots (battery, volume, mic, brightness, cpu, bluetooth, network, media, power profile) behind an adapter seam, with a serialized request queue for audio and brightness changes.
- **OSD**: a centered toast card under the bar for volume, mute, microphone, and brightness changes, whichever surface they come from (the shell, a keybind, or an outside tool like wpctl).
- **msg CLI**: `kuma-shell msg <verb>`, thin client subcommands (volume up/down/mute, mic-mute, brightness up/down, media, launcher, notifications, nostr) that niri keybinds spawn, riding a unix socket into the running shell. Audio and brightness verbs prefer the socket and fall back to standalone `wpctl`/`brightnessctl` when no shell is running.

## Building

```
./scripts/build.sh build --release
```

kumaOS is atomic and has no host toolchain, so the script builds inside a podman container (`localhost/kuma-dev-rust`, built on first use from `containers/`) and installs the binary to `~/.local/bin/kuma-shell`. Builds are incremental; rustup and the cargo registry live in named podman volumes.

On a regular Linux box, a plain `cargo build --release` works too; you need a Rust toolchain (see `rust-toolchain.toml`) and the dev headers the containerfile installs: `pkgconf-pkg-config fontconfig-devel freetype-devel libxkbcommon-devel pam-devel`.

Runtime requirements: a Wayland compositor with `wlr-layer-shell` and `ext-session-lock` (niri is the primary target), and PAM.

## Vendored gpui

kuma-shell depends on gpui via a path dependency into `vendor/zed/` (gitignored, regenerated from a pinned zed `main` commit). The crates.io `gpui` release (0.2.2) predates the Wayland layer-shell API this shell is built on. See [VENDORED.md](VENDORED.md) for the why and the re-vendoring script.

## Repository notes

- [`CONTEXT.md`](CONTEXT.md): domain glossary, the names of the seams in the code, one line each.
- [`docs/adr/`](docs/adr/): architecture decision records, including two about lock-screen failure modes that only ever show up as "PAM rejects the password the user typed correctly".
- [`AGENTS.md`](AGENTS.md): how AI agents should build, run, and debug in this repo.
- `kuma-shell/icons/`: the widget and panel icons, hand-made SVGs.

## Status

Personal project, developed on a single machine against kumaOS 44 + niri. Not packaged for general distribution yet.

## License

[MIT](LICENSE)
