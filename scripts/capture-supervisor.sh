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

BIN="${BIN:-/home/ubuntu/oq-capture}"
ROOT="${ROOT:-/home/ubuntu/capture}"
LOGDIR="${LOGDIR:-/home/ubuntu/oq/log/capture}"
FLOOR_GB="${FLOOR_GB:-8}"
# Hourly, not daily: a daily file is only sealed at UTC midnight, so a
# host that dies at 23:00 loses the day. Hourly bounds that to an hour.
# The watchdog must default to the same value the operator started with
# -- a restart that silently picks a different rotation leaves two
# layouts interleaved in one day.
ROTATION="${ROTATION:-hourly}"
SYMBOLS="${SYMBOLS:-BTCUSDT ETHUSDT BNBUSDT HYPEUSDT}"
STREAMS="${STREAMS:-depth bookTicker trade forceOrder markPrice}"

mkdir -p "$LOGDIR"
[ -x "$BIN" ] || { echo "supervisor: $BIN 不可执行" >&2; exit 2; }

rot_args=()
[ -n "$ROTATION" ] && rot_args=(--rotation "$ROTATION")

started=0
alive=0

for sym in $SYMBOLS; do
  for st in $STREAMS; do
    # Match on the exact flag pair so BTCUSDT never matches BTCUSDT-something
    # and depth never matches a future depth20 stream.
    if pgrep -f -- "--symbol $sym --stream $st\$" >/dev/null 2>&1 \
       || pgrep -f -- "--symbol $sym --stream $st " >/dev/null 2>&1; then
      alive=$((alive + 1))
      continue
    fi
    setsid nohup nice -n 5 "$BIN" \
      --root "$ROOT" --symbol "$sym" --stream "$st" \
      --floor-gb "$FLOOR_GB" "${rot_args[@]}" \
      >> "$LOGDIR/$sym-$st.log" 2>&1 < /dev/null &
    started=$((started + 1))
    echo "$(date -u '+%Y-%m-%dT%H:%M:%SZ') started $sym $st"
  done
done

sleep 2
running=$(pgrep -xc oq-capture 2>/dev/null) || running=0
expected=0
for sym in $SYMBOLS; do
  for _st in $STREAMS; do expected=$((expected + 1)); done
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
