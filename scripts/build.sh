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

# keep the binary niri's `spawn` finds fresh
if [ "${1:-}" = "build" ]; then
  mkdir -p ~/.local/bin
  cp target/release/kuma-shell ~/.local/bin/kuma-shell
fi
