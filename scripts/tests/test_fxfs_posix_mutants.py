# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of crates/fxfs-posix/mutants.sh, which runs the model test against the crate's mutants:
each mutant's fate from nextest's exit, and a run that couldn't test a mutant failing (TASK-642)."""

import os
import shutil
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

from script import SCRIPTS

LINES = [f"line {i}" for i in range(1, 21)]

# A cargo as nextest exits: 100 with the failed test for a file with a bad line, 0 without; and,
# for a file with a broken line, a build that fails, as cargo exits for one (101).
FAKE_CARGO = textwrap.dedent("""\
    #!/bin/bash
    if grep -q broken crates/fxfs-posix/src/f.txt; then
        echo "error: could not compile fxfs-posix"
        exit 101
    fi
    if grep -q bad crates/fxfs-posix/src/f.txt; then
        echo "        FAIL [   0.5s] fxfs-posix model::sequences"
        exit 100
    fi
    exit 0
""")


def mutant(name, change):
    """A patch of crates/fxfs-posix/src/f.txt changing line 11 to `change`."""
    body = [" line 8", " line 9", " line 10", "-line 11", f"+{change}", " line 12", " line 13",
            " line 14"]
    return name, ("--- a/crates/fxfs-posix/src/f.txt\n+++ b/crates/fxfs-posix/src/f.txt\n"
                  "@@ -8,7 +8,7 @@\n" + "".join(f"{line}\n" for line in body))


class FxfsPosixMutants(unittest.TestCase):
    def setUp(self):
        self.tree = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tree)
        crate = self.tree / "crates/fxfs-posix"
        (crate / "src").mkdir(parents=True)
        (crate / "mutants").mkdir()
        shutil.copy(SCRIPTS.parent / "crates/fxfs-posix/mutants.sh", crate)
        (self.tree / "scripts/lib").mkdir(parents=True)
        shutil.copy(SCRIPTS / "lib/patches.sh", self.tree / "scripts/lib")
        (crate / "src/f.txt").write_text("".join(f"{line}\n" for line in LINES))
        bin_dir = self.tree / "bin"
        bin_dir.mkdir()
        (bin_dir / "cargo").write_text(FAKE_CARGO)
        (bin_dir / "cargo").chmod(0o755)
        self.env = dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}")

    def run_script(self, *mutants):
        """Runs the script over `mutants`: its exit status and its table's rows, mutant and fate,
        time left out."""
        for name, patch in mutants:
            (self.tree / f"crates/fxfs-posix/mutants/{name}.patch").write_text(patch)
        done = subprocess.run([str(self.tree / "crates/fxfs-posix/mutants.sh")], cwd=self.tree,
                              env=self.env, capture_output=True, text=True)
        rows = [line.split(None, 2) for line in done.stdout.splitlines()[1:]]
        return done.returncode, [(row[0], row[2]) for row in rows]

    def test_mutants_found_or_not_pass_and_leave_the_tree_as_it_was(self):
        self.assertEqual(self.run_script(mutant("found", "bad"), mutant("missed", "eleven")),
                         (0, [("found", "model::sequences"),
                              ("missed", "not caught")]))
        self.assertEqual((self.tree / "crates/fxfs-posix/src/f.txt").read_text(),
                         "".join(f"{line}\n" for line in LINES))

    def test_a_mutant_whose_tests_could_not_run_fails_the_run(self):
        self.assertEqual(self.run_script(mutant("found", "bad"), mutant("unbuilt", "broken")),
                         (1, [("found", "model::sequences"),
                              ("unbuilt", "BROKEN: exit 101: error: could not compile fxfs-posix")]))


if __name__ == "__main__":
    unittest.main()
