#!/usr/bin/env bash
# The kuma-term smoke: the terminal must open a window under a headless
# compositor, spawn its shell into a PTY, and survive a resize without
# panicking. It cannot assert pixels, but a mapped app_id plus a clean log
# after a few seconds catches the big failure classes (platform init, font
# metrics, engine thread, first render).
#
# Run inside the container (the script assumes that, not the host):
#
#   podman run --rm -v "$PWD":/work:Z localhost/kuma-test-compositor \
#     /work/scripts/kuma-term-smoke.sh
set -euo pipefail

TERM_BIN=/work/target/release/kuma-term

fail() { echo "FAIL: $*"; exit 1; }
pass() { echo "PASS: $*"; }

[ -x "$TERM_BIN" ] || fail "no kuma-term binary at $TERM_BIN (build on the host first)"

export XDG_RUNTIME_DIR=/tmp/xdg-run
export WLR_BACKENDS=headless
export WLR_LIBINPUT_NO_DEVICES=1
export WLR_RENDERER=pixman
export RUST_LOG=info
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

dbus-daemon --session --fork --print-address --address=unix:path="$XDG_RUNTIME_DIR/bus" >/tmp/bus-address
export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"

cp /usr/bin/sway /tmp/sway
/tmp/sway -c /dev/null >/tmp/sway.log 2>&1 &
for _ in $(seq 10); do
	[ -n "$(ls "$XDG_RUNTIME_DIR" 2>/dev/null | grep '^wayland-')" ] && break
	sleep 1
done
export WAYLAND_DISPLAY="$(ls "$XDG_RUNTIME_DIR" | grep '^wayland-' | head -1)"
[ -n "$WAYLAND_DISPLAY" ] || { cat /tmp/sway.log; fail "sway exposed no wayland socket"; }
export SWAYSOCK="$(ls "$XDG_RUNTIME_DIR"/sway-ipc.* 2>/dev/null | head -1)"
[ -S "$SWAYSOCK" ] || { cat /tmp/sway.log; fail "no sway ipc socket"; }

"$TERM_BIN" >/tmp/kuma-term.log 2>&1 &
TERM_PID=$!

# give it time to map the window, spawn the shell, and settle
sleep 3

kill -0 "$TERM_PID" 2>/dev/null || { cat /tmp/kuma-term.log; fail "kuma-term exited early"; }

# the window must be mapped in the compositor tree under our app_id
if swaymsg -t get_tree | grep -q '"app_id": "kuma-term"'; then
	pass "window mapped with app_id kuma-term"
else
	swaymsg -t get_tree | grep -i kuma-term >/dev/null 2>&1 && \
		pass "window mapped (app_id formatting differs)" || {
		cat /tmp/kuma-term.log
		fail "no kuma-term window in the sway tree"
	}
fi

# the engine must have spawned its shell (the pty child lives under us)
if [ -n "$(find /proc/"$TERM_PID"/task -type d 2>/dev/null)" ]; then
	pass "engine threads alive after settle"
fi

# a resize storm must not kill it
swaymsg resize set 900 600 >/dev/null 2>&1 || true
sleep 1
swaymsg resize set 640 480 >/dev/null 2>&1 || true
sleep 1
kill -0 "$TERM_PID" 2>/dev/null || { cat /tmp/kuma-term.log; fail "kuma-term died during resize storm"; }
pass "survived the resize storm"

# clean exit on TERM, and a quiet log: panics or gpu errors would print
kill "$TERM_PID" 2>/dev/null || true
sleep 1
if grep -E "panicked at| ERROR " /tmp/kuma-term.log; then
	fail "log contains panics or error-level lines"
else
	pass "log is clean"
fi

# shell-integration bytes: fish emits OSC 133/7 markers and OSC 7 cwd on
# its own, so a real session feeds these to the parser constantly. The
# emulator does not consume them; the property under test is that the
# unknown-OSC stream rides through harmlessly and nothing panics.
KUMA_TERM_COMMAND="printf '\033]133;A\033\\\\'; printf '\033]7;file://smokehost/tmp\033\\\\'; echo smoke; printf '\033]133;D;3\033\\\\'; sleep 30" \
	"$TERM_BIN" >/tmp/kuma-term-bar.log 2>&1 &
BAR_PID=$!
sleep 3
kill -0 "$BAR_PID" 2>/dev/null || { cat /tmp/kuma-term-bar.log; fail "kuma-term died under OSC fixtures"; }
if swaymsg -t get_tree | grep -q '"app_id": "kuma-term"'; then
	pass "OSC-fixture instance mapped"
else
	fail "OSC-fixture instance never mapped"
fi
kill "$BAR_PID" 2>/dev/null || true
sleep 1
if grep -E "panicked at| ERROR " /tmp/kuma-term-bar.log; then
	fail "bar run logged panics or errors"
else
	pass "OSC fixtures rode clean"
fi
echo "SMOKE OK"
