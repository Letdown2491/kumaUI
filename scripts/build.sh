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
  --env KUMA_GIT_SHA="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)" \
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
  # kuma-greeter: binary only, no menu entry or icon: the launcher never
  # starts it (greetd runs the image's copy at login); a local copy
  # keeps --version and scripting on the current build
  cp target/release/kuma-greeter ~/.local/bin/kuma-greeter.new
  mv -f ~/.local/bin/kuma-greeter.new ~/.local/bin/kuma-greeter
  mkdir -p ~/.local/share/applications \
    ~/.local/share/icons/hicolor/256x256/apps
  sed "s|@BIN@|$HOME/.local/bin|" kuma-term/packaging/kuma-term.desktop.in \
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
  if [ -f target/release/kuma-monitor ]; then
    cp target/release/kuma-monitor ~/.local/bin/kuma-monitor.new
    mv -f ~/.local/bin/kuma-monitor.new ~/.local/bin/kuma-monitor
    sed "s|@BIN@|$HOME/.local/bin|" kuma-monitor/packaging/kuma-monitor.desktop.in \
      > ~/.local/share/applications/kuma-monitor.desktop
    cp kuma-monitor/packaging/kuma-monitor.png \
      ~/.local/share/icons/hicolor/256x256/apps/kuma-monitor.png
  fi
  # kumaOS's compose stages the three entries and three icons from
  # beside the running kuma, this directory, as the RELEASE variant
  # (@BIN@ -> /usr/bin); the session copies above stay dev-local. Same
  # deal as the binaries: the release's files live together or the
  # compose refuses. Writing them here on every build keeps the
  # siblings from going silently stale on a packaging change (kumaOS
  # #46); the X-Kumaui-Stamp key is the identity the compose's gate
  # reads — presence alone shipped a stale ride once (#42), and an
  # entry can lie about behavior the same way a binary can.
  ride=$(git rev-parse HEAD 2>/dev/null || echo unknown)
  for app in kuma-files kuma-term kuma-monitor; do
    sed -e "s|@BIN@|/usr/bin|" -e "/^\[Desktop Entry\]/a X-Kumaui-Stamp=$ride" \
      "$app/packaging/$app.desktop.in" > ~/.local/bin/"$app".desktop
    cp "$app/packaging/$app.png" ~/.local/bin/"$app".png
  done
fi
