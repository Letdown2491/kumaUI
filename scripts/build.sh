#!/usr/bin/env bash
# Builds kuma-shell inside a container (kumaOS is atomic: no host toolchain).
# Derives localhost/kuma-dev-rust from kuma-dev-gcc on first use; rustup and
# the cargo registry live in named volumes so later builds are incremental.
#
#   ./scripts/build.sh build --release
#   ./scripts/build.sh run -p kuma-shell
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE=localhost/kuma-dev-rust

if ! podman image exists "$IMAGE"; then
  podman build -t "$IMAGE" -f containers/kuma-dev-rust.containerfile containers/
fi

podman run --rm \
  -v "$PWD":/work:Z -w /work \
  -v kumaui-cargo-home:/root/.cargo \
  -v kumaui-rustup-home:/root/.rustup \
  "$IMAGE" \
  bash -c '
    if [ ! -x /root/.cargo/bin/cargo ]; then
      curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs |
        sh -s -- -y --profile minimal --default-toolchain 1.98.1
    fi
    export PATH=/root/.cargo/bin:$PATH
    cargo "$@"
  ' _ "$@"

# keep the binary niri's `spawn` finds fresh, and install kuma-files
# for the session: binary, menu entry, icon, directory-handler binding
if [ "${1:-}" = "build" ]; then
  mkdir -p ~/.local/bin
  # rename(2) over a running binary works where cp fails with Text file busy
  cp target/release/kuma-shell ~/.local/bin/kuma-shell.new
  mv -f ~/.local/bin/kuma-shell.new ~/.local/bin/kuma-shell
  # kuma-term: binary plus menu entry, so Mod+T and the launcher find it
  cp target/release/kuma-term ~/.local/bin/kuma-term.new
  mv -f ~/.local/bin/kuma-term.new ~/.local/bin/kuma-term
  mkdir -p ~/.local/share/applications \
    ~/.local/share/icons/hicolor/256x256/apps
  sed "s|@HOME@|$HOME|" kuma-term/packaging/kuma-term.desktop.in \
    > ~/.local/share/applications/kuma-term.desktop
  # png, not svg: same reason as Koguma's, the shell tints svgs
  cp kuma-term/packaging/kuma-term.png \
    ~/.local/share/icons/hicolor/256x256/apps/kuma-term.png
  if [ -f target/release/kuma-files ]; then
    cp target/release/kuma-files ~/.local/bin/kuma-files.new
    mv -f ~/.local/bin/kuma-files.new ~/.local/bin/kuma-files
    mkdir -p ~/.local/share/applications \
      ~/.local/share/icons/hicolor/256x256/apps
    # one template, two substitutions: @BIN@ is $HOME/.local/bin here,
    # /usr/bin in the image (kumaOS's kuma-shell action substitutes
    # that side)
    sed "s|@BIN@|$HOME/.local/bin|" kuma-files/packaging/kuma-files.desktop.in \
      > ~/.local/share/applications/kuma-files.desktop
    # png, not svg: the shell tints svg icons with the theme text
    # color, which eats the bear; raster icons decode true-color
    cp kuma-files/packaging/kuma-files.png \
      ~/.local/share/icons/hicolor/256x256/apps/kuma-files.png
    # "Open folder" from other apps lands in Koguma
    if command -v xdg-mime >/dev/null 2>&1; then
      xdg-mime default kuma-files.desktop inode/directory
    fi
  fi
fi
