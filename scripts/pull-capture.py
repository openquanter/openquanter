#!/usr/bin/env python3
"""Pull archived capture files from COS down to the long-term archive.

Runs on the archive host, not the capture host. The capture host uploads
and forgets; this pulls and keeps. Splitting it this way means neither
side has to be reachable from the other -- the capture host sits behind
a link with 20% packet loss, and any design that needs a live connection
between the two inherits that.

The staging bucket expires objects after 7 days, so this has a week of
slack: a few failed runs cost nothing, a week of silence loses data.
That is what the heartbeat is for.

Idempotent by construction. An object already present locally at the
same size is skipped, so re-running is free and a partial run resumes
where it stopped. Downloads land on a .part file inside `get_file` and
are renamed once the body is written; the MD5-against-ETag check happens
after that, and a file failing it is removed. So an interrupted transfer
leaves a .part nobody will mistake for a complete object, and a corrupted
one leaves nothing.

Two things this learned the hard way, both on a Synology:

- **It takes a lock.** Two pullers race on the same .part: one renames
  it, the other's rename raises `FileNotFoundError` and ends the run.
- **One object cannot end the run.** An `OSError` on object 97 of 4144
  used to take the remaining 4047 with it, and the next scheduled run
  inherited the same object and the same crash. With a staging bucket
  that expires after seven days, that is how a week of capture is lost.

Exit status is what a scheduled run says about itself: 0 every object
is local, 1 some object could not be fetched, 3 another puller holds
the lock and this run did nothing. The third is separate from the
second because a full backfill outlasts the interval that starts the
next run, so overlap is the normal state of a catching-up archive and
must not read as loss.
"""

import argparse
import hashlib
import os
import sys
import time
import xml.etree.ElementTree as ET
from urllib.parse import quote

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "cos"))
from cos_client import from_env, _sign  # noqa: E402

CHUNK = 1 << 20

# A scheduled run reports through its exit status, and "another puller
# is still going" is not a failure -- the previous run is doing the work
# this one would have done. Sharing status 1 with a genuine transfer
# failure means a monitor either raises on healthy overlap or stays
# quiet through real loss; it cannot do both. 2 is left to argparse,
# which uses it for a usage error.
EXIT_OK = 0
EXIT_FAILED = 1
EXIT_LOCKED = 3


class OnlyOne:
    """Refuse to run beside another puller.

    Two instances race on the same `.part` file: one renames it, the
    other's `os.replace` raises `FileNotFoundError` and takes the whole
    run down. That happened — a scheduled run and a manual one, and the
    manual one appeared to have exited because the check used `pgrep`,
    which does not exist on the Synology this runs on. A liveness test
    that silently reports "not running" is worse than none.

    So the lock is a file, not a process check. `O_EXCL` is atomic on
    every filesystem this touches, and a stale one names the pid that
    left it rather than being cleared automatically: this cannot tell a
    crash from a slow run, and pid reuse makes the obvious check wrong
    rather than merely unreliable.
    """

    def __init__(self, path):
        self.path = path
        self.fd = None

    def __enter__(self):
        try:
            self.fd = os.open(self.path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644)
            os.write(self.fd, f"pid {os.getpid()}\n".encode())
        except FileExistsError:
            try:
                held = open(self.path).read().strip()
            except OSError:
                held = "nothing about itself"
            print(f"pull: another puller holds {self.path} ({held}).\n"
                  f"      If that process is gone, remove the file.",
                  file=sys.stderr)
            raise SystemExit(EXIT_LOCKED)
        return self

    def __exit__(self, *exc):
        if self.fd is not None:
            os.close(self.fd)
            os.unlink(self.path)
        return False


def list_all(cos, prefix):
    """Every key under a prefix, following the truncation marker.

    A single LIST returns at most 1000 keys. Taking only the first page
    would silently stop archiving once the bucket grew past that, and
    the failure would look exactly like "nothing new to pull".
    """
    out, marker = [], ""
    while True:
        headers = {"Host": cos.host}
        auth = _sign(cos.sid, cos.skey, "GET", "/", headers)
        params = f"prefix={quote(prefix, '')}&max-keys=1000"
        if marker:
            params += f"&marker={quote(marker, '')}"
        conn = cos._conn(120)
        try:
            conn.request("GET", f"/?{params}",
                         headers={"Host": cos.host, "Authorization": auth})
            resp = conn.getresponse()
            data = resp.read()
            if resp.status != 200:
                raise RuntimeError(f"list failed: HTTP {resp.status} {data[:200]!r}")
        finally:
            conn.close()

        root = ET.fromstring(data)
        tag = (lambda n: f"{{{root.tag.split('}')[0][1:]}}}{n}"
               if "}" in root.tag else n)
        page = 0
        for c in root.iter(tag("Contents")):
            k = c.find(tag("Key"))
            s = c.find(tag("Size"))
            e = c.find(tag("ETag"))
            if k is None:
                continue
            out.append((k.text,
                        int(s.text) if s is not None else 0,
                        (e.text or "").strip('"') if e is not None else ""))
            page += 1
        trunc = root.find(tag("IsTruncated"))
        if trunc is None or (trunc.text or "").lower() != "true" or page == 0:
            break
        marker = out[-1][0]
    return out


