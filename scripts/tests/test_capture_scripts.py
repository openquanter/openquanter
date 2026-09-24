"""Tests for the capture archive scripts, standard library only.

    python3 -m unittest discover -s scripts/tests

The scripts run unattended on the capture host and the archive NAS, and
both of their worst failures -- deleting data that was never uploaded,
and writing outside the archive -- are silent where they happen.
"""

import importlib.util
import os
import shutil
import sys
import tempfile
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPTS = os.path.dirname(HERE)
sys.path.insert(0, os.path.join(SCRIPTS, "cos"))


def load(name, file):
    spec = importlib.util.spec_from_file_location(name, os.path.join(SCRIPTS, file))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


archive = load("archive_capture", "archive-capture.py")
pull = load("pull_capture", "pull-capture.py")


class ObjectKeysStayInsideTheArchive(unittest.TestCase):
    def setUp(self):
        self.dest = tempfile.mkdtemp()

    def tearDown(self):
        shutil.rmtree(self.dest)

    def test_an_ordinary_key_lands_under_the_destination(self):
        got = pull.local_path(self.dest, "binance-perp/BTCUSDT/depth/2026-09-01/00.oqcap.zst")
        self.assertTrue(got.startswith(os.path.realpath(self.dest) + os.sep), got)

    def test_keys_that_climb_out_or_are_absolute_are_refused(self):
        for rel in ["../x", "a/../../b", "/etc/passwd", "a//b", "./a", "", "a\\..\\b",
                    "..", "a/.."]:
            self.assertIsNone(pull.local_path(self.dest, rel), rel)

    def test_a_link_inside_the_archive_does_not_lead_out_of_it(self):
        outside = tempfile.mkdtemp()
        try:
            os.symlink(outside, os.path.join(self.dest, "link"))
            self.assertIsNone(pull.local_path(self.dest, "link/x"))
        finally:
            shutil.rmtree(outside)


@unittest.skipUnless(shutil.which("zstd"), "zstd is not installed")
class ABlobIsReusedOnlyForTheBytesItWasMadeFrom(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp()
        self.raw = os.path.join(self.dir, "00.oqcap")
        with open(self.raw, "wb") as f:
            f.write(b"frame" * 1000)

    def tearDown(self):
        shutil.rmtree(self.dir)

    def test_an_unchanged_file_reuses_its_blob(self):
        blob, sig = archive.compress(self.raw, 3)
        made = os.stat(blob).st_mtime_ns
        time.sleep(0.01)
        again, sig2 = archive.compress(self.raw, 3)
        self.assertEqual((blob, sig), (again, sig2))
        self.assertEqual(os.stat(again).st_mtime_ns, made, "recompressed an unchanged file")

    def test_a_file_appended_to_after_compression_is_compressed_again(self):
        blob, sig = archive.compress(self.raw, 3)
        with open(self.raw, "ab") as f:
            f.write(b"more" * 100)
        again, sig2 = archive.compress(self.raw, 3)
        self.assertNotEqual(sig, sig2)
        with open(again, "rb") as f:
            data = f.read()
        import subprocess
        out = subprocess.run(["zstd", "-dc"], input=data, capture_output=True).stdout
        self.assertTrue(out.endswith(b"more" * 100), "the blob does not cover the appended bytes")

    def test_a_half_written_blob_is_not_reused(self):
        with open(self.raw + ".zst", "wb") as f:
            f.write(b"\x28\xb5\x2f\xfd truncated")
        blob, _ = archive.compress(self.raw, 3)
        import subprocess
        self.assertEqual(subprocess.run(["zstd", "-t", "-q", blob]).returncode, 0)

    def test_a_file_changed_after_upload_is_not_deleted(self):
        _, sig = archive.compress(self.raw, 3)
        with open(self.raw, "ab") as f:
            f.write(b"late")
        self.assertFalse(archive.unchanged_and_unheld(self.raw, sig))


@unittest.skipUnless(os.path.isdir("/proc"), "needs /proc")
class AFileSomeoneHasOpenIsNotDeleted(unittest.TestCase):
    def test_an_open_file_is_held(self):
        d = tempfile.mkdtemp()
        try:
            raw = os.path.join(d, "00.oqcap")
            with open(raw, "wb") as f:
                f.write(b"x")
            sig = archive.signature(raw)
            with open(raw, "ab"):
                self.assertFalse(archive.unchanged_and_unheld(raw, sig))
            self.assertTrue(archive.unchanged_and_unheld(raw, sig))
        finally:
            shutil.rmtree(d)


if __name__ == "__main__":
    unittest.main()
