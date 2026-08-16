#!/usr/bin/env bash
# Seal, compress, ship and verify captured files, then reclaim space.
#
# Usage:
#   ARCHIVE_DEST=user@host:/path scripts/archive-capture.sh --root /var/capture
#   scripts/archive-capture.sh --root /var/capture --dry-run
#
# Run it on a timer. Every closed capture file is compressed, copied to
# the archive, verified *there* by recomputing its hash, and only then
# deleted locally.
#
# The order is the whole point:
#
#   compress → transfer → verify at the destination → delete locally
#
# Never the other way round, and never skipping the verify. `rsync`
# exiting zero means it believes it wrote the bytes; recomputing the
# hash on the far side means the bytes are there. Capture is the part
# that cannot be redone, so the local copy is the last thing to go.
#
# Files still being written are skipped. With hourly rotation the open
# file is at most an hour old, which is what makes a small disk viable:
# the local peak is roughly two rotation periods, not two days.

set -euo pipefail

ROOT=""
DEST="${ARCHIVE_DEST:-}"
DRY_RUN=0
KEEP_HOURS="${KEEP_HOURS:-2}"
ZSTD_LEVEL="${ZSTD_LEVEL:-19}"

while [ $# -gt 0 ]; do
  case "$1" in
    --root) ROOT="${2:?}"; shift 2 ;;
    --dest) DEST="${2:?}"; shift 2 ;;
    --keep-hours) KEEP_HOURS="${2:?}"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --help|-h)
      sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) echo "archive: unknown argument $1" >&2; exit 2 ;;
  esac
done

[ -n "$ROOT" ] || { echo "archive: --root is required" >&2; exit 2; }
[ -d "$ROOT" ] || { echo "archive: $ROOT does not exist" >&2; exit 2; }
if [ "$DRY_RUN" -eq 0 ] && [ -z "$DEST" ]; then
  echo "archive: set --dest or ARCHIVE_DEST (user@host:/path)" >&2
  exit 2
fi

for tool in zstd rsync sha256sum; do
  command -v "$tool" >/dev/null || {
    echo "archive: missing $tool" >&2
    exit 2
  }
done

# A capture file is finished when its manifest exists: the writer emits
# the manifest as the last step of sealing, so its presence is the
# signal that nothing more will be appended.
sealed=0
shipped=0
skipped_open=0
failed=0

while IFS= read -r manifest; do
  raw="${manifest%.manifest.json}.oqcap"
  [ -f "$raw" ] || continue

  # Guard against a manifest written for a file that is somehow still
  # growing: compare size across a short pause.
  size_a=$(stat -c %s "$raw" 2>/dev/null || stat -f %z "$raw")
  sleep 1
  size_b=$(stat -c %s "$raw" 2>/dev/null || stat -f %z "$raw")
  if [ "$size_a" != "$size_b" ]; then
    skipped_open=$((skipped_open + 1))
    continue
  fi

  sealed=$((sealed + 1))
  rel="${raw#"$ROOT"/}"
  compressed="$raw.zst"

  if [ "$DRY_RUN" -eq 1 ]; then
    printf 'would archive %s (%s bytes)\n' "$rel" "$size_b"
    continue
  fi

  if [ ! -f "$compressed" ]; then
    zstd -q -"$ZSTD_LEVEL" --long -o "$compressed" "$raw" || {
      echo "archive: compression failed for $rel" >&2
      failed=$((failed + 1))
      continue
    }
  fi

  local_hash=$(sha256sum "$compressed" | cut -d' ' -f1)
  remote_dir="${DEST#*:}/$(dirname "$rel")"
  remote_host="${DEST%%:*}"

  ssh "$remote_host" "mkdir -p '$remote_dir'" || {
    echo "archive: cannot create $remote_dir on $remote_host" >&2
    failed=$((failed + 1))
    continue
  }

  rsync -q --partial "$compressed" "$remote_host:$remote_dir/" || {
    echo "archive: transfer failed for $rel" >&2
    failed=$((failed + 1))
    continue
  }
  rsync -q --partial "$manifest" "$remote_host:$remote_dir/" || {
    echo "archive: manifest transfer failed for $rel" >&2
    failed=$((failed + 1))
    continue
  }

  # The verification that matters: the hash recomputed *there*, not the
  # exit code of the tool that claims to have sent it.
  remote_hash=$(ssh "$remote_host" \
    "sha256sum '$remote_dir/$(basename "$compressed")' | cut -d' ' -f1") || {
    echo "archive: cannot verify $rel at the destination" >&2
    failed=$((failed + 1))
    continue
  }

  if [ "$local_hash" != "$remote_hash" ]; then
    echo "archive: HASH MISMATCH for $rel — local $local_hash, remote $remote_hash" >&2
    echo "archive: keeping the local copy; investigate before retrying" >&2
    failed=$((failed + 1))
    continue
  fi

  rm -f "$raw" "$compressed" "$manifest"
  shipped=$((shipped + 1))
  printf 'archived %s\n' "$rel"
done < <(find "$ROOT" -name '*.manifest.json' -type f | sort)

echo
echo "sealed files found : $sealed"
echo "archived + verified: $shipped"
echo "still being written: $skipped_open"
echo "failed             : $failed"

if [ "$failed" -gt 0 ]; then
  echo
  echo "Local copies of the failed files were kept. Nothing is deleted until"
  echo "its hash has been confirmed at the destination."
  exit 1
fi
