#!/usr/bin/env python3
"""Minimal Tencent COS client — stdlib only, streaming.

The capture host should not need a package manager to ship its own
data. COS's signature is a short, stable algorithm, so implementing it
here costs less than carrying an SDK and its dependencies on a 1 GB
box.

Two properties matter more than features:

* **Streaming.** Bodies are handed to http.client as file objects, so a
  600 MB capture file never lands in memory. Reading it whole would OOM
  the capture host, which is the one machine that must not fall over.

* **Verification without egress.** COS returns the object's MD5 as the
  ETag for a single-part PUT. Comparing that against a locally computed
  MD5 proves the bytes arrived without downloading them back, so the
  integrity check costs nothing in traffic. Re-reading the object to
  check it would pay the per-GB egress rate for every file archived.

Signature reference: Tencent COS Signature v5 (q-sign-algorithm=sha1).
"""

import hashlib
import hmac
import http.client
import os
import time
from urllib.parse import quote

CHUNK = 1 << 20


def _sign(secret_id, secret_key, method, uri, headers, expire=1800):
    now = int(time.time())
    keytime = f"{now - 60};{now + expire}"
    sign_key = hmac.new(secret_key.encode(), keytime.encode(), hashlib.sha1).hexdigest()

    pairs = sorted((k.lower(), v) for k, v in headers.items())
    header_list = ";".join(k for k, _ in pairs)
    http_headers = "&".join(f"{quote(k, '')}={quote(str(v), '')}" for k, v in pairs)

    http_string = f"{method.lower()}\n{uri}\n\n{http_headers}\n"
    string_to_sign = "sha1\n{}\n{}\n".format(
        keytime, hashlib.sha1(http_string.encode()).hexdigest()
    )
    signature = hmac.new(
        sign_key.encode(), string_to_sign.encode(), hashlib.sha1
    ).hexdigest()

    return (
        "q-sign-algorithm=sha1"
        f"&q-ak={secret_id}&q-sign-time={keytime}&q-key-time={keytime}"
        f"&q-header-list={header_list}&q-url-param-list=&q-signature={signature}"
    )


class Cos:
    """One bucket in one region, reached over the internal endpoint.

    The internal endpoint is not a performance tweak: same-region
    traffic between Tencent products carries no charge, and naming it
    explicitly is what keeps the upload leg free rather than relying on
    the public domain happening to route internally.
    """

    def __init__(self, secret_id, secret_key, appid, region,
                 bucket_prefix="oq-capture", internal=True):
        self.sid = secret_id
        self.skey = secret_key
        self.bucket = f"{bucket_prefix}-{appid}"
        if internal:
            self.host = f"{self.bucket}.cos-internal.{region}.tencentcos.cn"
            self.tls = False
        else:
            self.host = f"{self.bucket}.cos.{region}.myqcloud.com"
            self.tls = True

    def _conn(self, timeout):
        cls = http.client.HTTPSConnection if self.tls else http.client.HTTPConnection
        return cls(self.host, timeout=timeout)

    def _request(self, method, key, body=None, length=None, timeout=1800):
        uri = "/" + key.lstrip("/")
        headers = {"Host": self.host}
        if length is not None:
            headers["Content-Length"] = str(length)
        signed = dict(headers)
        signed["Authorization"] = _sign(self.sid, self.skey, method, uri, headers)

        conn = self._conn(timeout)
        try:
            conn.request(method, uri, body=body, headers=signed)
            resp = conn.getresponse()
            payload = resp.read() if method != "GET" else None
            return resp.status, dict(resp.getheaders()), payload, resp
        finally:
            if method != "GET":
                conn.close()

    def put_file(self, key, path, timeout=1800):
        """Stream a file up. Returns (ok, detail).

        Integrity is confirmed against the ETag rather than by reading
        the object back, so a verified upload costs no egress.
        """
        size = os.path.getsize(path)
        md5 = hashlib.md5()
        with open(path, "rb") as f:
            for block in iter(lambda: f.read(CHUNK), b""):
                md5.update(block)
        local_etag = md5.hexdigest()

        with open(path, "rb") as f:
            status, headers, body, _ = self._request("PUT", key, f, size, timeout)

        if status != 200:
            return False, f"HTTP {status}: {(body or b'')[:200]!r}"

        remote_etag = (headers.get("ETag") or headers.get("etag") or "").strip('"')
        if remote_etag and remote_etag != local_etag:
            return False, f"ETag mismatch: local {local_etag} remote {remote_etag}"
        return True, local_etag

    def head(self, key, timeout=60):
        status, headers, _, _ = self._request("HEAD", key, None, None, timeout)
        return status, headers

    def delete(self, key, timeout=60):
        status, _, body, _ = self._request("DELETE", key, None, None, timeout)
        return status in (200, 204), status

    def get_file(self, key, path, timeout=1800):
        """Stream an object down to a path. Returns (ok, sha256|error)."""
        uri = "/" + key.lstrip("/")
        headers = {"Host": self.host}
        signed = dict(headers)
        signed["Authorization"] = _sign(self.sid, self.skey, "GET", uri, headers)
        conn = self._conn(timeout)
        try:
            conn.request("GET", uri, headers=signed)
            resp = conn.getresponse()
            if resp.status != 200:
                return False, f"HTTP {resp.status}"
            sha = hashlib.sha256()
            tmp = path + ".part"
            with open(tmp, "wb") as f:
                while True:
                    block = resp.read(CHUNK)
                    if not block:
                        break
                    sha.update(block)
                    f.write(block)
            os.replace(tmp, path)
            return True, sha.hexdigest()
        finally:
            conn.close()

    def list(self, prefix="", max_keys=1000, timeout=120):
        """List keys under a prefix. Returns a list of (key, size)."""
        import xml.etree.ElementTree as ET

        uri = "/"
        headers = {"Host": self.host}
        signed = dict(headers)
        signed["Authorization"] = _sign(self.sid, self.skey, "GET", uri, headers)
        conn = self._conn(timeout)
        try:
            q = f"/?prefix={quote(prefix, '')}&max-keys={max_keys}"
            conn.request("GET", q, headers=signed)
            resp = conn.getresponse()
            data = resp.read()
            if resp.status != 200:
                return []
            root = ET.fromstring(data)
            ns = {"s3": root.tag.split("}")[0].strip("{")} if "}" in root.tag else {}
            out = []
            for c in root.findall(".//s3:Contents" if ns else ".//Contents", ns):
                k = c.find("s3:Key" if ns else "Key", ns)
                s = c.find("s3:Size" if ns else "Size", ns)
                if k is not None:
                    out.append((k.text, int(s.text) if s is not None else 0))
            return out
        finally:
            conn.close()


def require(env, key):
    """A setting with no default, because the default would be a fact.

    Where a bucket lives is not a secret the way a key is, but it is the
    half of a deployment a reader cannot change and an attacker does not
    have to guess. A default in source publishes it to everyone who
    clones; an environment variable publishes it to whoever runs the
    thing, which is the audience it was meant for.
    """
    value = env.get(key)
    if not value:
        raise SystemExit(f"{key} is not set; it has no default on purpose")
    return value


def from_env(path, internal=True):
    env = {}
    with open(path) as f:
        for line in f:
            if "=" in line and not line.strip().startswith("#"):
                k, v = line.strip().split("=", 1)
                env[k] = v
    return Cos(
        env["COS_SECRET_ID"],
        env["COS_SECRET_KEY"],
        env["COS_APPID"],
        require(env, "COS_REGION"),
        env.get("COS_BUCKET_PREFIX", "oq-capture"),
        internal=internal,
    )
