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
ROTATION="${ROTATION:-}"          # empty until a binary that supports it is deployed
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
running=$(pgrep -xc oq-capture 2>/dev/null || echo 0)
echo "$(date -u '+%Y-%m-%dT%H:%M:%SZ') alive=$alive started=$started running=$running"
