# kumaUI

## Build

kumaOS is atomic: **there is no host toolchain**. Always build with:

    ./scripts/build.sh build --release

This runs cargo inside the `localhost/kuma-dev-rust` podman container
(built on first use from `containers/`). It installs `kuma-shell` to
`~/.local/bin/kuma-shell`; when `target/release/kuma-files` exists it
also installs Koguma: the binary, the menu entry, the app icon, and
the directory-handler binding (all user-local).

Do not build on the host and do not try to shim a toolchain: there is no
`cc`, and brew's gcc-16 fails at link time (`rust-lld` cannot find
crti.o/glibc libraries). Same for `cargo check`/`cargo test` outside the
container. The container keeps rustup and the cargo registry in named
volumes, so builds are incremental.

## Run / test

- Production runs the user service `kuma-shell.service`. Its drop-in
  (`~/.config/systemd/user/kuma-shell.service.d/override.conf`) points
  `ExecStart` at `~/.local/bin/kuma-shell` (which build.sh installs); the
  image's `/usr/bin/kuma-shell` stays untouched. The whole update loop is:

      ./scripts/build.sh build --release && systemctl --user restart kuma-shell

  Two shells cannot run at once: they fight over the layer surfaces and the
  logind lock listener. Never launch the binary by hand while the service is
  up; to run it by hand, stop the service first.
- The service logs errors only (no RUST_LOG). To diagnose in place, add
  `Environment=RUST_LOG=info` to the override and restart; the per-request
  `msg:` lines, the `surfaces:` recreate lines, the `session:` mirror
  transitions (`session: workspaces N (focused X), windows M (focused Y)`),
  OSD cards, and poll snapshots appear in `journalctl --user -u kuma-shell`.
  PAM unlock failures log at info level (`unlock attempt failed: ...`).
- Lock screen: `loginctl lock-session` to lock, password to unlock,
  `loginctl unlock-session` as a backdoor.
- Tests run inside the build container (no host toolchain):

      ./scripts/build.sh test --release -p kuma-shell

- Koguma (`kuma-files`) runs as a plain user process, no service:

      pkill -x kuma-files; sleep 1; setsid env RUST_LOG=info \
        ~/.local/bin/kuma-files > /tmp/opencode/koguma.log 2>&1 < /dev/null &

  A bare launch restores tabs from the state file; a directory argument
  defeats restore. Two instances never fight: the second hands its dir
  over the activation socket and exits. Menu-launch equivalent: the
  desktop entry in `~/.local/share/applications/`.
- Koguma test notes (same container):

      ./scripts/build.sh test --release -p kuma-files

  - TestApp::with_text_system_and_assets(CosmicTextSystem, icons::Assets);
    `app.run_until_parked()` after open_window; `app.advance_clock(Duration)`
    passes the 150ms search debounce timer.
  - `debug_element_bounds(selector)` keys off `.debug_selector(|| ...)`,
    not `.id()`.
  - if/else render branches must share one element type: give every branch
    `.id(...)` (ids nest under the row's `.id(ix)`, staying unique).
  - `Keystroke::parse(k)` leaves `key_char` None; the rename and compress
    buffers insert from `key_char`, so printable-key tests must set
    `keystroke.key_char = Some(...)`.
  - The notify 8.x watch mask includes OPEN: any readdir of a watched
    dir's children fires events; the watcher pump drops
    `EventKind::Access` wholesale (else the walker's own readdir
    re-triggers the reload, a self-sustaining loop).

- The displayless smoke runs the whole surface lifecycle plus the session
  mirror under a headless sway, in a second container image (niri cannot
  start in a container; sway stands in). It needs the release binary the
  build step produced, mounted at `/work/target`:

      podman build -t localhost/kuma-test-compositor \
        -f containers/kuma-test-compositor.containerfile containers/
      podman run --rm -v "$PWD":/work:Z localhost/kuma-test-compositor \
        /work/scripts/displayless-smoke.sh

  The memory probe (`scripts/memory-probe.sh`) uses the same image and the
  same mount.

## Release

A reader-facing change lands with its changelog bullet in the same
commit: a bullet under `## Unreleased` in `CHANGELOG.md`, grouped under
`### Added`, `### Changed`, or `### Fixed` (test-only and docs-only
work earns no bullet). Cut a release when the owner asks for one:

    ./scripts/release.sh

The script infers the bump from the subsections (any Added or Removed:
minor; only Changed and Fixed: patch), asks which apps move, bumps
their Cargo.tomls, inserts the release heading and the three version
lines, refreshes the lock and installs through the build, and commits
(`--tag` also tags vN). A test in each crate fails when Cargo.toml and
the changelog's newest version line disagree, so a version bump
without changelog bullets will not go green.

## Writing

No em dashes in documentation (README, CONTEXT.md, VENDORED.md, ADRs, and
code comments). Use commas, colons, or parentheses instead. This is a
deliberate project-owner preference (set 2026-10-02); keep it in all new
docs.
