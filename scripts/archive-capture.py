#!/usr/bin/env python3
"""Ship sealed capture files to object storage, then reclaim local space.

The order is the whole point:

    compress -> upload -> confirm the ETag -> delete locally

Never the other way round and never skipping the confirmation. An
upload returning 200 means the service accepted the request; a matching
ETag means the bytes it stored are the bytes we sent. Capture is the
part that cannot be redone -- a missing hour of order book is gone for
good -- so the local copy is the last thing to go.

Why object storage rather than a direct copy to the archive NAS,
measured on this exact path:

    capture host -> archive NAS, VPN tunnel      0.013 MB/s
    capture host -> object storage, same region  53      MB/s

The tunnel crosses a link with 20% packet loss at 158 ms RTT, which
collapses TCP to roughly 20 KB/s (Mathis). That is under the ~11 KB/s
sustained average the capture actually produces -- no usable margin.
The upload leg here is four thousand times faster and, being
same-account same-region, carries no traffic charge. The NAS then pulls
from COS on its own schedule, a leg that measured 0.7-1.1 MB/s.

Files still being written are skipped: a capture file counts as sealed
once its manifest exists, because the writer emits the manifest last.
A size-stability check is the second guard.

Compression defaults to zstd level 9, measured on real capture data on
this two-core host:

    level  3   7.9x  151 MB/s
    level  9   8.9x   63 MB/s     <- default
    level 12   8.8x   30 MB/s     <- dominated: worse ratio AND slower
    level 19  10.0x    2.7 MB/s

Level 19 buys 12% for 23x the CPU. Against terabytes of archive space
that saving is worth nothing, and the CPU is shared with capture.
Level 12 is listed because it looks like a sensible middle and is not.
"""

import argparse
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "cos"))
from cos_client import from_env  # noqa: E402


def open_files_on_host():
    """Every regular file currently held open by any process.

    Read from /proc rather than inferred, because the alternative --
    "it has not been written to recently" -- is wrong for exactly the
    streams that matter. A liquidation feed can idle for hours; its
    file is indistinguishable by mtime from one whose writer died. Act
    on that guess and the archive deletes a file out from under a live
    capture, which keeps writing to the unlinked inode until it exits
    and takes the data with it.
    """
    held = set()
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        fd_dir = f"/proc/{pid}/fd"
        try:
            for fd in os.listdir(fd_dir):
                try:
                    held.add(os.path.realpath(os.path.join(fd_dir, fd)))
                except OSError:
                    pass
        except OSError:
            # The process exited between listdir and open, or belongs
            # to another user. Either way it is not ours to inspect.
            continue
    return held


def archivable(root, stale_minutes):
    """Yield (raw, manifest_or_None) for every file that should ship.

    Two cases, and the second is the one that matters:

    * **Sealed.** A manifest exists, so the writer closed the file
      normally. Normal rotation and clean shutdown both land here.

    * **Orphaned.** No manifest and no process holding it open. The
      capture died -- OOM, power loss, a restart, an operator's kill --
      without running its seal step.

    Only handling the sealed case would strand data exactly when
    something went wrong, which is when it is least affordable. The
    frame format tolerates a torn tail, so an orphan is still readable;
    what it lacks is the manifest's record count and time bounds, and
    that is no reason to leave it on a disk that is filling.

    Liveness is decided by an open file descriptor, with staleness only
    as a second, weaker guard.
    """
    held = open_files_on_host()
    cutoff = time.time() - stale_minutes * 60
    for dirpath, _dirs, files in os.walk(root):
        for name in sorted(files):
            if not name.endswith(".oqcap"):
                continue
            raw = os.path.join(dirpath, name)

            # An open descriptor beats every other signal, manifest
            # included. A capture restarted mid-window reopens the
            # window's file and appends to it, which leaves a manifest
            # that is real but stale -- it describes fewer records than
            # the file now holds. Trusting the manifest alone would
            # upload and delete a file a live capture is still writing,
            # and the process would go on writing to the unlinked inode
            # until it exited.
            if os.path.realpath(raw) in held:
                continue

            manifest = raw[: -len(".oqcap")] + ".manifest.json"
            if os.path.exists(manifest):
                yield raw, manifest
            elif os.path.getmtime(raw) < cutoff:
                yield raw, None


