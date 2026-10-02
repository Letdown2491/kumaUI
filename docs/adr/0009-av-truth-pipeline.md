# ADR-0009: Audio and brightness truth: one worker, named mute targets, a confirm gate, and a bounce guard

Status: accepted (2026-10-02)

## Context

ADR-0006 gave volume changes one seam with an optimistic write. Living with
it exposed four ways the shell's belief and the machine's truth diverged:

- Out-of-order writes: rapid steps spawned wpctl calls concurrently; two
  could land swapped and lose a step.
- Blind toggles: `wpctl set-mute ... toggle` reads live state and flips it.
  Around a mute change WirePlumber re-resolves default nodes for roughly a
  second, so a toggle could land while the resolution flapped and the poll
  would then confirm the wrong world (the field case: mic toggles that
  muted and unmuted the sink).
- Outside changes: wpctl/brightnessctl run from anywhere (a terminal, a
  headset button). One transient read could apply a change that never
  happened and toast a card for it.
- Key bounce: some laptops' mute keys emit the event twice, 14-18ms apart
  (measured with millisecond log timestamps). Each physical press unmuted
  and instantly re-muted, so the mic never left its old state.

## Decision

- Every av write serializes through one background worker (`SysMon::queue`
  + `AvRequest`); each request carries the previous value for rollback on
  error.
- Mute requests carry a named target (`wpctl set-mute <obj> 0|1`), not a
  blind toggle: the shell names what the press meant, because it just
  flipped it optimistically. A blind toggle remains only for the no-belief
  case (no snapshot yet).
- The poll splits: a 500ms fast pass reads the audio/brightness trio, a 2s
  full pass refreshes everything. A trio read that differs from the state
  applies only after the next read agrees (the confirm gate); the agreeing
  re-check rides a 120ms timer instead of the next full tick. While a shell
  request is landing (250ms window), the poll skips the trio entirely.
- A mute request within 150ms of the last one for the same target is the
  bounce, and is dropped (`MUTE_BOUNCE`, per-target clocks for the sink and
  the mic).

## Consequences

- Outside changes surface in about 0.6s; one-tick garbage reads are still
  rejected. A sustained (roughly 1s) routing flap can slip two agreeing
  garbage reads through the gate: accepted residual risk, and named
  targets removed the shell's own contribution to such flaps.
- A deliberate second press inside 150ms is eaten. On bouncy keys that
  second event was never deliberate; on clean keys a human double-tap
  lands past 150ms.
- The pipeline is observable in the field: `msg:` logs every arriving
  request, OSD cards name what applied, and the logger prints
  millisecond timestamps (the bounce was invisible at second precision).
