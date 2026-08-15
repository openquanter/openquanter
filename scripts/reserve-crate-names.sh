#!/usr/bin/env bash
# Reserve the `oq-*` crate names on crates.io by publishing placeholder
# 0.0.1 packages that point back at this repository.
#
# The crates in this workspace are published as they are implemented; the
# placeholders exist so the names cannot be taken by someone else in the
# meantime. Each placeholder states plainly that it is a reservation, per
# crates.io naming policy.
#
# Usage:
#   scripts/reserve-crate-names.sh            # publish everything missing
#   DRY_RUN=1 scripts/reserve-crate-names.sh  # package and verify only
#   scripts/reserve-crate-names.sh oq-margin  # a single name
#
# Requires `cargo login` (or CARGO_REGISTRY_TOKEN) unless DRY_RUN=1.
#
# crates.io rate-limits new crates (a burst, then roughly one every ten
# minutes). The script publishes sequentially, waits when it is throttled,
# and is safe to re-run: names that already exist are skipped.

set -euo pipefail

VERSION="0.0.1"
REPO="https://github.com/openquanter/openquanter"
DRY_RUN="${DRY_RUN:-0}"
RETRY_WAIT="${RETRY_WAIT:-660}"
MAX_RETRIES="${MAX_RETRIES:-6}"
CARGO="${CARGO:-cargo}"

# name|role sentence used in the crate description and stub docs
CRATES=(
  "openquanter|Deterministic, AI-native quantitative trading framework (umbrella crate)"
  "oq-types|Core domain types, fixed-point arithmetic and order state machines"
  "oq-journal|Memory-mapped event journal with snapshots and deterministic replay"
  "oq-core|Sequencer and deterministic event kernel"
  "oq-engine|Matching engine with a selectable fidelity ladder"
  "oq-margin|Tiered maintenance margin, liquidation paths and funding modeling"
  "oq-backtest|Backtest scheduling, accounting and fidelity reporting"
  "oq-parity|Trade-by-trade run diffing and difference attribution"
  "oq-data|Dual-timestamp columnar market data with bitemporal reference data"
  "oq-l2feed|Market data capture: incremental depth, best bid/offer, trades, mark price"
  "oq-strategy|Strategy traits and reusable indicator components"
  "oq-py|Python bindings for the OpenQuanter trading framework"
  "oq-stats|Backtest overfitting statistics: deflated Sharpe ratio and PBO"
  "oq-cli|Command line interface for backtesting, sweeps, replay and live trading"
  "oq-sim|Deterministic whole-system fault simulation and scenario corpus"
  "oq-risk|Unbypassable pre-trade risk gate, limits, kill switch and reconciliation"
  "oq-gateway|Venue adapters and connector conformance suite"
  "oq-live|Live process assembly, snapshot recovery and graceful restart"
  "oq-features|Point-in-time feature layer with online/offline consistency metrics"
  "oq-infer|In-process model inference for ONNX and compiled decision trees"
  "oq-env|Vectorized reinforcement learning environments"
  "oq-lab|Experimental sandbox for LLM-driven research tooling"
)

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

crate_exists() {
  local name="$1" code
  code="$(curl -sS -o /dev/null -w '%{http_code}' \
    -H 'User-Agent: openquanter-name-reservation (https://github.com/openquanter/openquanter)' \
    "https://crates.io/api/v1/crates/$name" || echo 000)"
  [ "$code" = "200" ]
}

generate_stub() {
  local name="$1" role="$2" dir="$work_dir/$name"
  mkdir -p "$dir/src"

  cat > "$dir/Cargo.toml" <<TOML
[package]
name = "$name"
version = "$VERSION"
edition = "2021"
license = "Apache-2.0"
description = "$role. Placeholder release reserving the name for the OpenQuanter project; the implementation lives in the repository and will be published as it lands."
repository = "$REPO"
homepage = "$REPO"
documentation = "$REPO"
readme = "README.md"
keywords = ["trading", "quant", "backtesting", "crypto"]
categories = ["finance"]

[dependencies]
TOML

  cat > "$dir/README.md" <<MD
# $name

**Placeholder release — no implementation yet.**

$role.

This crate is part of [OpenQuanter]($REPO), a deterministic, AI-native
quantitative trading framework written in Rust. Version $VERSION reserves the
name; the working implementation is developed in the repository and published
here once it is usable.

- Repository: $REPO
- Requirements, roadmap and implementation plan: $REPO/tree/main/docs
- License: Apache-2.0
MD

  cat > "$dir/src/lib.rs" <<RS
//! **Placeholder — not yet implemented.**
//!
//! $role.
//!
//! This crate is part of [OpenQuanter]($REPO), a deterministic, AI-native
//! quantitative trading framework. Version $VERSION exists only to reserve
//! the name; the implementation is developed in the repository and will be
//! published here once it is usable.
//!
//! See the [roadmap]($REPO/blob/main/docs/ROADMAP.md) for the milestone this
//! crate belongs to.
RS

  printf '%s\n' "$dir"
}

publish_one() {
  local name="$1" role="$2" dir attempt=0
  dir="$(generate_stub "$name" "$role")"

  if [ "$DRY_RUN" = "1" ]; then
    echo "--- dry run: $name"
    (cd "$dir" && "$CARGO" publish --dry-run --allow-dirty --quiet)
    echo "ok  $name (dry run)"
    return 0
  fi

  while :; do
    if (cd "$dir" && "$CARGO" publish --allow-dirty --no-verify 2>"$work_dir/err.log"); then
      echo "published  $name $VERSION"
      return 0
    fi

    if grep -qiE 'rate limit|429|too many requests' "$work_dir/err.log"; then
      attempt=$((attempt + 1))
      if [ "$attempt" -gt "$MAX_RETRIES" ]; then
        echo "giving up on $name after $MAX_RETRIES rate-limited attempts" >&2
        sed 's/^/    /' "$work_dir/err.log" >&2
        return 1
      fi
      echo "rate limited on $name; waiting ${RETRY_WAIT}s (attempt $attempt/$MAX_RETRIES)"
      sleep "$RETRY_WAIT"
      continue
    fi

    echo "FAILED  $name" >&2
    sed 's/^/    /' "$work_dir/err.log" >&2
    return 1
  done
}

selected=("$@")
failed=0
skipped=0
done_count=0

for entry in "${CRATES[@]}"; do
  name="${entry%%|*}"
  role="${entry#*|}"

  if [ "${#selected[@]}" -gt 0 ]; then
    match=0
    for want in "${selected[@]}"; do
      [ "$want" = "$name" ] && match=1
    done
    [ "$match" -eq 1 ] || continue
  fi

  if [ "$DRY_RUN" != "1" ] && crate_exists "$name"; then
    echo "skip       $name (already on crates.io)"
    skipped=$((skipped + 1))
    continue
  fi

  if publish_one "$name" "$role"; then
    done_count=$((done_count + 1))
  else
    failed=$((failed + 1))
  fi
done

echo
echo "done: $done_count published, $skipped skipped, $failed failed"
[ "$failed" -eq 0 ]
