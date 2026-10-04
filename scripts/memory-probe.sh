#!/usr/bin/env bash
# The memory probe: run the Shell under scripted load and watch its
# footprint move. Growth that keeps climbing after a phase ends is a
# leak; growth that plateaus is allocator retention or a cache. The
# point is the table, not a pass/fail: read the deltas per phase.
#
#   podman run --rm -v "$PWD":/work:Z localhost/kuma-test-compositor \
#     /work/scripts/memory-probe.sh [more-seconds-idle]
#
# Loads driven, in order, each isolated in time so its RSS delta reads
# clean:
#   idle        settle after startup
#   notify      60 notifications over the session bus
#   panels      20 settings-panel open/close cycles over the msg CLI
#   hotplug     10 create_output/unplug cycles
#   settle      idle again; what stayed grown here is what leaked
set -euo pipefail

SHELL_BIN="${SHELL_BIN:-/work/target/release/kuma-shell}"
IDLE_EXTRA="${1:-0}"
LOAD_SCALE="${LOAD_SCALE:-1}"

export XDG_RUNTIME_DIR=/tmp/xdg-run
export WLR_BACKENDS=headless
export WLR_LIBINPUT_NO_DEVICES=1
export WLR_RENDERER=pixman
export RUST_LOG=info
# the shipped configuration runs with this cap (the shell's override
# sets it): glibc's arena-per-thread spread otherwise pads the probe's
# numbers by hundreds of MB and drowns the phase deltas
export MALLOC_ARENA_MAX=2
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

dbus-daemon --session --fork --print-address --address=unix:path="$XDG_RUNTIME_DIR/bus" >/tmp/bus-address
export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"

cp /usr/bin/sway /tmp/sway
/tmp/sway -c /dev/null >/tmp/sway.log 2>&1 &
sleep 2
export WAYLAND_DISPLAY="$(ls "$XDG_RUNTIME_DIR" | grep '^wayland-' | head -1)"
export SWAYSOCK="$(ls "$XDG_RUNTIME_DIR"/sway-ipc.* 2>/dev/null | head -1)"
[ -S "$SWAYSOCK" ] || { cat /tmp/sway.log; echo "FAIL: no sway ipc socket"; exit 1; }

swaymsg create_output >/dev/null

"$SHELL_BIN" >>/tmp/kuma-shell.log 2>&1 &
shell_pid=$!
sleep 5
kill -0 "$shell_pid" 2>/dev/null || { cat /tmp/kuma-shell.log; echo "FAIL: shell died at startup"; exit 1; }

: >/tmp/mem.tsv
echo -e "phase\tt\trss_kb\tswap_kb\tthreads\tfds" >/tmp/mem.tsv

phase=idle
set_phase() { phase=$1; printf 'phase\t%s\n' "$1" >>/tmp/mem.tsv; }

sample() {
	local rss swap threads fds
	read -r _ rss _ < <(awk '/VmRSS/{print $1, $2}' /proc/"$shell_pid"/status 2>/dev/null)
	read -r _ swap _ < <(awk '/VmSwap/{print $1, $2}' /proc/"$shell_pid"/status 2>/dev/null)
	threads="$(awk '/Threads/{print $2}' /proc/"$shell_pid"/status 2>/dev/null)"
	fds="$(ls /proc/"$shell_pid"/fd 2>/dev/null | wc -l)"
	[ -n "$rss" ] || return 0
	printf '%s\t% d\t% d\t% d\t%s\t%s\n' "$phase" "$SECONDS" "$rss" "${swap:-0}" "$threads" "$fds" >>/tmp/mem.tsv
}

notify_once() {
	busctl --user call org.freedesktop.Notifications /org/freedesktop/Notifications \
		org.freedesktop.Notifications Notify susssasa{sv}i \
		"probe" 0 "" "load $1" "body of load notification $1" 0 0 3000 >/dev/null 2>&1
}

sample
set_phase startup
for _ in $(seq 15); do sample; sleep 2; done

set_phase notify
for i in $(seq $((60 * LOAD_SCALE))); do notify_once "$i"; sample; sleep 0.5; done
sleep 8

set_phase panels
for i in $(seq $((20 * LOAD_SCALE))); do
	"$SHELL_BIN" settings >/dev/null 2>&1 || true
	sample
	sleep 0.7
	"$SHELL_BIN" settings >/dev/null 2>&1 || true
	sample
	sleep 0.7
done
sleep 8

set_phase launcher
# the launcher decodes every installed app's icon for the grid: the
# heaviest single open in the shell. Open, let icons land, close; a
# spike that never comes back is the icon cache pinning memory.
for i in $(seq 3); do
	"$SHELL_BIN" launcher-toggle >/dev/null 2>&1 || true
	sample
	sleep 6
	sample
	"$SHELL_BIN" launcher-toggle >/dev/null 2>&1 || true
	sample
	sleep 3
done
sleep 8

set_phase hotplug
for i in $(seq $((10 * LOAD_SCALE))); do
	swaymsg create_output >/dev/null
	sample
	sleep 1
	for name in $(swaymsg -t get_outputs | jq -r '.[].name'); do
		swaymsg output "$name" unplug >/dev/null 2>&1 || true
	done
	sample
	sleep 1
done
sleep 8

set_phase settle
for _ in $(seq 15); do sample; sleep 2; done
[ "$IDLE_EXTRA" -gt 0 ] 2>/dev/null && {
	for _ in $(seq "$IDLE_EXTRA"); do sample; sleep 2; done
}

kill "$shell_pid" 2>/dev/null || true

# the readout: first and last sample of each phase
awk -F'\t' '
$1 == "phase" { next }
!($1 in first) { first[$1] = $3; fswap[$1] = $4; ffds[$1] = $6 }
{ last[$1] = $3; lswap[$1] = $4; lfds[$1] = $6 }
END {
	printf "%-10s %10s %10s %10s %8s %8s\n", "phase", "rss_first", "rss_last", "rss_delta", "fds_f", "fds_l"
	for (p in first)
		printf "%-10s %10d %10d %10d %8s %8s\n", p, first[p], last[p], last[p]-first[p], ffds[p], lfds[p]
}' /tmp/mem.tsv | sort -k2 -n
