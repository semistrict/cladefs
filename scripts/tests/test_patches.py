# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/lib/patches.sh: patch_well_formed, the mutant scripts' check that a patch
applies with no fuzz as GNU patch takes it, which BSD patch (macOS) does not check (TASK-395); and
patch_run, which has a mutant in the tree while its killer runs and takes it out whatever stops the
runner, and the runners' use of it (TASK-444.2.5)."""

import os
import re
import shutil
import signal
import subprocess
import tempfile
import textwrap
import time
import unittest
from pathlib import Path

from script import SCRIPTS

ROOT = SCRIPTS.parent

LINES = [f"line {i}" for i in range(1, 21)]


def hunk(header, body):
    """A patch of one hunk to f.txt."""
    return "# Kind: test\n--- a/f.txt\n+++ b/f.txt\n" + header + "\n" + "".join(f"{line}\n" for line in body)


class WellFormed(unittest.TestCase):
    def setUp(self):
        self.directory = Path(tempfile.mkdtemp())
        (self.directory / "f.txt").write_text("".join(f"{line}\n" for line in LINES))

    def check(self, patch):
        """patch_well_formed over `patch`, from the directory f.txt is in: its status and errors."""
        (self.directory / "m.patch").write_text(patch)
        done = subprocess.run(
            ["/bin/bash", "-c", f'source "{SCRIPTS}/lib/patches.sh"; patch_well_formed m.patch'],
            cwd=self.directory,
            capture_output=True,
            text=True,
        )
        return done.returncode, done.stderr

    def test_a_hunk_with_even_context_is_well_formed(self):
        body = [" line 8", " line 9", " line 10", "-line 11", "+eleven", " line 12", " line 13", " line 14"]
        self.assertEqual(self.check(hunk("@@ -8,7 +8,7 @@", body)), (0, ""))

    def test_a_hunk_with_more_lines_than_its_header_counts_is_refused(self):
        # The header counts 7 of the 8 lines it has, as a patch re-anchored by hand did: BSD patch
        # applied it, GNU patch didn't.
        body = [" line 7", " line 8", " line 9", " line 10", "-line 11", "+eleven", " line 12", " line 13", " line 14"]
        self.assertEqual(
            self.check(hunk("@@ -7,7 +7,7 @@", body)),
            (
                1,
                "m.patch: the hunk at line 4 has 4 lines of context before its changes and 2 after, "
                "away from the start and end of f.txt\n"
                "m.patch: line 13 is past its hunk, as its header counts it\n",
            ),
        )

    def test_a_hunk_with_fewer_lines_than_its_header_counts_is_refused(self):
        body = [" line 9", " line 10", "-line 11", "+eleven", " line 12", " line 13"]
        self.assertEqual(
            self.check(hunk("@@ -9,6 +9,6 @@", body)),
            (1, "m.patch: the hunk at line 4 has fewer lines than its header counts\n"),
        )

    def test_uneven_context_away_from_the_files_ends_is_refused(self):
        body = [" line 7", " line 8", " line 9", " line 10", "-line 11", "+eleven", " line 12", " line 13", " line 14"]
        self.assertEqual(
            self.check(hunk("@@ -7,7 +7,7 @@", body[:-1])),
            (
                1,
                "m.patch: the hunk at line 4 has 4 lines of context before its changes and 2 after, "
                "away from the start and end of f.txt\n",
            ),
        )

    def test_uneven_context_at_the_files_start_or_end_is_well_formed(self):
        start = ["-line 1", "+one", " line 2", " line 3", " line 4"]
        self.assertEqual(self.check(hunk("@@ -1,4 +1,4 @@", start)), (0, ""))
        end = [" line 17", " line 18", " line 19", "-line 20", "+twenty"]
        self.assertEqual(self.check(hunk("@@ -17,4 +17,4 @@", end)), (0, ""))


# A killer that says it runs, its pid in `pid`, and runs till it is stopped.
BLOCKER = textwrap.dedent("""\
    #!/bin/bash
    echo $$ > "$BLOCKER_DIR/pid"
    exec sleep 600
""")

