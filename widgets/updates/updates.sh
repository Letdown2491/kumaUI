#!/bin/sh
# The stock update chip: prints a Waybar-contract JSON object when a
# bootc staged deployment waits for a reboot; prints nothing, so the
# widget hides, when the system is current or the status is unreadable.
# bootc status wants root: on kumaOS the image grants the read (see
# docs/widgets-deploy.md); elsewhere the chip stays hidden.

status=$(bootc status --json 2>/dev/null) ||
  status=$(sudo -n bootc status --json 2>/dev/null) ||
  exit 0

case "$status" in
*'"staged": {'*)
  printf '{"text": "reboot to apply", "tooltip": "A staged system update waits for a reboot to become active.", "class": "urgent"}\n'
  ;;
esac
