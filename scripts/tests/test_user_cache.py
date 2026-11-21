# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/lib/user-cache.sh, the scripts' cache of downloads (TASK-643): made where
XDG_CACHE_HOME says, and a directory it can't make named, through rustfs_binary too."""

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

from script import SCRIPTS


class UserCache(unittest.TestCase):
    def setUp(self):
        self.home = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.home)

    def run_bash(self, command, cache):
        done = subprocess.run(["bash", "-c", command], capture_output=True, text=True,
                              env=dict(os.environ, HOME=str(self.home), XDG_CACHE_HOME=str(cache)))
        return done.returncode, done.stdout, done.stderr

    def test_the_cache_and_a_directory_of_it_are_made(self):
        cache = self.home / "cache"
        self.assertEqual(
            self.run_bash(f"source {SCRIPTS}/lib/user-cache.sh; user_cache; user_cache tla", cache),
            (0, f"{cache}/cladefs\n{cache}/cladefs/tla\n", ""))
        self.assertTrue((cache / "cladefs" / "tla").is_dir())

    def test_a_directory_it_cannot_make_is_named(self):
        # A file where the cache would be: no directory can be made under it, root's run or not.
        cache = self.home / "cache"
        cache.write_text("")
        # rustfs.sh's name for this machine's release.
        platform = {("Darwin", "arm64"): "macos-aarch64", ("Linux", "x86_64"): "linux-x86_64-musl",
                    ("Linux", "aarch64"): "linux-aarch64-musl"}[os.uname().sysname, os.uname().machine]
        self.assertEqual(
            self.run_bash(f"source {SCRIPTS}/lib/rustfs.sh; rustfs_binary", cache),
            (1, "", f"user_cache: couldn't make {cache}/cladefs/rustfs-1.0.4-{platform}\n"))


if __name__ == "__main__":
    unittest.main()
