#!/usr/bin/env bash
# The displayless smoke: the Shell must idle through zero outputs with its
# bus names held, and bring its surfaces back when an output appears. It
# also asserts the session mirror end to end: the sway adapter detected at
# startup, workspace switches and the msg command path reporting back, and
# a real window (foot) opening and closing in the mirror.
#
# A compositor with no outputs is the condition: kumaOS CI reaches it with
# qemu (virtio-vga, display none) and niri; this script reaches it with a
# wlroots sway started headless. sway retires an unplugged output's
# wl_output global asynchronously, so the prelude waits the retirement
# out and verifies a fresh client sees no globals before the Shell starts.
#
# Run inside the container (the script assumes that, not the host):
#
#   podman build -t localhost/kuma-test-compositor \
#     -f containers/kuma-test-compositor.containerfile containers/
#   podman run --rm -v "$PWD":/work:Z localhost/kuma-test-compositor \
#     /work/scripts/displayless-smoke.sh
set -euo pipefail

SHELL_BIN=/work/target/release/kuma-shell

fail() { echo "FAIL: $*"; exit 1; }
pass() { echo "PASS: $*"; }

[ -x "$SHELL_BIN" ] || fail "no shell binary at $SHELL_BIN (run ./scripts/build.sh build --release on the host first)"

export XDG_RUNTIME_DIR=/tmp/xdg-run
export WLR_BACKENDS=headless
export WLR_LIBINPUT_NO_DEVICES=1
export WLR_RENDERER=pixman
# the shell logs errors only by default; the recreate lines are info
export RUST_LOG=info
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

# a private session bus: the shell must own its names there, and the
# host session's bus is nobody else's business
dbus-daemon --session --fork --print-address --address=unix:path="$XDG_RUNTIME_DIR/bus" >/tmp/bus-address
export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"

# sway ships with a cap_sys_nice file capability, and podman refuses to
# exec capability binaries (EPERM); a copy without the xattr runs, and a
# headless sway has no use for realtime priority anyway
cp /usr/bin/sway /tmp/sway
/tmp/sway -c /dev/null >/tmp/sway.log 2>&1 &
# the wayland socket appears when sway is listening: poll, don't guess
for _ in $(seq 10); do
	[ -n "$(ls "$XDG_RUNTIME_DIR" 2>/dev/null | grep '^wayland-')" ] && break
	sleep 1
done
export WAYLAND_DISPLAY="$(ls "$XDG_RUNTIME_DIR" | grep '^wayland-' | head -1)"
[ -n "$WAYLAND_DISPLAY" ] || { cat /tmp/sway.log; fail "sway exposed no wayland socket"; }
# swaymsg does not find the IPC socket on its own in here; hand it over
export SWAYSOCK="$(ls "$XDG_RUNTIME_DIR"/sway-ipc.* 2>/dev/null | head -1)"
[ -S "$SWAYSOCK" ] || { cat /tmp/sway.log; fail "no sway ipc socket"; }

# sway's headless output reports enabled: null (not true), so filter on
# nothing: every output get_outputs lists gets unplugged by name
listed_outputs() { swaymsg -t get_outputs 2>/dev/null | jq '[.[]] | length'; }
unplug_all_outputs() {
	for name in $(swaymsg -t get_outputs | jq -r '.[].name'); do
		swaymsg output "$name" unplug >/dev/null
	done
}
# poll the shell's log for a line after a marker: the wayland roundtrip
# that carries a hotplug has compositor-dependent lag, so assert the
# transition happens, not that it beats a fixed sleep
await_log() {
	local marker=$1 pattern=$2 timeout=$3
	for _ in $(seq "$timeout"); do
		sed -n "/--- $marker/,\$p" /tmp/kuma-shell.log | grep -q "$pattern" && return 0
		sleep 1
	done
	return 1
}

# sway's headless backend auto-creates one output; unplug it so the
# Shell starts with no output at all. The global's retirement is
# asynchronous on sway's side, so wait until a fresh client really sees
# none before bringing the Shell up.
unplug_all_outputs
for _ in $(seq 10); do
	[ "$(wayland-info 2>/dev/null | grep -c wl_output)" = "0" ] && break
	sleep 1
done
[ "$(wayland-info 2>/dev/null | grep -c wl_output)" = "0" ] \
	|| { cat /tmp/sway.log; fail "a wl_output global survived the unplug"; }
[ "$(listed_outputs)" = "0" ] || { cat /tmp/sway.log; fail "could not reach zero outputs; the smoke means nothing"; }

"$SHELL_BIN" >>/tmp/kuma-shell.log 2>&1 &
shell_pid=$!

# the deliverable: sixty seconds without a working output, alive the
# whole time, bus name owned. The systemd restart counter would read 0:
# nothing exited. The duration is the contract, so it is not shortened;
# a shell that dies at second 3 fails at second 3, not at second 60.
for _ in $(seq 65); do
	kill -0 "$shell_pid" 2>/dev/null || { cat /tmp/kuma-shell.log; fail "shell exited while displayless"; }
	sleep 1
done
pass "alive after 65s displayless"

busctl --user status org.freedesktop.Notifications >/dev/null 2>&1 \
	|| { tail -20 /tmp/kuma-shell.log; fail "org.freedesktop.Notifications not held"; }
pass "owns org.freedesktop.Notifications"

# a missing config.toml must keep failing soft: a fresh machine has none
grep -q "using defaults" /tmp/kuma-shell.log || fail "missing config did not fail soft"
pass "missing config.toml failed soft"

