# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/loom-mutants.sh, which runs the loom models against the ordering mutants of
crates/*/loom-mutants (TASK-444.2): each mutant's fate from its model's exit, `# Fate: equivalent`
turning a pass into its expected fate, and --check refusing a patch that names no killer."""

import os
import shutil
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

from script import SCRIPTS

LINES = [f"line {i}" for i in range(1, 21)]

# A cargo whose loom model fails, as nextest exits for a failed test, if the file has a weakened
# line in it, and passes otherwise; saying what it ran.
FAKE_CARGO = textwrap.dedent("""\
    #!/bin/bash
    echo "cargo $*" >> "$FAKE_CARGO_RAN"
    if grep -q weak crates/c/src/f.txt; then
        echo "thread 'loom::m' panicked at crates/c/src/f.txt:11:1:"
        echo "    the weak line let the model fail"
        exit 100
    fi
    exit 0
""")


def mutant(name, change, header):
    """A patch of crates/c/src/f.txt changing line 11 to `change`, with `header`."""
    body = [" line 8", " line 9", " line 10", "-line 11", f"+{change}", " line 12", " line 13", " line 14"]
    return (
        name,
        header + "--- a/crates/c/src/f.txt\n+++ b/crates/c/src/f.txt\n@@ -8,7 +8,7 @@\n"
        + "".join(f"{line}\n" for line in body),
    )


KILLER = "# Killer: c loom::m\n"
EQUIVALENT = "# Fate: equivalent: stronger than needed\n"


class LoomMutants(unittest.TestCase):
    def setUp(self):
        self.tree = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tree)
        (self.tree / "scripts/lib").mkdir(parents=True)
        shutil.copy(SCRIPTS / "loom-mutants.sh", self.tree / "scripts")
        shutil.copy(SCRIPTS / "lib/patches.sh", self.tree / "scripts/lib")
        (self.tree / "crates/c/src").mkdir(parents=True)
        (self.tree / "crates/c/loom-mutants").mkdir()
        (self.tree / "crates/c/src/f.txt").write_text("".join(f"{line}\n" for line in LINES))
        bin_dir = self.tree / "bin"
        bin_dir.mkdir()
        (bin_dir / "cargo").write_text(FAKE_CARGO)
        (bin_dir / "cargo").chmod(0o755)
        self.ran = self.tree / "ran"
        self.env = dict(
            os.environ,
            PATH=f"{bin_dir}:{os.environ['PATH']}",
            FAKE_CARGO_RAN=str(self.ran),
            CLADEFS_MUTANTS_LOGS=str(self.tree / "logs"),
        )

    def add(self, *mutants):
        for name, patch in mutants:
            (self.tree / f"crates/c/loom-mutants/{name}.patch").write_text(patch)

    def run_script(self, *args):
        """The script's exit status and its table's rows, mutant and fate, time left out."""
        done = subprocess.run(
            [str(self.tree / "scripts/loom-mutants.sh"), *args],
            cwd=self.tree,
            env=self.env,
            capture_output=True,
            text=True,
        )
        rows = [line.split(None, 2) for line in done.stdout.splitlines()[2:]]
        return done.returncode, [(row[0], row[2]) for row in rows]

    def test_each_mutant_has_its_fate_and_the_tree_is_left_as_it_was(self):
        self.add(
            mutant("killed", "weak", KILLER),
            mutant("missed", "eleven", KILLER),
            mutant("equivalent", "eleven", EQUIVALENT + KILLER),
            mutant("wrongly-equivalent", "weak", EQUIVALENT + KILLER),
        )
        status, rows = self.run_script()
        self.assertEqual(
            rows,
            [
                ("equivalent", "survived, as equivalent"),
                ("killed", "killed: the weak line let the model fail"),
                ("missed", "MISSED"),
                ("wrongly-equivalent", "KILLED, though said equivalent"),
            ],
        )
        self.assertEqual(status, 1)
        self.assertEqual(
            (self.tree / "crates/c/src/f.txt").read_text(), "".join(f"{line}\n" for line in LINES)
        )
        self.assertEqual(
            self.ran.read_text().splitlines(),
            ["cargo nextest run -p c --features loom --lib --no-fail-fast -E test(=loom::m)"] * 4,
        )

    def test_a_run_whose_mutants_all_have_their_fates_passes(self):
        self.add(mutant("killed", "weak", KILLER), mutant("equivalent", "eleven", EQUIVALENT + KILLER))
        status, rows = self.run_script("killed", "equivalent")
        self.assertEqual(
            rows,
            [
                ("killed", "killed: the weak line let the model fail"),
                ("equivalent", "survived, as equivalent"),
            ],
        )
        self.assertEqual(status, 0)

    def test_check_refuses_a_mutant_naming_no_killer_and_runs_nothing(self):
        self.add(mutant("killed", "weak", KILLER), mutant("unnamed", "weak", "# Why: none\n"))
        done = subprocess.run(
            [str(self.tree / "scripts/loom-mutants.sh"), "--check"],
            cwd=self.tree,
            env=self.env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(
            (done.returncode, done.stdout, done.stderr),
            (1, "loom mutants: 2 checked, 1 broken\n", "crates/c/loom-mutants/unnamed.patch names no killer\n"),
        )
        self.assertFalse(self.ran.exists())


if __name__ == "__main__":
    unittest.main()
