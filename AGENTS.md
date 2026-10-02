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

- `kuma-shell.service` runs the image binary `/usr/bin/noctalia`, **not**
  your build. To test dev changes, stop the service and run
  `~/.local/bin/kuma-shell` in its place, because two shells fight over the layer
  surfaces and the logind lock listener.
- Lock screen: `loginctl lock-session` to lock, password to unlock,
  `loginctl unlock-session` as a backdoor.
- Shell logs: `journalctl --user -u kuma-shell` for the service, or the
  terminal output of a manually run dev shell. PAM unlock failures are
  logged at info level (`unlock attempt failed: ...`).

## Writing

No em dashes in documentation (README, CONTEXT.md, VENDORED.md, ADRs, and
code comments). Use commas, colons, or parentheses instead. This is a
deliberate project-owner preference (set 2026-10-02); keep it in all new
docs.