MUTANT = hunk(
    "@@ -8,7 +8,7 @@", [" line 8", " line 9", " line 10", "-line 11", "+weak", " line 12", " line 13", " line 14"]
)
ORIGINAL = "".join(f"{line}\n" for line in LINES)
MUTATED = ORIGINAL.replace("line 11\n", "weak\n")


class Stoppable:
    """A runner started in `self.tree` and stopped while its killer, BLOCKER, runs."""

    def start_blocker(self, directory):
        self.blocker_dir = directory
        (directory / "bin").mkdir(exist_ok=True)
        for name in ["blocker", "cargo"]:
            (directory / "bin" / name).write_text(BLOCKER)
            (directory / "bin" / name).chmod(0o755)
        self.env = dict(
            os.environ, PATH=f"{directory / 'bin'}:{os.environ['PATH']}", BLOCKER_DIR=str(directory)
        )
        self.addCleanup(self.kill_blocker)

    def kill_blocker(self):
        """The blocker stopped, if what it was in didn't stop it: whether it was left running."""
        try:
            os.kill(self.blocker_pid(), signal.SIGKILL)
        except (ProcessLookupError, FileNotFoundError, ValueError):
            return False
        return True

    def blocker_pid(self):
        return int((self.blocker_dir / "pid").read_text())

    def start(self, argv, cwd):
        """`argv` started in `cwd`, once its killer runs."""
        runner = subprocess.Popen(
            argv, cwd=cwd, env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
        )
        self.addCleanup(lambda: runner.poll() is None and runner.kill())
        deadline = time.monotonic() + 30
        while not (self.blocker_dir / "pid").exists():
            if runner.poll() is not None:
                self.fail(f"it ended before its killer ran: {runner.returncode}, {runner.communicate()}")
            self.assertLess(time.monotonic(), deadline, "the killer never ran")
            time.sleep(0.05)
        return runner

    def stop(self, runner, sig):
        """`runner` sent `sig`: its status and stderr, and whether its killer was left running."""
        runner.send_signal(sig)
        _, err = runner.communicate(timeout=30)
        time.sleep(0.2)
        return runner.returncode, err, self.kill_blocker()


class PatchRun(Stoppable, unittest.TestCase):
    def setUp(self):
        self.tree = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tree)
        (self.tree / "f.txt").write_text(ORIGINAL)
        (self.tree / "m.patch").write_text(MUTANT)
        self.start_blocker(self.tree)

    def bash(self, script):
        """`script` in bash, with patches.sh sourced."""
        return ["/bin/bash", "-c", f'source "{SCRIPTS}/lib/patches.sh"; {script}']

    def run_bash(self, script):
        return subprocess.run(self.bash(script), cwd=self.tree, capture_output=True, text=True)

    def test_it_runs_its_command_with_the_mutant_in_and_returns_its_status(self):
        done = self.run_bash(
            'patch_run m.patch log bash -c "cat f.txt; ls .mutant-in; exit 7"; echo "status $?"'
        )
        self.assertEqual((done.stdout, done.stderr), ("status 7\n", ""))
        self.assertEqual((self.tree / "log").read_text(), MUTATED + ".mutant-in\n")
        self.assertEqual((self.tree / "f.txt").read_text(), ORIGINAL)
        self.assertFalse((self.tree / ".mutant-in").exists())

    def test_a_sigterm_while_its_command_runs_stops_it_and_takes_the_mutant_out(self):
        runner = self.start(self.bash("patch_run m.patch log blocker; echo ran on"), self.tree)
        self.assertEqual((self.tree / "f.txt").read_text(), MUTATED)
        self.assertEqual(self.stop(runner, signal.SIGTERM), (143, "", False))
        self.assertEqual((self.tree / "f.txt").read_text(), ORIGINAL)
        self.assertFalse((self.tree / ".mutant-in").exists())

    def test_a_sigint_does_the_same(self):
        runner = self.start(self.bash("patch_run m.patch log blocker"), self.tree)
        self.assertEqual(self.stop(runner, signal.SIGINT), (130, "", False))
        self.assertEqual((self.tree / "f.txt").read_text(), ORIGINAL)

    def test_a_mutant_a_sigkill_left_in_is_taken_out_by_the_next_run(self):
        runner = self.start(self.bash("patch_run m.patch log blocker"), self.tree)
        status, _, left_running = self.stop(runner, signal.SIGKILL)
        self.assertEqual((status, left_running), (-signal.SIGKILL, True))
        self.assertEqual((self.tree / "f.txt").read_text(), MUTATED)
        self.assertEqual((self.tree / ".mutant-in").read_text(), f"{runner.pid} m.patch\n")
        done = self.run_bash("patch_run m.patch log cat f.txt")
        self.assertEqual(
            (done.returncode, done.stderr),
            (0, f"took out m.patch, which runner {runner.pid} left in the tree when it was stopped\n"),
        )
        self.assertEqual((self.tree / "log").read_text(), MUTATED)
        self.assertEqual((self.tree / "f.txt").read_text(), ORIGINAL)
        self.assertFalse((self.tree / ".mutant-in").exists())

    def test_a_tree_a_live_runner_holds_is_refused(self):
        (self.tree / ".mutant-in").write_text(f"{os.getpid()} other.patch\n")
        done = self.run_bash("patch_run m.patch log cat f.txt")
        self.assertEqual(
            (done.returncode, done.stderr),
            (
                1,
                f"runner {os.getpid()} has other.patch in this tree; "
                "if it is gone, .mutant-in is stale: remove it\n",
            ),
        )
        self.assertEqual((self.tree / "f.txt").read_text(), ORIGINAL)
        self.assertFalse((self.tree / "log").exists())


