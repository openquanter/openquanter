#!/usr/bin/env bash
# Every publishable crate in the workspace has its name reserved.
#
# The reservation list in `reserve-crate-names.sh` is maintained by hand,
# and a hand-maintained list drifts silently: it is only consulted when
# someone publishes, and by then the name may belong to someone else.
# When this check was written the list had drifted three ways at once —
# `oq-hash` and `oq-ingest` were reserved on crates.io but absent from
# it, and `oq-book` was in neither.
#
# This runs offline on purpose. It compares the workspace against the
# list, not against crates.io, so CI does not depend on a third party
# being reachable and does not spend a request per crate per run.
# Reconciling the list with what is actually reserved is what the
# publish script itself does, by skipping names that already exist.
#
# A crate marked `publish = false` needs no name and is not required to
# have one.

set -euo pipefail

cd "$(dirname "$0")/.."

script="scripts/reserve-crate-names.sh"

listed="$(grep -oE '^  "[a-z0-9-]+\|' "$script" | tr -d ' "|' | sort)"

publishable=""
for dir in crates/*/; do
  name="$(basename "$dir")"
  # `publish = false` says this crate never leaves the workspace.
  if grep -qE '^publish *= *false' "$dir/Cargo.toml"; then
    continue
  fi
  publishable="$publishable$name"$'\n'
done
publishable="$(printf '%s' "$publishable" | sort)"

missing="$(comm -23 <(printf '%s\n' "$publishable") <(printf '%s\n' "$listed"))"

if [ -n "$missing" ]; then
  echo "crate names not in $script:" >&2
  printf '  %s\n' $missing >&2
  echo >&2
  echo "Add them to the CRATES list, then run:" >&2
  printf '  %s %s\n' "$script" "$(printf '%s ' $missing)" >&2
  exit 1
fi

count="$(printf '%s\n' "$listed" | grep -c .)"
echo "crate names: $(printf '%s\n' "$publishable" | grep -c .) publishable crate(s), all present in a list of $count"
