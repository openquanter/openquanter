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
where it stopped. Downloads land on a .part file and are renamed only
after the MD5 matches the ETag COS reported, so an interrupted transfer
can never be mistaken for a complete one.
"""

import argparse
import hashlib
import os
import sys
import xml.etree.ElementTree as ET
from urllib.parse import quote

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "cos"))
from cos_client import from_env, _sign  # noqa: E402

CHUNK = 1 << 20


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

    objects = list_all(cos, args.prefix)
    have = new = failed = 0
    bytes_pulled = 0

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
        ok, detail = cos.get_file(key, local)
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
        return 1

    if args.heartbeat:
        try:
            import urllib.request
            urllib.request.urlopen(f"{args.heartbeat}&msg=pulled-{new}",
                                   timeout=8).read()
        except Exception:
            pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
