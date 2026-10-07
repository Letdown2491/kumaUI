#!/usr/bin/env bash
# The kuma-term visual probe: render a fixed pattern (box drawing, block
# elements, braille, color ramps) under the headless compositor and shoot a
# screenshot. Pixels are for eyes, not asserts; the value is comparing the
# grid alignment and palette against what kitty shows for the same bytes.
#
# Run inside the container (the script assumes that, not the host):
#
#   podman run --rm -v "$PWD":/work:Z localhost/kuma-test-compositor \
#     /work/scripts/kuma-term-visual.sh
set -euo pipefail

TERM_BIN=/work/target/release/kuma-term
OUT=/work/.scratch
mkdir -p "$OUT"

fail() { echo "FAIL: $*"; exit 1; }
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

# the pattern: every glyph class a TUI leans on, in deterministic order
cat >/tmp/pattern.sh <<'PATTERN'
printf '\033[1;36mBOX SINGLE\033[0m\n'
printf '┌─────┬─────┐  ┏━┳━┓\n'
printf '│     │     │  ┃ ┃ ┃\n'
printf '├─────┼─────┤  ┣━╋━┫\n'
printf '└─────┴─────┘  ┗━┻━┛\n'
printf '\033[1;36mBOX DOUBLE\033[0m\n'
printf '╔═══╦═══╗   ╔═╗\n'
printf '║   ║   ║   ║ ║\n'
printf '╠═══╬═══╣   ╚═╝\n'
printf '\033[1;36mWEIGHTS\033[0m\n'
printf '───── ━━━━━ ═════ │││ ┃┃┃ ║║║\n'
printf '\033[1;36mTEE FAMILY\033[0m\n'
printf '├┤┬┴┼  ┝┥┮┰╀  ┣┫┳┻╋\n'
printf '\033[1;36mSTUBS\033[0m\n'
printf '╴╵╶╷ ╸╹╺╻ ╼╽╾╿\n'
printf '\033[1;36mBLOCKS\033[0m\n'
printf '▁▂▃▄▅▆▇█ ▉▊▋▌▍▎▏ ▌▐ ▖▗▘▝▚▞\n'
printf '\033[1;36mSHADES (text)\033[0m\n'
printf '░▒▓█ ░▒▓█\n'
sleep 300
PATTERN
chmod +x /tmp/pattern.sh

KUMA_TERM_COMMAND="/bin/sh /tmp/pattern.sh" KUMA_TERM_FONT_PT="${KUMA_TERM_FONT_PT:-11}" "$TERM_BIN" >/tmp/kuma-term.log 2>&1 &
TERM_PID=$!
sleep 3
kill -0 "$TERM_PID" 2>/dev/null || { cat /tmp/kuma-term.log; fail "kuma-term exited early"; }

grim -o HEADLESS "$OUT/kuma-term.png" 2>/dev/null || grim "$OUT/kuma-term.png" || { cat /tmp/kuma-term.log; fail "grim produced nothing"; }
[ -s "$OUT/kuma-term.png" ] || fail "empty screenshot"
echo "shot: $OUT/kuma-term.png ($(wc -c <"$OUT/kuma-term.png") bytes)"

# echo the resolved font + metrics for the record
grep -E "cell metrics|font" /tmp/kuma-term.log | head -4 || true
fc-match monospace

kill "$TERM_PID" 2>/dev/null || true
echo "VISUAL OK"
