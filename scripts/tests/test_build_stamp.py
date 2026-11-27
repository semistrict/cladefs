# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of what crates/cladefs_core/build.rs stamps a build with and checks of it
(crates/cladefs_core/build/stamp.rs), as cargo runs it: in a workspace of its own, a git checkout, whose
crate `stamped` builds with the module as cladefs's build script does and prints its stamp, and
depends on a crate `dep`, as cladefs does on local_cell and bucket_cell.

- The stamp follows the tree whichever crate an edit is in (board 8599): an edit to `dep`'s source
  alone left a build saying the clean commit, as its script re-ran only for cladefs's own sources;
  and a revert, `git checkout -- FILE`, makes it clean again.
- A release build without frame pointers is refused (board 8597): a RUSTFLAGS in the environment
  replaces .cargo/config.toml's -Cforce-frame-pointers=yes, and /z/pprof's stacks went wrong with
  no error."""

import os
import shutil
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

from script import SCRIPTS

STAMP = SCRIPTS.parent / "crates" / "cladefs_core" / "build" / "stamp.rs"
FILES = {
    "Cargo.toml": '[workspace]\nresolver = "3"\nmembers = ["crates/*"]\n',
    "crates/dep/Cargo.toml": '[package]\nname = "dep"\nedition = "2024"\n',
    "crates/dep/src/lib.rs": "pub fn answer() -> u32 {\n    42\n}\n",
    "crates/stamped/Cargo.toml": '[package]\nname = "stamped"\nedition = "2024"\n\n'
                                 '[dependencies]\ndep = { path = "../dep" }\n',
    "crates/stamped/build.rs": f'#[path = "{STAMP}"]\nmod stamp;\n\n'
                               "fn main() -> Result<(), Box<dyn std::error::Error>> {\n"
                               "    stamp::require_frame_pointers()?;\n"
                               "    stamp::stamp_commit()\n}\n",
    "crates/stamped/src/main.rs": 'fn main() {\n    assert_eq!(dep::answer(), 42);\n'
                                  '    println!("{}", env!("CLADEFS_BUILD_COMMIT"));\n}\n',
}
# What a caller's environment may set that would steer the builds.
STEERING = ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS", "CARGO_TARGET_DIR",
            "CLADEFS_BUILD_COMMIT", "CARGO_BUILD_TARGET"]


class Stamp(unittest.TestCase):
    def setUp(self):
        self.directory = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.directory)
        self.repo = self.directory / "repo"
        for name, text in FILES.items():
            path = self.repo / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        (self.repo / "scripts").mkdir()
        shutil.copy(SCRIPTS / "build-commit.sh", self.repo / "scripts" / "build-commit.sh")
        self.env = {name: value for name, value in os.environ.items() if name not in STEERING}
        self.env["CARGO_TARGET_DIR"] = str(self.directory / "target")
        self.cargo("generate-lockfile", "--offline")
        for args in (["init", "-q"], ["add", "-A"],
                     ["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "tree"]):
            self.git(*args)
        self.commit = self.git("rev-parse", "HEAD")
        # The files older than the index, which then says them clean as it is: written in the same
        # second, git's status rewrote the index at the first build, whose script then ran again at
        # the next whatever it watched, as cargo takes a file changed during a run for stale.
        past = time.time() - 60
        for path in self.repo.rglob("*"):
            if ".git" not in path.relative_to(self.repo).parts:
                os.utime(path, (past, past))
        self.git("update-index", "--refresh", "-q")

    def git(self, *args):
        return subprocess.run(["git", "-C", str(self.repo), *args], check=True,
                              capture_output=True, text=True).stdout.strip()

    def cargo(self, *args, env=None):
        return subprocess.run(["cargo", *args], cwd=self.repo, env={**self.env, **(env or {})},
                              capture_output=True, text=True)

    def stamp(self):
        """What a build of the tree as it is now says it is of."""
        built = self.cargo("build", "--offline", "-q")
        self.assertEqual(built.returncode, 0, built.stderr)
        return subprocess.run([self.directory / "target" / "debug" / "stamped"], check=True,
                              capture_output=True, text=True).stdout.strip()

    def test_an_edit_to_a_dependency_makes_the_build_dirty_and_its_revert_clean(self):
        self.assertEqual(self.stamp(), self.commit)
        self.assertEqual(self.stamp(), self.commit)
        source = self.repo / "crates" / "dep" / "src" / "lib.rs"
        source.write_text(source.read_text().replace("42", "40 + 2"))
        self.assertEqual(self.stamp(), f"{self.commit}-dirty")
        self.git("checkout", "--", "crates/dep/src/lib.rs")
        self.assertEqual(self.stamp(), self.commit)

    def test_a_release_build_without_frame_pointers_is_refused(self):
        # The workspace has no .cargo/config.toml: rustflags are RUSTFLAGS's alone.
        refused = self.cargo("build", "--offline", "-q", "--release", env={"RUSTFLAGS": "--cfg x"})
        self.assertEqual(refused.returncode, 101, refused.stderr)
        self.assertIn("a release build without frame pointers, which perf and /z/pprof unwind by",
                      refused.stderr)
        for flags in ["-Cforce-frame-pointers=yes", "--cfg x -C force-frame-pointers=on",
                      "-Cforce-frame-pointers=no -Cforce-frame-pointers"]:
            with self.subTest(flags):
                built = self.cargo("build", "--offline", "-q", "--release",
                                   env={"RUSTFLAGS": flags})
                self.assertEqual(built.returncode, 0, built.stderr)
        # The last option says: frame pointers turned off after on are off.
        off = self.cargo("build", "--offline", "-q", "--release",
                         env={"RUSTFLAGS": "-Cforce-frame-pointers=yes -Cforce-frame-pointers=no"})
        self.assertEqual(off.returncode, 101, off.stderr)


if __name__ == "__main__":
    unittest.main()
