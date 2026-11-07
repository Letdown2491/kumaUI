# Handoff: kumaOS ships ddcutil and the i2c access for DDC brightness

For a kumaOS session. kumaUI (this repo) never edits kumaOS; the owner
lands this change there. The prompt below is the whole job.

## Why

External monitors expose no backlight in /sys/class/backlight: the
shell's brightness widget, slider, and OSD reach built-in panels only.
kumaUI 0.8.0 adds the DDC/CI path: the shell's brightness panel probes
for panels that answer DDC/CI (through the ddcutil subprocess) and
gives each one its own slider row and OSD card. The shell-side code
degrades honestly when ddcutil is missing or no panel answers: empty
detection, no rows, no error. For the feature to exist on kumaOS, the
image needs the tool and the permission to use it.

## The change (kumaOS)

1. The `ddcutil` package ships in the image: add it to the package
   block that carries the desktop tools (the same block that carries
   brightnessctl, which the internal path already uses).
2. The user's session can open `/dev/i2c-*`: ddcutil talks DDC over
   the i2c char devices. Two pieces:
   - the `i2c-dev` module loads at boot (a modules-load.d entry), and
   - a udev rule tags the i2c devices for the seated user:
     `KERNEL=="i2c-[0-9]*", TAG+="uaccess"`.
   Without the rule the detect runs and answers nothing (the devices
   refuse rootless reads), which looks identical to "no DDC panels"
   and is the one wrong-looking failure mode: the honest absence
   should mean absent hardware, not missing permissions.

## Verify (after the image lands)

    ddcutil detect              # lists the external panels as the user
    ddcutil getvcp 10 -d 1      # a Current/Max value line per panel

Then on the running system: the brightness panel (the bar's brightness
widget, click) shows one row per external panel with its model name;
dragging a row moves the monitor's backlight; the OSD card names the
panel ("Brightness (LG HDR 4K)"). A machine with no external panel
shows exactly what it showed before.

## Rollback

Drop the package and the rule. The shell's probe answers nothing and
the DDC rows never appear; no shell state to clean. The kumaUI pin
bump can stay or ride back with it: the feature is inert without
ddcutil, not broken.
