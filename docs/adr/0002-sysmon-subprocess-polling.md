# ADR-0002: System monitors poll via subprocesses; D-Bus adapter comes later

Status: accepted (2026-10-01)

## Context

The System monitors module needs battery, volume, bluetooth, network, and
recording state. The canonical sources are D-Bus services (UPower, BlueZ,
NetworkManager, PipeWire), but wiring zbus connections, signal subscriptions,
and property caching is a large chunk of work with real failure modes in a
minimal session.

Spawning `wpctl`, `nmcli`, and `bluetoothctl` every 2s is crude but simple,
read-only, and failure-tolerant (a missing binary hides the widget).

## Decision

Poll subprocesses/procfs on a 2s tick. The parse logic is extracted as pure
functions (unit-tested) separate from the process spawns.

## Consequences

- Three to five short-lived processes every 2s while the shell runs. Fine for
  now; measure before "optimizing".
- Cadence update 2026-10-02: the audio/brightness trio rides a 500ms fast
  pass (a changed widget and the OSD should not wait 2s); everything else
  keeps the 2s full pass. ADR-0009 has the confirm gate that keeps the
  faster poll honest.
- When the notification daemon and system tray arrive (they need zbus
  anyway), revisit this ADR: the poll readers sit behind an adapter seam
  (`sysmon.rs` readers), so a D-Bus adapter replaces the subprocess adapter
  without touching the bar.
