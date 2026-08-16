#!/usr/bin/env bash
# Replace the capture binary: stop, swap, start.
#
# Every stream is down from the moment it notices SIGTERM until the new
# one connects -- measured at 70-75 seconds, bounded by the websocket
# read timeout on streams with nothing to say. The hole is real, and it
# is marked: each resumed window writes a gap record into the stream and
# counts it in the manifest, so a replay can see exactly what is missing
# rather than reading silence as a quiet market.
#
# A zero-gap mode was written and withdrawn. Running a second generation
# on a second archive root does keep both recording across the
# changeover, and oq-merge stitches the trees back together correctly.
# What it could not do is end where it started: switching the canonical
# root back either reopens the gap it was avoiding, or merges into a
# directory a live process is writing, which corrupts it. The version
# that shipped did neither -- it left both generations running, renamed
# the archive root out from under the processes that had files open in
# it, and sent manifests to one tree while the data went to another. The
# archive then reported success while recording nothing.
#
# So this does one thing. A 70-second marked gap per upgrade, a few
# times a year, is a known and visible cost; a zero-gap mode that
# silently misroutes data is not. oq-merge is kept -- it is correct and
# useful for reconciling trees after any incident.
#
# The binary is swapped with rename(2), never with cp. Writing into a
# running executable fails with ETXTBSY, and a cp whose exit code nobody
# checked is how an "upgrade" silently leaves the old build in place --
# which is exactly what happened the first time this was done by hand.

set -uo pipefail

BIN="${BIN:-/home/ubuntu/oq-capture}"
NEW="${NEW:-}"
ROOT="${ROOT:-/home/ubuntu/capture}"
SUPERVISOR="${SUPERVISOR:-/home/ubuntu/oq/capture-supervisor.sh}"

while [ $# -gt 0 ]; do
  case "$1" in
    --new) NEW="${2:?}"; shift 2 ;;
    --help|-h) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
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

echo "$(stamp) running=$(running "$ROOT")"

# rename(2) swaps the directory entry. Processes already executing the
# old inode keep running it untouched; the next exec picks up the new
# one. cp writes into the live inode instead and fails with ETXTBSY.
cp "$NEW" "$BIN.incoming" || { echo "upgrade: cannot stage new binary" >&2; exit 1; }
chmod +x "$BIN.incoming"
cp -f "$BIN" "$BIN.previous" 2>/dev/null || true
if ! mv -f "$BIN.incoming" "$BIN"; then
  echo "upgrade: rename failed, old binary left in place" >&2
  exit 1
fi
cmp -s "$BIN" "$NEW" || { echo "upgrade: binary does not match after swap" >&2; exit 1; }
echo "$(stamp) binary swapped (previous kept at $BIN.previous)"

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

