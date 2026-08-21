#!/bin/sh
# Run the archive pull on a schedule and leave a record of how it went.
#
# The pull itself is in pull-capture.py; this is only the part a cron
# entry needs and a manual run does not. Kept in the repository rather
# than written by hand on the archive host, because the exit codes it
# branches on are that script's interface -- the two have to change
# together, and a copy that lives only on one Synology cannot.
#
#   pull-capture-cron.sh <archive-root>
#
# Leaves .last-success or .last-failure in the archive root. Nothing
# reads them yet; they exist so that "when did this last work?" has an
# answer that does not require reading a log, and so a monitor added
# later has something to look at. Silence is the failure mode that
# loses data here, so the record has to outlive the run that made it.
set -u

DEST=${1:?usage: pull-capture-cron.sh <archive-root>}
HERE=$(dirname "$(readlink -f "$0")")
LOG=${OQ_PULL_LOG:-$HERE/log/pull.log}
MAX_LOG=$((20 * 1024 * 1024))

# cron does not promise HOME, and the credential default is
# ~/.oq-cos.env -- which under an unset HOME resolves somewhere the
# credentials are not. Refusing here names the real problem; letting
# it through produces an authentication error that reads like COS
# rejecting the key.
ENV_FILE=${OQ_COS_ENV:-${HOME:-}/.oq-cos.env}

stamp() { date '+%Y-%m-%dT%H:%M:%S%z'; }

mkdir -p "$(dirname "$LOG")" "$DEST"

if [ ! -r "$ENV_FILE" ]; then
    echo "pull-capture-cron: no readable credentials at $ENV_FILE" >&2
    echo "                   set OQ_COS_ENV to their path" >&2
    stamp > "$DEST/.last-failure"
    exit 1
fi

# Rotate before the run, never during, so one run's output is never
# split across two files.
if [ -f "$LOG" ] && [ "$(wc -c < "$LOG")" -gt "$MAX_LOG" ]; then
    mv "$LOG" "$LOG.1"
fi

status=0
{
    echo "=== $(stamp) start ==="
    python3 "$HERE/pull-capture.py" --dest "$DEST" --env "$ENV_FILE" || status=$?
    echo "=== $(stamp) end, status $status ==="
} >> "$LOG" 2>&1

case $status in
    0) stamp > "$DEST/.last-success" ;;
    3) : ;;  # A backfill is still going. It owns the outcome, not this run.
    *) stamp > "$DEST/.last-failure" ;;
esac

exit $status
