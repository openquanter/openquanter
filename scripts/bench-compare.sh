#!/usr/bin/env bash
# Measure a performance change against the code before it.
#
# Usage:
#   scripts/bench-compare.sh save    <name> [filter]   # on the base commit
#   scripts/bench-compare.sh compare <name> [filter]   # on the change
#
# CI's throughput check is a floor, not a regression gate: shared runners
# vary by several times, so a number from one is not comparable with a
# number from another. Two runs on one machine are. Save a baseline on
# the commit before the change, check out the change, compare, and paste
# the comparison into the pull request.
#
# Both benchmark files run: `engine` (journal, each matching tier, the
# kernel, fixed-point arithmetic — one layer at a time) and `backtest`
# (the whole loop). `filter` narrows to benchmarks whose name contains
# it, e.g. `l1_on_tick`.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

usage() {
    sed -n '2,6p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

[ $# -ge 2 ] || usage
mode=$1
name=$2
filter=${3:-}

case "$mode" in
    save) flag=--save-baseline ;;
    compare) flag=--baseline ;;
    *) usage ;;
esac

if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "note: the working tree has uncommitted changes; they are part of what is measured" >&2
fi
echo "measuring $(git rev-parse --short HEAD) ($mode $name)" >&2

cargo bench -p oq-examples --bench engine --bench backtest -- "$flag" "$name" ${filter:+"$filter"}