# hotplug: an output appears; the watch has a tick (2s) to open the
# wallpaper and the bar on it, from nothing
echo "--- hotplug 1" >>/tmp/kuma-shell.log
swaymsg create_output >/dev/null
kill -0 "$shell_pid" 2>/dev/null || { tail -20 /tmp/kuma-shell.log; fail "shell exited when the output appeared"; }
await_log "hotplug 1" "surfaces: displays now 1" 20 || fail "the watch did not see the hotplug"
await_log "hotplug 1" "surfaces: wallpaper opened" 20 \
	|| { tail -20 /tmp/kuma-shell.log; fail "wallpaper did not open on hotplug"; }
await_log "hotplug 1" "surfaces: bar opened" 20 || fail "bar did not open on hotplug"
pass "surfaces opened when the output appeared"

# and back to zero: the output is unplugged, its global goes away
# (GlobalRemove retires it), and the Shell idles again. The opened count
# must stay at one: a watch that kept recreating against the dead output
# would spin (that is what the GlobalRemove patch prevents).
opens_before_unplug="$(grep -c 'surfaces: wallpaper opened' /tmp/kuma-shell.log)"
echo "--- unplug" >>/tmp/kuma-shell.log
unplug_all_outputs
kill -0 "$shell_pid" 2>/dev/null || { tail -20 /tmp/kuma-shell.log; fail "shell exited when the output went away"; }
await_log "unplug" "surfaces: displays now 0" 20 || fail "displays did not retire on unplug"
[ "$(grep -c 'surfaces: wallpaper opened' /tmp/kuma-shell.log)" = "$opens_before_unplug" ] \
	|| fail "watch spun against the unplugged output"
pass "idled through output loss"

# the cycle repeats: a second hotplug. sway orphans layer surfaces on
# unplug instead of closing them (no closed event), so the Shell rightly
# still holds its wallpaper and bar and nothing reopens here; what this
# phase asserts is that the watch keeps seeing outputs across cycles and
# the Shell keeps its name through all of it. niri does close the
# surfaces, and the reopen-from-zero is exactly what hotplug 1 exercised.
echo "--- hotplug 2" >>/tmp/kuma-shell.log
swaymsg create_output >/dev/null
await_log "hotplug 2" "surfaces: displays now 1" 20 \
	|| { tail -20 /tmp/kuma-shell.log; fail "the watch did not see the second hotplug"; }
kill -0 "$shell_pid" 2>/dev/null || { tail -20 /tmp/kuma-shell.log; fail "shell exited on the second hotplug"; }
pass "watch saw the second hotplug"

busctl --user status org.freedesktop.Notifications >/dev/null 2>&1 \
	|| fail "bus name lost across the hotplug cycle"
pass "still owns org.freedesktop.Notifications"

# the session mirror: the sway adapter detected at startup, feeding the
# neutral session state the widgets read. The startup detection and the
# first snapshot land in the log's first seconds, hence no marker.
grep -q "session: sway compositor detected" /tmp/kuma-shell.log \
	|| fail "the sway adapter was not detected at startup"
pass "sway adapter detected"

echo "--- session" >>/tmp/kuma-shell.log
swaymsg workspace 2 >/dev/null
# sway destroys the workspace left behind and mints a fresh node, so the
# count need not move; what must hold is parity: sway says workspace
# number 2 holds focus, and the mirror reports that workspace's exact
# node id
[ "$(swaymsg -t get_workspaces | jq -r '[.[] | select(.focused)][0].num')" = "2" ] \
	|| fail "sway did not focus workspace 2"
focused_id="$(swaymsg -t get_workspaces | jq -r '[.[] | select(.focused)][0].id')"
await_log "session" "focused ${focused_id})" 20 \
	|| { tail -20 /tmp/kuma-shell.log; fail "the mirror did not see the workspace switch"; }
# the command path rides the same seam: the msg CLI's workspace verb
# asks sway to focus, and the mirror reports the switch back
echo "--- msg" >>/tmp/kuma-shell.log
"$SHELL_BIN" msg workspace 3 >/dev/null 2>&1 \
	|| fail "msg workspace failed over the sway adapter"
[ "$(swaymsg -t get_workspaces | jq -r '[.[] | select(.focused)][0].num')" = "3" ] \
	|| fail "sway says workspace 3 is not the focused one"
focused_id="$(swaymsg -t get_workspaces | jq -r '[.[] | select(.focused)][0].id')"
await_log "msg" "focused ${focused_id})" 20 \
	|| { tail -20 /tmp/kuma-shell.log; fail "msg workspace did not reach the mirror"; }
pass "workspace mirror and command path work on sway"

# a real toplevel: open one, watch the mirror, close it, watch again.
# foot is spawned directly (swaymsg exec's children are unreliable in
# here) and killed by criteria (kill hits whatever holds focus)
echo "--- window" >>/tmp/kuma-shell.log
foot -e sh -c 'sleep 60' >/dev/null 2>&1 &
await_log "window" "windows 1 (focused " 20 \
	|| { tail -20 /tmp/kuma-shell.log; fail "the mirror did not see the window open"; }
swaymsg '[app_id=foot] kill' >/dev/null 2>&1
await_log "window" "windows 0 (focused none)" 20 \
	|| { tail -20 /tmp/kuma-shell.log; fail "the mirror did not see the window close"; }
pass "window mirror tracks open and close on sway"

echo "displayless smoke: all green"
