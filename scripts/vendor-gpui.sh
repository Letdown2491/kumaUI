#!/usr/bin/env bash
set -euo pipefail

# The crates.io release of gpui (0.2.x) is an old snapshot without the
# layer-shell API, so we vendor gpui from zed main at a pinned commit.
# See VENDORED.md.
SHA="${1:-40180d9c40e2d20eb63d388bff920818f2910b53}"

mkdir -p vendor
curl -sL "https://codeload.github.com/zed-industries/zed/tar.gz/${SHA}" -o /tmp/zed-main.tar.gz
rm -rf vendor/zed
tar -xzf /tmp/zed-main.tar.gz -C vendor
mv "vendor/zed-${SHA}" vendor/zed
echo "vendored zed at ${SHA}"
