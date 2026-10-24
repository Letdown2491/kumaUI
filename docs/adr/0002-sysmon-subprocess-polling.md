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
- Cadence update 2026-10-23: the fast pass's reads (wpctl, the recording
  /proc walk) run on the background executor, so a wedged PipeWire costs
  a poll tick and not the paint; the state seam is unchanged.
- Update 2026-10-23, the network slice: it no longer polls (the watcher
  `nm.rs` arrived with #88; this note catches the ADR up). It listens once
  on NetworkManager's service-wide signal stream and recomputes the state
  per burst, folded into a single read per quiet window; the nmcli poll
  stands down while the watcher is healthy and remains the fallback plus
  the Wi-Fi panel's scan seam. The Bluetooth slice still polls
  (`bluetoothctl` per device); its per-device `info` spawns stay a known
  cost, skipped-for-unpaired risks panel correctness.
- When the notification daemon and system tray arrive (they need zbus
  anyway), revisit this ADR: the poll readers sit behind an adapter seam
  (`sysmon.rs` readers), so a D-Bus adapter replaces the subprocess adapter
  without touching the bar.