def is_quiescent(path, settle=1.0):
    """True if the file stopped growing -- the writer is done with it."""
    a = os.path.getsize(path)
    time.sleep(settle)
    return a == os.path.getsize(path)


def compress(raw, level):
    out = raw + ".zst"
    if os.path.exists(out):
        return out
    rc = subprocess.call(
        ["zstd", "-q", f"-{level}", "--long", "-o", out, raw],
        stdout=subprocess.DEVNULL,
    )
    return out if rc == 0 else None


def free_gb(path):
    st = os.statvfs(path)
    return st.f_bavail * st.f_frsize / (1 << 30)


def main():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--root", required=True, help="capture root directory")
    p.add_argument("--env", default=os.path.expanduser("~/.oq-cos.env"),
                   help="file holding COS_SECRET_ID / COS_SECRET_KEY / COS_APPID")
    p.add_argument("--prefix", default="", help="key prefix inside the bucket")
    p.add_argument("--level", type=int, default=9, help="zstd level [9]")
    p.add_argument("--public", action="store_true",
                   help="use the public endpoint (billed); default is internal")
    p.add_argument("--keep-hours", type=float, default=0,
                   help="keep archived files locally for this long before deleting")
    p.add_argument("--heartbeat", default=os.environ.get("ARCHIVE_HEARTBEAT", ""),
                   help="URL pinged only on a fully clean run")
    p.add_argument("--stale-minutes", type=float, default=20,
                   help="a manifest-less file untouched this long is orphaned [20]")
    p.add_argument("--dry-run", action="store_true")
    args = p.parse_args()

    if not os.path.isdir(args.root):
        sys.exit(f"archive: {args.root} does not exist")

    cos = None if args.dry_run else from_env(args.env, internal=not args.public)

    found = shipped = open_files = failed = orphans = 0
    now = time.time()

    for raw, manifest in archivable(args.root, args.stale_minutes):
        if not is_quiescent(raw):
            open_files += 1
            continue
        found += 1
        if manifest is None:
            orphans += 1
        rel = os.path.relpath(raw, args.root)

        if args.dry_run:
            print(f"would archive {rel} ({os.path.getsize(raw)} bytes)")
            continue

        blob = compress(raw, args.level)
        if blob is None:
            print(f"archive: compression failed for {rel}", file=sys.stderr)
            failed += 1
            continue

        key = f"{args.prefix.rstrip('/')}/{rel}.zst" if args.prefix else f"{rel}.zst"
        mkey = key[: -len(".oqcap.zst")] + ".manifest.json"

        ok, detail = cos.put_file(key, blob)
        if not ok:
            print(f"archive: upload failed for {rel}: {detail}", file=sys.stderr)
            failed += 1
            continue

        if manifest is not None:
            ok_m, detail_m = cos.put_file(mkey, manifest)
            if not ok_m:
                print(f"archive: manifest upload failed for {rel}: {detail_m}",
                      file=sys.stderr)
                failed += 1
                continue

        # Only now is the remote copy known-good. Deleting before this
        # point would trade a recoverable disk-space problem for an
        # unrecoverable data-loss one.
        age_h = (now - os.path.getmtime(raw)) / 3600.0
        if age_h >= args.keep_hours:
            os.remove(raw)
            os.remove(blob)
            if manifest is not None:
                os.remove(manifest)
            tag = "" if manifest is not None else " [orphan, no manifest]"
            print(f"archived + removed {rel}{tag}")
        else:
            print(f"archived (kept locally, {age_h:.1f}h old) {rel}")
        shipped += 1

    print()
    print(f"files found        : {found} ({orphans} orphaned, no manifest)")
    print(f"archived + verified: {shipped}")
    print(f"still being written: {open_files}")
    print(f"failed             : {failed}")
    print(f"free space         : {free_gb(args.root):.1f} GiB")

    if failed:
        print("\nLocal copies of the failed files were kept. Nothing is deleted "
              "until its ETag has been confirmed at the destination.")
        return 1

    # A cron job that stops running is invisible; a monitor that stops
    # hearing from it is not. Fires only on a clean run, so a silent
    # failure raises an alert instead of passing unnoticed.
    if args.heartbeat:
        try:
            import urllib.request
            urllib.request.urlopen(f"{args.heartbeat}&msg=archived-{shipped}",
                                   timeout=8).read()
        except Exception:
            pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
