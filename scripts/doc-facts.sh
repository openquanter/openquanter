#!/usr/bin/env bash
# Print the facts about this workspace that documentation keeps getting
# wrong, in a form a document can embed and CI can compare.
#
#   scripts/doc-facts.sh            # print the block
#   scripts/doc-facts.sh --check    # fail if any document's copy differs
#   scripts/doc-facts.sh --write    # update every document's copy
#
# Why this exists
# ---------------
# Some sentences in the documentation are assertions about the code:
# which crates exist, what the live path depends on, whether a thing has
# been built. They were written by hand, they went stale, and each time
# the staleness was found by a person reading the code — twice in one
# day, in both directions. Documentation that oversells unbuilt work is
# disqualifying in a project whose claim is that its results can be
# checked; documentation that undersells shipped work is a smaller fault
# and still teaches readers that the honest-sounding parts are not
# maintained either.
#
# Writing them more carefully was not the fix, because care is the thing
# that had already failed. So they are no longer written. They are
# generated from `Cargo.toml`, embedded between markers, and compared in
# CI. A fact that cannot be typed cannot be mistyped.
#
# What belongs here: anything derivable from the tree, that a reader
# would take as current, and that changes without anyone thinking about
# the document. What does not: judgement, intent, and anything the code
# cannot answer — those still have to be written, and still have to be
# maintained by hand.

set -uo pipefail

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

BEGIN="<!-- begin generated: workspace facts (scripts/doc-facts.sh) -->"
END="<!-- end generated -->"

# Documents carrying a copy. Adding one here is all it takes for it to
# be checked from then on.
DOCS=(README.md README.zh-CN.md)

crate_list() {
  # Directory names, not Cargo.toml `name` fields: the directory is what
  # a reader browsing the repository sees.
  for d in crates/*/; do
    [ -f "$d/Cargo.toml" ] && basename "$d"
  done | sort | paste -sd' ' -
}

deps_of() {
  # Workspace siblings only. A third-party dependency is a different
  # question, and `check-composability.sh` already answers it.
  sed -n '/^\[dependencies\]/,/^\[/p' "crates/$1/Cargo.toml" 2>/dev/null |
    grep -oE '^oq-[a-z0-9]+' | sort | paste -sd' ' -
}

block() {
  local crates
  crates="$(crate_list)"
  echo "$BEGIN"
  echo
  echo "\`\`\`text"
  printf 'crates (%d)\n' "$(echo "$crates" | wc -w)"
  # Wrapped, because one long line reads as noise and nobody checks noise.
  echo "$crates" | fold -s -w 66 | sed 's/[[:space:]]*$//; s/^/  /'
  echo
  echo "the live path"
  for c in oq-live oq-backtest; do
    printf '  %s\n' "$c"
    deps_of "$c" | fold -s -w 62 | sed 's/[[:space:]]*$//; s/^/    /'
  done
  echo
  echo "shared by both"
  comm -12 <(deps_of oq-live | tr ' ' '\n' | sort) \
           <(deps_of oq-backtest | tr ' ' '\n' | sort) |
    paste -sd' ' - | fold -s -w 66 | sed 's/[[:space:]]*$//; s/^/  /'
  echo "live only"
  comm -23 <(deps_of oq-live | tr ' ' '\n' | sort) \
           <(deps_of oq-backtest | tr ' ' '\n' | sort) |
    paste -sd' ' - | sed 's/^/  /'
  echo "backtest only"
  comm -13 <(deps_of oq-live | tr ' ' '\n' | sort) \
           <(deps_of oq-backtest | tr ' ' '\n' | sort) |
    paste -sd' ' - | sed 's/^/  /'
  echo "\`\`\`"
  echo
  echo "$END"
}

replace_in() {
  local doc="$1" tmp
  tmp="$(mktemp)"
  awk -v b="$BEGIN" -v e="$END" -v f="$2" '
    $0 == b { print_block = 1; while ((getline line < f) > 0) print line; next }
    $0 == e { print_block = 0; next }
    !print_block { print }
  ' "$doc" > "$tmp"
  mv "$tmp" "$doc"
}

case "${1:-}" in
  --write)
    tmp="$(mktemp)"; block > "$tmp"
    for d in "${DOCS[@]}"; do
      grep -qF "$BEGIN" "$d" || { echo "$d has no marker; add $BEGIN"; exit 1; }
      replace_in "$d" "$tmp"
      echo "updated $d"
    done
    rm -f "$tmp"
    ;;
  --check)
    tmp="$(mktemp)"; block > "$tmp"
    fail=0
    for d in "${DOCS[@]}"; do
      if ! grep -qF "$BEGIN" "$d"; then
        echo "FAIL $d: no generated block. Add the markers, then --write."
        fail=1
        continue
      fi
      have="$(mktemp)"
      awk -v b="$BEGIN" -v e="$END" '$0==b{p=1} p{print} $0==e{p=0}' "$d" > "$have"
      if diff -u "$have" "$tmp" > /dev/null; then
        echo "ok   $d"
      else
        echo "FAIL $d is out of date:"
        diff -u "$have" "$tmp" | sed -n '4,20p' | sed 's/^/     /'
        fail=1
      fi
      rm -f "$have"
    done
    rm -f "$tmp"
    if [ "$fail" -ne 0 ]; then
      echo
      echo "Run scripts/doc-facts.sh --write and commit the result."
      echo "These lines are generated; editing them by hand is what this"
      echo "check exists to stop."
      exit 1
    fi
    ;;
  *)
    block
    ;;
esac