# The runners that put a mutant into their tree, and where their mutants are.
RUNNERS = {
    "scripts/loom-mutants.sh": "crates/c/loom-mutants",
    "crates/cladefs/mutants.sh": "crates/cladefs/mutants",
    "crates/extent_bucket/mutants.sh": "crates/extent_bucket/mutants",
    "crates/fxfs-posix/mutants.sh": "crates/fxfs-posix/mutants",
}


class Runners(Stoppable, unittest.TestCase):
    def test_every_runner_puts_a_mutant_into_its_tree_only_through_patch_run(self):
        # patch with -p1 and no -d: into the tree it runs in, not a copy.
        applies = re.compile(r"\bpatch\b[^|;&]*-p1(?![^|;&]*-d )")
        scripts = [*ROOT.glob("crates/*/*.sh"), *ROOT.glob("scripts/*.sh"), *ROOT.glob("guest/**/*.sh")]
        found = {
            f"{script.relative_to(ROOT)}:{number}": line.strip()
            for script in scripts
            for number, line in enumerate(script.read_text().splitlines(), 1)
            if applies.search(line) and not line.lstrip().startswith("#")
        }
        self.assertEqual(found, {})
        self.assertEqual([r for r in RUNNERS if "patch_run " not in (ROOT / r).read_text()], [])

    def test_a_runner_stopped_while_a_killer_runs_leaves_its_tree_as_it_was(self):
        for runner, mutants in RUNNERS.items():
            with self.subTest(runner):
                tree = Path(tempfile.mkdtemp())
                self.addCleanup(shutil.rmtree, tree)
                (tree / "scripts/lib").mkdir(parents=True)
                (tree / runner).parent.mkdir(parents=True, exist_ok=True)
                shutil.copy(ROOT / runner, tree / runner)
                shutil.copy(SCRIPTS / "lib/patches.sh", tree / "scripts/lib")
                (tree / mutants).mkdir(parents=True)
                source = tree / "crates/c/src/f.txt"
                source.parent.mkdir(parents=True, exist_ok=True)
                source.write_text(ORIGINAL)
                patch = MUTANT.replace("# Kind: test\n", "").replace("/f.txt", "/crates/c/src/f.txt")
                (tree / mutants / "weak.patch").write_text("# Killer: c loom::m\n" + patch)
                blockers = Path(tempfile.mkdtemp())
                self.addCleanup(shutil.rmtree, blockers)
                self.start_blocker(blockers)
                self.env["CLADEFS_MUTANTS_LOGS"] = str(tree / "logs")
                started = self.start([str(tree / runner)], tree)
                self.assertEqual(source.read_text(), MUTATED)
                status, _, left_running = self.stop(started, signal.SIGTERM)
                self.assertEqual((status, left_running), (143, False))
                self.assertEqual(source.read_text(), ORIGINAL)
                self.assertFalse((tree / ".mutant-in").exists())


if __name__ == "__main__":
    unittest.main()
