#!/bin/bash
# Keep one capture process alive per (symbol, stream) pair.
#
# Run it from cron every few minutes. It starts what is missing and
# leaves what is running alone, so it doubles as both the initial
# launcher and the watchdog -- there is no separate "start" path that
# could drift from the "restart" path.
#
# A capture process that dies is the worst kind of failure here,
# because it is silent and the data it missed cannot be recaptured.
# Anything that exits -- a websocket close the retry loop gives up on,
# the OOM killer, an operator's stray pkill -- is back within one cron
# interval, and the gap is bounded by that interval rather than by how
# long it takes someone to notice.
#
# Deliberately no --minutes: an expiry that nothing renews turns into a
# capture that stops at a time nobody remembers setting.

set -uo pipefail

BIN="${BIN:-$HOME/oq-capture}"
ROOT="${ROOT:-$HOME/capture}"
LOGDIR="${LOGDIR:-$HOME/oq/log/capture}"
FLOOR_GB="${FLOOR_GB:-8}"
# Hourly, not daily: a daily file is only sealed at UTC midnight, so a
# host that dies at 23:00 loses the day. Hourly bounds that to an hour.
# The watchdog must default to the same value the operator started with
# -- a restart that silently picks a different rotation leaves two
# layouts interleaved in one day.
ROTATION="${ROTATION:-hourly}"
# What to keep alive, as `venue:symbol:stream,stream,...` entries.
#
# One list rather than a symbol list crossed with a stream list, because
# venues do not offer the same streams: this one has no bookTicker or
# forceOrder channel under these names, and generating the cross product
# would have the watchdog forever restarting streams that cannot exist.
# The venue a process without an explicit --venue flag is running.
# Older processes predate the flag; they are still that venue.
DEFAULT_VENUE="${DEFAULT_VENUE:-binance-perp}"

PLAN="${PLAN:-\
binance-perp:BTCUSDT:depth,bookTicker,trade,forceOrder,markPrice \
binance-perp:ETHUSDT:depth,bookTicker,trade,forceOrder,markPrice \
binance-perp:BNBUSDT:depth,bookTicker,trade,forceOrder,markPrice \
binance-perp:HYPEUSDT:depth,bookTicker,trade,forceOrder,markPrice \
okx-swap:BTCUSDT:depth,trade}"

mkdir -p "$LOGDIR"
[ -x "$BIN" ] || { echo "supervisor: $BIN 不可执行" >&2; exit 2; }

rot_args=()
[ -n "$ROTATION" ] && rot_args=(--rotation "$ROTATION")

started=0
alive=0

for entry in $PLAN; do
  venue=${entry%%:*}
  rest=${entry#*:}
  sym=${rest%%:*}
  streams=$(printf '%s' "${rest#*:}" | tr ',' ' ')

  for st in $streams; do
    # Match on venue and root as well as symbol and stream. Without the
    # venue, the same symbol on two venues looks like one process, so
    # the second is never started; without the root, a second generation
    # under a different archive root looks like the first is already
    # running.
    # Match with and without an explicit --venue, because a process
    # started before the flag existed does not carry it and is running
    # the same stream all the same.
    #
    # This is not hypothetical. A version of this check that required
    # --venue did not recognise twenty running captures, declared them
    # all missing, and started a second copy of each — every five
    # minutes, from cron, for as long as it took to notice. Two writers
    # on one file interleave their messages, and the hour they shared
    # took 41 sequence breaks to repair.
    if pgrep -f -- "--root $ROOT --venue $venue --symbol $sym --stream $st" >/dev/null 2>&1 \
       || { [ "$venue" = "$DEFAULT_VENUE" ] \
            && pgrep -f -- "--root $ROOT --symbol $sym --stream $st" >/dev/null 2>&1; }; then
      alive=$((alive + 1))
      continue
    fi
    setsid nohup nice -n 5 "$BIN" \
      --root "$ROOT" --venue "$venue" --symbol "$sym" --stream "$st" \
      --floor-gb "$FLOOR_GB" "${rot_args[@]}" \
      >> "$LOGDIR/$venue-$sym-$st.log" 2>&1 < /dev/null &
    started=$((started + 1))
    echo "$(date -u '+%Y-%m-%dT%H:%M:%SZ') started $venue $sym $st"
  done
done

sleep 2
# Count only this root's generation. During an overlapping upgrade two
# generations run at once, and a plain process-name count would report
# double and never match the expected number.
# Count by process name and archive root together. `pgrep -f` alone
# matches anything whose command line contains the root — including this
# script's own children while they are still being exec'd — which made
# the count read one high and would have failed the heartbeat's
# equality check at random.
running=$(pgrep -x "$(basename "$BIN")" 2>/dev/null | while read -r pid; do
  tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -qF -- "--root $ROOT " && echo x
done | wc -l | tr -d ' ')
running=${running:-0}
expected=0
for entry in $PLAN; do
  streams=$(printf '%s' "${entry#*:*:}" | tr ',' ' ')
  for _st in $streams; do expected=$((expected + 1)); done
done
echo "$(date -u '+%Y-%m-%dT%H:%M:%SZ') alive=$alive started=$started running=$running/$expected"

# Heartbeat only when every stream is up, and only from here.
#
# The archive job cannot stand in for this. If capture stops entirely --
# the disk floor is hit, the binary is missing, the venue rejects every
# connection -- the archive still runs, finds nothing to do, exits zero
# and reports success. Both of the other monitors would stay green while
# not a single message was being recorded. Liveness has to be asserted
# by the thing that knows how many streams there should be.
if [ -n "${CAPTURE_HEARTBEAT:-}" ] && [ "$running" -eq "$expected" ]; then
  curl -fsS --max-time 8 "${CAPTURE_HEARTBEAT}&msg=streams-${running}" >/dev/null 2>&1 || true
fi
