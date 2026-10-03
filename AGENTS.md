# kumaui

## Build

kumaOS is atomic: **there is no host toolchain**. Always build with:

    ./scripts/build.sh build --release

This runs cargo inside the `localhost/kuma-dev-rust` podman container
(built on first use from `containers/`) and copies the binary to
`~/.local/bin/kuma-shell`.

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

## Writing

No em dashes in documentation (README, CONTEXT.md, VENDORED.md, ADRs, and
code comments). Use commas, colons, or parentheses instead. This is a
deliberate project-owner preference (set 2026-10-02); keep it in all new
docs.
