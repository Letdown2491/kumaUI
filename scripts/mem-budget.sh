#!/usr/bin/env bash
# Memory-budget guard: asserts the running kuma-shell stays under budget.
#
# The regression this guards against: a debug build installed to
# ~/.local/bin (measured at 293MB PSS vs the release's ~72MB; the profile,
# not the architecture). Run it after swapping in a new binary:
#
#   ./scripts/mem-budget.sh           # assert + print top mappings
#
# Exits nonzero on breach, with the breakdown printed for diagnosis.
set -euo pipefail

BUDGET_KB=150000  # PSS budget: release measured 72MB fresh, 94MB grown; a
                  # debug build measures ~293MB; the budget exists to catch
                  # that, not to fail on single-digit growth

pids=$(pgrep -x kuma-shell || true)
if [[ -z "$pids" ]]; then
    echo "mem-budget: no running kuma-shell; start one first (skipped)"
    exit 0
fi

failed=0
for pid in $pids; do
    pss_kb=$(awk '/^Pss:/ { print $2 }' "/proc/$pid/smaps_rollup")
    rss_kb=$(awk '/^Rss:/ { print $2 }' "/proc/$pid/smaps_rollup")
    if (( pss_kb > BUDGET_KB )); then
        echo "mem-budget: BREACH pid $pid: PSS ${pss_kb}kB > ${BUDGET_KB}kB budget (RSS ${rss_kb}kB)"
        failed=1
    else
        echo "mem-budget: ok pid $pid: PSS ${pss_kb}kB (budget ${BUDGET_KB}kB, RSS ${rss_kb}kB)"
    fi

    # the breakdown, worst first; only interesting when something's wrong
    if (( failed )); then
        echo "  top mappings by PSS:"
        awk '
            /^[0-9a-f]+-[0-9a-f]+/ { path = $6 == "" ? "[anon]" : $6 }
            /^Pss:/ { print $2, path }
        ' "/proc/$pid/smaps" | sort -rn | head -15 | sed 's/^/    /'
    fi
done

exit $failed
