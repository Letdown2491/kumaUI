# Handoff: kumaOS retires the baked kuma-battery-watch script

For a kumaOS session. kumaUI (this repo) never edits kumaOS; the owner
lands this change there. The prompt below is the whole job.

Planned for the kumaOS ride after kumaUI 0.9.0, tracked in kumaUI
issue #110 (the kumaOS-side half; the shell half landed as kumaUI
commit 8004f4c). The end-state decision is kumaOS#80's: the baked
kuma-battery-watch script plans its own absorption, "when the shell
grows threshold notifications, this is the first thing to go." The
shell has grown them.

## Why

kuma-shell now watches the battery itself: it rides
`udevadm monitor --kernel --subsystem-match=power_supply` with a 300
second die-of-old-age safety read (the respawn is the poll, the same
shape the script uses), evaluates the same thresholds (critical at
<=5% urgent, low at <=15%), warns once per band with any non-
Discharging status clearing the state, and warns from its first read
on start. It pushes through the shell's own notification daemon, so
the notify-send dependency the script had (a notification daemon on
the session bus) is gone too: the shell is the daemon. The script's
one remaining behavior the shell does not replicate is its flock,
which guarded against two instances; the shell is single-instance per
session, so the guard has no job. Every 60-second poll wakeup the
script cost (1,440 a day) now belongs to the script's retirement.

## The change (kumaOS)

1. Retire the `kuma-battery-watch` script from
   `src/containerfile/blocks.rs` (the `BATTERY_WATCH` block, staged in
   the image and wired into the niri config).
2. Drop the niri config line that spawns it at session start.
3. Ride the kumaUI pin to 0.9.0 (the release that ships the shell-side
   notifier), the version/commit ride the image's kumaUI gate already
   reads.
4. Recompose.

The containerfile test that pins the script's shape (the monitor form,
the 300s cadence, the udevadm fallback) retires with it: the pin's
text lives or dies with the thing it pins.

## Verify (after the image lands)

On a machine with a battery: drain past 15% while discharging and see
"Battery low: N%" once, not once per capacity step; replug, drain back
through the band, see it warn again; at 5% see "Battery critical: N%"
with "Plug in now." and urgent urgency. A session that opens at 12% on
battery warns at once. On AC or with no battery, nothing ever appears.
The script is absent from the image (`ostree`/`ls` on the staged path)
and the journal shows no battery-watch spawn at login.

## Rollback

Re-land the block and the config line (git history in the kumaOS repo
keeps both). The shell's notifier and the script can coexist: the
script's flock is per-script, the shell does not contend, and the
worst case is double notifications while both run. The kumaUI pin can
stay: the notifier is silent where the battery is absent or on AC.
