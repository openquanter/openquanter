#!/usr/bin/env bash
# Replace the capture binary with a bounded, visible interruption.
#
# Two modes, because they trade different things:
#
#   --restart  (default)  Stop, swap, start. Every stream is down for
#                         somewhere between a second and the websocket
#                         read timeout. The hole is real but marked --
#                         each restarted window records the silence as a
#                         gap in its manifest.
#
#   --overlap             Start the new binary on a second archive root
#                         before stopping the old one, so both are
#                         recording during the changeover and no message
#                         is missed. Costs double bandwidth and CPU for
#                         the overlap window. The two trees are merged
#                         automatically by oq-merge afterwards.
#
# Overlap does NOT mean two processes on one file. Each writer holds a
# 1 MiB buffer and appends in buffer-sized chunks; two of them on the
# same file interleave at chunk boundaries and split records down the
# middle. Separate roots, merged after, is the only safe shape.
#
# The binary is swapped with rename(2), never with cp. Writing into a
# running executable fails with ETXTBSY, and a cp that fails after
# printing nothing is how an "upgrade" silently leaves the old build in
# place -- which is exactly what happened the first time this was done
# by hand.

set -uo pipefail

BIN="${BIN:-/home/ubuntu/oq-capture}"
NEW="${NEW:-}"
ROOT="${ROOT:-/home/ubuntu/capture}"
OVERLAP_ROOT="${OVERLAP_ROOT:-/home/ubuntu/capture-overlap}"
SUPERVISOR="${SUPERVISOR:-/home/ubuntu/oq/capture-supervisor.sh}"
OVERLAP_SECONDS="${OVERLAP_SECONDS:-120}"
MERGE="${MERGE:-/home/ubuntu/oq-merge}"
MODE=restart

while [ $# -gt 0 ]; do
  case "$1" in
    --new) NEW="${2:?}"; shift 2 ;;
    --restart) MODE=restart; shift ;;
    --overlap) MODE=overlap; shift ;;
    --overlap-seconds) OVERLAP_SECONDS="${2:?}"; shift 2 ;;
    --help|-h) sed -n '2,28p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "upgrade: unknown argument $1" >&2; exit 2 ;;
  esac
done

[ -n "$NEW" ] || { echo "upgrade: --new <path to freshly built binary> is required" >&2; exit 2; }
[ -x "$NEW" ] || { echo "upgrade: $NEW is not executable" >&2; exit 2; }

stamp() { date -u '+%Y-%m-%dT%H:%M:%SZ'; }

# Generations are told apart by their archive root, not by process name:
# after the swap both run the same binary under the same name, and
# killing by name would take the new one down with the old.
running() { pgrep -fc -- "--root $1 --symbol " 2>/dev/null || true; }

if cmp -s "$NEW" "$BIN"; then
  echo "upgrade: $BIN is already this build; nothing to do"
  exit 0
fi

echo "$(stamp) mode=$MODE  running=$(running "$ROOT")"

# rename(2) swaps the directory entry. Processes already executing the
# old inode keep running it untouched; the next exec picks up the new
# one. This is what makes the overlap mode possible at all.
cp "$NEW" "$BIN.incoming" || { echo "upgrade: cannot stage new binary" >&2; exit 1; }
chmod +x "$BIN.incoming"
cp -f "$BIN" "$BIN.previous" 2>/dev/null || true
if ! mv -f "$BIN.incoming" "$BIN"; then
  echo "upgrade: rename failed, old binary left in place" >&2
  exit 1
fi
cmp -s "$BIN" "$NEW" || { echo "upgrade: binary does not match after swap" >&2; exit 1; }
echo "$(stamp) binary swapped (previous kept at $BIN.previous)"

if [ "$MODE" = overlap ]; then
  echo "$(stamp) starting overlap capture under $OVERLAP_ROOT"
  mkdir -p "$OVERLAP_ROOT"
  ROOT="$OVERLAP_ROOT" "$SUPERVISOR" || {
    echo "upgrade: overlap capture failed to start; old capture untouched" >&2
    exit 1
  }
  echo "$(stamp) both generations recording; holding ${OVERLAP_SECONDS}s"
  sleep "$OVERLAP_SECONDS"
fi

echo "$(stamp) stopping the old generation"
pkill -f -- "--root $ROOT --symbol " 2>/dev/null || true

# A stream with nothing to say is blocked in its read until the socket
# timeout, so a graceful stop is bounded by that, not immediate. Waiting
# is the point: the alternative is SIGKILL, which loses the buffer and
# the manifest that this whole path exists to preserve.
waited=0
while [ "$(running "$ROOT")" -gt 0 ] && [ "$waited" -lt 180 ]; do
  sleep 5
  waited=$((waited + 5))
done
echo "$(stamp) old generation gone after ${waited}s (remaining=$(running "$ROOT"))"

echo "$(stamp) starting the new generation"
ROOT="$ROOT" "$SUPERVISOR" | tail -1

if [ "$MODE" = overlap ]; then
  echo
  echo "$(stamp) merging the overlap"
  [ -x "$MERGE" ] || { echo "upgrade: $MERGE not found; overlap tree left at $OVERLAP_ROOT" >&2; exit 1; }

  # The old generation is the primary: within the overlap both trees
  # hold the same messages, and keeping one connection's timeline
  # rather than choosing per message is what stops local_ts from
  # becoming a biased mixture. See oq-merge for the reasoning.
  MERGED="$ROOT.merged.$(date -u +%Y%m%dT%H%M%SZ)"
  if ! "$MERGE" --primary "$ROOT" --secondary "$OVERLAP_ROOT" --out "$MERGED"; then
    echo "upgrade: merge failed; both trees left in place ($ROOT, $OVERLAP_ROOT)" >&2
    exit 1
  fi

  # Swap the merged tree in only after it exists and the merge reported
  # success. The originals are moved aside, never deleted here: an
  # archive that cannot be regenerated does not get removed by a script
  # on the strength of its own exit code.
  ASIDE="$ROOT.pre-merge.$(date -u +%Y%m%dT%H%M%SZ)"
  mv "$ROOT" "$ASIDE" && mv "$MERGED" "$ROOT" || {
    echo "upgrade: could not swap the merged tree into place" >&2
    exit 1
  }
  echo "$(stamp) merged tree is now $ROOT"
  echo
  echo "Originals kept at:"
  echo "  $ASIDE          (old generation)"
  echo "  $OVERLAP_ROOT   (new generation)"
  echo "Remove them once the merged tree has been archived and verified."
fi
