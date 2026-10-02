# ADR-0006: Volume changes flow through one seam; the Bar's optimistic write lives inside it

Status: accepted (2026-09-30)

## Context

Three uncoordinated "change volume" paths existed: the Bar computed the new
percent from its ≤2s-old snapshot, wrote it optimistically into `SysMon`,
and spawned wpctl; the MSG CLI re-read live volume, clamped, and spawned;
mute was duplicated in both. The optimistic write makes scroll feel instant
but writes into what CONTEXT.md calls a polled snapshot.

## Decision

One seam on the System monitors owns every volume/mute change: an entity
method (`SysMon::request_volume` / `request_mute_toggle`) for the GUI
(optimistic write, spawn, rollback on error), plus the existing cx-free
functions for the MSG CLI, both sharing one pure clamp/rounding core.
The Bar never touches `SysMon.volume` directly.

## Consequences

- The optimistic write is deliberate: routing scroll bursts through the
  CLI's read-live path would spawn ~20 processes/sec (rejected alternative).
- Rollback on spawn error keeps the snapshot honest between 2s polls; the
  next poll reconciles any drift.
- The GUI clamps against the possibly-stale snapshot; staleness
  self-corrects on the next poll.
