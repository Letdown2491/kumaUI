#!/usr/bin/env bash
# kuma-term throughput probe, the kitty way: cat a large mixed-text file
# through the terminal and time how long the render takes. Run this INSIDE
# the terminal under test (kuma-term, kitty, alacritty, ...), bigger is
# better. kitty's published figure is around 134 MB/s on similar hardware.
#
#   ./scripts/kuma-term-bench.sh
set -euo pipefail

file="${TMPDIR:-/tmp}/kuma-term-bench.txt"
size_mb=100
want_bytes=$((size_mb * 1024 * 1024))

if [ ! -f "$file" ] || [ "$(stat -c%s "$file")" -ne "$want_bytes" ]; then
  echo "generating ${size_mb}MB of mixed text (one-time, base64 line noise)..."
  head -c "$want_bytes" /dev/urandom | base64 | head -c "$want_bytes" > "$file"
fi

printf 'press enter to start; the timer stops when the text finishes drawing\n'
read -r
t0=$(date +%s.%N)
cat "$file"
t1=$(date +%s.%N)
awk -v s="$t0" -v e="$t1" -v mb="$size_mb" \
  'BEGIN { printf "%d MB in %.2fs = %.1f MB/s\n", mb, e - s, mb / (e - s) }'
