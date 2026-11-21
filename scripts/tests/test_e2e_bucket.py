# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/lib/e2e-bucket.sh, scripts/test-e2e.sh's bucket in memory (board 1764): a run
killed outright leaves its bucket, which the next run removes; a live run's, and one of a tree
from before, which names no owner, stay."""

import os
import shutil
import signal
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from script import SCRIPTS

LIBRARY = SCRIPTS / "lib" / "e2e-bucket.sh"
# A run: makes its bucket under $1, says where, and lives until killed.
RUN = f'set -e; source {LIBRARY}; e2e_bucket "$1"; echo "$e2e_bucket_dir"; exec sleep 300'


class E2eBucket(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root)
        self.env = dict(os.environ)
        # util-linux's flock where the host has none (macOS): fcntl's, as the tests' hosts have.
        if shutil.which("flock") is None:
            bin_dir = self.root / "bin"
            bin_dir.mkdir()
            (bin_dir / "flock").write_text(
                f'#!/bin/sh\nexec {sys.executable} {SCRIPTS / "tests" / "fake_flock.py"} "$@"\n')
            (bin_dir / "flock").chmod(0o755)
            self.env["PATH"] = f"{bin_dir}:{self.env['PATH']}"
        self.buckets = self.root / "shm"
        self.buckets.mkdir()

    def start(self):
        """Starts a run in a session of its own: the run and its bucket."""
        run = subprocess.Popen(["bash", "-c", RUN, "run", str(self.buckets)], env=self.env,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                               start_new_session=True)
        self.addCleanup(run.wait)
        self.addCleanup(lambda: run.poll() is None and os.killpg(run.pid, signal.SIGKILL))
        self.addCleanup(run.stdout.close)
        self.addCleanup(run.stderr.close)
        return run, Path(run.stdout.readline().strip())

    def test_the_bucket_of_a_run_killed_outright_goes_with_the_next_run(self):
        killed, gone = self.start()
        live, kept = self.start()
        # A tree's from before, which names no owner.
        older = self.buckets / "cladefs-e2e-bucket.before"
        older.mkdir()
        os.killpg(killed.pid, signal.SIGKILL)
        killed.wait()
        _, made = self.start()
        buckets = self.buckets.glob("cladefs-e2e-bucket.*")
        self.assertEqual(sorted(path.name for path in buckets if path.is_dir()),
                         sorted([kept.name, older.name, made.name]))
        self.assertTrue(gone.name not in (kept.name, made.name))
        self.assertEqual(live.poll(), None)


if __name__ == "__main__":
    unittest.main()
