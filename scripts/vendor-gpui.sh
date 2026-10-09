#!/usr/bin/env bash
set -euo pipefail

# The crates.io release of gpui (0.2.x) is an old snapshot of the code
# that predates the layer-shell API, so we vendor gpui from zed main at
# a pinned commit. See VENDORED.md.
SHA="${1:-40180d9c40e2d20eb63d388bff920818f2910b53}"

mkdir -p vendor
curl -sL "https://codeload.github.com/zed-industries/zed/tar.gz/${SHA}" -o /tmp/zed-main.tar.gz
rm -rf vendor/zed
tar -xzf /tmp/zed-main.tar.gz -C vendor
mv "vendor/zed-${SHA}" vendor/zed

# The local patches travel as files under patches/ and are applied after
# the untar, so a fresh clone assembles the same vendor tree this
# checkout builds (and the committed Cargo.lock matches what both see).
# Every patch is recorded in VENDORED.md. A patch that no longer applies
# means the zed pin moved through it: re-apply the change by hand, move
# the pin, and record it, before anything here can build.
shopt -s nullglob
for p in patches/*.patch; do
    git apply --whitespace=nowarn "$p"
    echo "applied $(basename "$p")"
done
echo "vendored zed at ${SHA}"
