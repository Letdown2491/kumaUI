# ADR-0001: Vendor GPUI from zed main instead of crates.io

Status: accepted (2026-10-01)

## Context

kuma-shell's UI framework is GPUI. The crates.io release (`gpui` 0.2.x) is an
old snapshot: no layer-shell API, and several layer-shell sizing fixes are
still open PRs on zed main. The shell's core feature, a bar plus drawers on
niri, depends on that API.

## Decision

Vendor the zed repository at a pinned commit into `vendor/zed/` (gitignored)
and depend on gpui via a path dependency. `scripts/vendor-gpui.sh` re-vendors;
the pin lives in `VENDORED.md`.

## Consequences

- Upstream layer-shell bug fixes require re-vendoring or a local patch to
  `vendor/zed/` (patch notes go in `VENDORED.md`).
- We do not get gpui updates automatically; updating is a deliberate act.
- If gpui ever publishes a crates.io release with layer-shell + the sizing
  fixes, revisit: the vendored tree is ~600MB on disk and this ADR exists to
  be retired.