# Idle seconds before a transfer is treated as stalled.
#
# `http.client`'s timeout is per socket operation, not per transfer, so
# this is "no bytes for two minutes" and not "finish within two
# minutes" — a 93 MB object is unaffected. The default is 1800, which on
# a link that stalls turns every blip into half an hour of a process
# sitting in `poll` with a zero-byte `.part` beside it. Measured: that
# is exactly what happened on the first backfill.
STALL_SECONDS = 120

# A stalled object is retried rather than counted out. The link this
# runs over drops packets; one attempt is a coin toss, and giving up
# after one leaves the object for the next scheduled run, which inherits
# the same coin toss against a seven-day expiry.
ATTEMPTS = 4


def fetch(cos, key, local):
    """Download with a stall timeout, retrying a few times."""
    last = None
    for attempt in range(1, ATTEMPTS + 1):
        try:
            return cos.get_file(key, local, timeout=STALL_SECONDS)
        except OSError as e:
            last = e
            # The partial file is the previous attempt's, not this one's.
            try:
                os.remove(local + ".part")
            except OSError:
                pass
            if attempt < ATTEMPTS:
                print(f"pull: retry {attempt}/{ATTEMPTS - 1} {key}: {e}",
                      file=sys.stderr)
                time.sleep(2 * attempt)
    raise last


def md5_of(path):
    h = hashlib.md5()
    with open(path, "rb") as f:
        for b in iter(lambda: f.read(CHUNK), b""):
            h.update(b)
    return h.hexdigest()


def main():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--dest", required=True, help="local archive root")
    p.add_argument("--env", default=os.path.expanduser("~/.oq-cos.env"))
    p.add_argument("--prefix", default="", help="key prefix to mirror")
    p.add_argument("--heartbeat", default=os.environ.get("PULL_HEARTBEAT", ""))
    p.add_argument("--dry-run", action="store_true")
    args = p.parse_args()

    os.makedirs(args.dest, exist_ok=True)
    # The archive host reaches COS over the public endpoint; it is not
    # in the same region, so there is no internal path to use.
    cos = from_env(args.env, internal=False)

    with OnlyOne(os.path.join(args.dest, ".pull.lock")):
        return pull(cos, args)


def pull(cos, args):
    have = new = failed = 0
    bytes_pulled = 0
    objects = list_all(cos, args.prefix)

    for key, size, etag in sorted(objects):
        rel = key[len(args.prefix):] if key.startswith(args.prefix) else key
        local = os.path.join(args.dest, rel)

        if os.path.exists(local) and os.path.getsize(local) == size:
            have += 1
            continue

        if args.dry_run:
            print(f"would pull {rel} ({size:,} bytes)")
            new += 1
            continue

        os.makedirs(os.path.dirname(local), exist_ok=True)
        try:
            ok, detail = fetch(cos, key, local)
        except OSError as e:
            # One object must not end the run. The staging bucket expires
            # after seven days, so a crash on object 97 of 4144 is not an
            # inconvenience — it is the rest of the week's data, and the
            # next scheduled run inherits the same object and the same
            # crash. Counted, named, and stepped over; the summary says
            # how many, and a non-zero exit still reports the failure.
            print(f"pull: error {rel}: {e}", file=sys.stderr)
            for leftover in (local + ".part", local):
                try:
                    os.remove(leftover)
                except OSError:
                    pass
            failed += 1
            continue
        if not ok:
            print(f"pull: failed {rel}: {detail}", file=sys.stderr)
            failed += 1
            continue

        # ETag is the object's MD5 for a single-part upload, which is
        # how the capture host writes them. A mismatch means the bytes
        # on disk are not the bytes in the bucket.
        if etag and len(etag) == 32:
            local_md5 = md5_of(local)
            if local_md5 != etag:
                print(f"pull: MD5 mismatch {rel}: local {local_md5} etag {etag}",
                      file=sys.stderr)
                os.remove(local)
                failed += 1
                continue

        new += 1
        bytes_pulled += size
        print(f"pulled {rel}")

    print()
    print(f"objects in bucket : {len(objects)}")
    print(f"already local     : {have}")
    print(f"pulled            : {new} ({bytes_pulled / (1 << 20):.1f} MiB)")
    print(f"failed            : {failed}")

    if failed:
        return EXIT_FAILED

    if args.heartbeat:
        try:
            import urllib.request
            urllib.request.urlopen(f"{args.heartbeat}&msg=pulled-{new}",
                                   timeout=8).read()
        except Exception:
            pass
    return EXIT_OK


if __name__ == "__main__":
    sys.exit(main())
