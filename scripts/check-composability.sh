#!/usr/bin/env bash
# Assert that the engine crates stay dependency-free and independently
# usable.
#
# Usage:
#   scripts/check-composability.sh
#
# Two properties, both claimed on the front page, both checked here
# rather than asserted:
#
#   1. Every crate builds on its own. A crate that only compiles as part
#      of the workspace is not a component anyone can adopt piecemeal.
#   2. Third-party dependencies stay within a declared budget. Eleven of
#      twelve crates are at zero: the entire engine — types, journal,
#      core, matching, margin, backtest, data, parity, statistics — is
#      plain std Rust. That is worth defending, because it erodes one
#      convenient dependency at a time and is very hard to walk back.
#
# Raising a budget is a deliberate act. Edit the table below in the same
# commit that adds the dependency, and say in the message what it buys.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

# crate:max third-party dependencies (transitive)
BUDGETS=(
  "oq-types:0"
  "oq-hash:0"
  "oq-journal:0"
  "oq-core:0"
  "oq-engine:0"
  "oq-margin:0"
  "oq-backtest:0"
  "oq-data:0"
  "oq-parity:0"
  # The pre-trade gate. Zero because it is the layer everything else is
  # checked against: a risk gate that cannot be built from the workspace
  # alone is a risk gate whose availability depends on a registry.
  "oq-risk:0"
  "oq-stats:0"
  # The strategy contract. Zero, and it has to stay zero for a reason
  # the other zeros do not share: this is the crate a user writes
  # against. Anything that lands here is inherited by every strategy
  # anyone ever writes, in backtest and in production alike.
  "oq-strategy:0"
  # The bridge from captured archives to the tick format. It depends on
  # oq-l2feed and inherits its transitive tree, which is why it is a
  # separate crate: oq-data must stay at zero, and it would not if the
  # conversion lived there.
  "oq-ingest:60"
  # Zero at runtime. The budget is measured over `--edges normal`, so
  # the `criterion` dev-dependency behind `cargo bench` is out of scope
  # by design: nothing that depends on a crate inherits its dev-deps,
  # and the property being defended is what a *consumer* pulls in.
  "oq-examples:0"
  # Speaks WebSocket and HTTP to a venue, so it carries a TLS stack.
  # Isolated here on purpose: the engine must not inherit it.
  "oq-l2feed:60"
  # Reads an account over HTTPS, so it carries a TLS stack for the same
  # reason oq-l2feed does — and the same ureq, so this adds no tree the
  # workspace was not already carrying. Signing and JSON reading are
  # written out by hand rather than pulled in: this is the crate that
  # holds the API secret, and every dependency here is one more thing
  # trusted with it.
  # Reads an account over HTTPS and hears about fills over a websocket,
  # so it carries a TLS stack and a websocket client. Raised from 40
  # when the user data stream landed: the venue pushes fills and there
  # is no way to hear them over HTTPS. Isolated here for the same
  # reason as the capture crate — the engine must not inherit it.
  "oq-gateway:60"
)

third_party_count() {
  # The `|| true` matters: with no third-party dependencies the final
  # grep matches nothing and exits non-zero, which under `set -e` would
  # abort the script on exactly the crates that pass.
  {
    cargo tree -p "$1" --edges normal 2>/dev/null \
      | grep -oE "[a-z0-9_-]+ v[0-9]" \
      | sed 's/ v[0-9]//' \
      | sort -u \
      | grep -v '^oq-' \
      || true
  } | wc -l | tr -d ' '
}

failures=0

echo "== dependency budgets =="
for entry in "${BUDGETS[@]}"; do
  crate="${entry%%:*}"
  budget="${entry##*:}"

  if [ ! -d "crates/$crate" ]; then
    echo "  $crate: listed in the budget table but not in crates/"
    failures=$((failures + 1))
    continue
  fi

  count=$(third_party_count "$crate")
  if [ "$count" -le "$budget" ]; then
    printf '  %-14s %2s  (budget %s)\n' "$crate" "$count" "$budget"
  else
    printf '  %-14s %2s  OVER BUDGET (%s)\n' "$crate" "$count" "$budget"
    failures=$((failures + 1))
  fi
done

# Any crate missing from the table is also a failure: a new crate must
# declare its budget, so the decision is made rather than defaulted.
for dir in crates/*/; do
  crate=$(basename "$dir")
  listed=0
  for entry in "${BUDGETS[@]}"; do
    [ "${entry%%:*}" = "$crate" ] && listed=1
  done
  if [ "$listed" -eq 0 ]; then
    echo "  $crate: no declared dependency budget — add one to this script"
    failures=$((failures + 1))
  fi
done

echo
echo "== standalone builds =="
for dir in crates/*/; do
  crate=$(basename "$dir")
  if cargo build -p "$crate" --quiet 2>/dev/null; then
    printf '  %-14s ok\n' "$crate"
  else
    printf '  %-14s FAILED to build on its own\n' "$crate"
    failures=$((failures + 1))
  fi
done

echo
if [ "$failures" -gt 0 ]; then
  echo "composability: $failures problem(s)"
  echo
  echo "If a new dependency is genuinely needed, raise the budget in this"
  echo "script in the same commit and say what it buys. The check exists so"
  echo "that adding one is a decision, not an accident."
  exit 1
fi

echo "composability: engine crates carry no third-party dependencies"
