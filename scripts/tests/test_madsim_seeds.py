# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/madsim-seeds.py (board 2912): every seed runs; failures madsim-known lists pass
the run, said as known, and any other fails it; against fake_nextest.py's runs."""

import io
import json
import os
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

from fake_nextest import report
from script import load

seeds = load("madsim-seeds")
FAKE_NEXTEST = Path(__file__).resolve().with_name("fake_nextest.py")

KNOWN = r"""
# BOARD TEST PANIC
5721 ephemeral_cells_survive_disk_faults   round \d+: handing over: Errno\(ENOENT\)
5721 ephemeral_cells_survive_disk_faults   the cell noted on hosts \[\d+(, \d+)+\], held on \d+
5469 ephemeral_cells_survive_crashes_and_migrations  called `Result::unwrap\(\)` on an `Err` value: Errno\(EBUSY\)
"""


def panic(message, seed=6):
    return (f"thread '<unnamed>' (10924854) panicked at crates/cladefs/tests/madsim.rs:710:9:\n"
            f"{message}\nnote: run with `RUST_BACKTRACE=1` environment variable to display a "
            f'backtrace\ncontext: node=0 "madsim-main", task=0\n'
            f"note: run with `MADSIM_TEST_SEED={seed}` environment variable to reproduce this error")


class ParseTest(unittest.TestCase):
    def test_regression_seeds_are_each_lines_first_word(self):
        text = "# SEED what\n#\n3       TASK-699: a probe\n\n71      board 2934: # a marker\n"
        self.assertEqual(seeds.regression_seeds(text), [3, 71])

    def test_known_failures_are_a_board_a_test_and_a_panic(self):
        listed = seeds.known(KNOWN)
        self.assertEqual([(k.board, k.test) for k in listed], [
            (5721, "ephemeral_cells_survive_disk_faults"),
            (5721, "ephemeral_cells_survive_disk_faults"),
            (5469, "ephemeral_cells_survive_crashes_and_migrations"),
        ])
        self.assertEqual(listed[2].panic.pattern,
                         r"called `Result::unwrap\(\)` on an `Err` value: Errno\(EBUSY\)")

    def test_a_line_without_a_board_first_is_refused(self):
        with self.assertRaisesRegex(ValueError, r"madsim-known:2: not BOARD TEST PANIC"):
            seeds.known("# x\nephemeral_cells_survive_disk_faults host \\d+ never answered\n")

    def test_the_repositorys_list_parses_and_names_the_simulations_tests(self):
        """Empty once every bug it lists is fixed; each line it has, a test of tests/madsim.rs."""
        simulation = (seeds.ROOT / "crates" / "cladefs" / "tests" / "madsim.rs").read_text()
        for listed in seeds.known(seeds.KNOWN.read_text()):
            self.assertIn(f"fn {listed.test}(", simulation)

    def test_a_panics_message_is_what_it_says_up_to_its_note(self):
        self.assertEqual(seeds.panic_of(" WARN x\n" + panic("the cell noted on hosts [0, 2]")),
                         "the cell noted on hosts [0, 2]")
        self.assertEqual(seeds.panic_of("\x1b[31;1mthread 'a' panicked at x.rs:1:2:\x1b[0m\n"
                                        "\x1b[31;1mtwo\nlines\x1b[0m\nnote: run"), "two\nlines")
        self.assertIsNone(seeds.panic_of("test timed out"))


class FailuresTest(unittest.TestCase):
    def failures(self, *cases):
        return seeds.failures(report(cases), 6, seeds.known(KNOWN))

    def test_a_listed_panic_of_its_test_is_known(self):
        [failure] = self.failures(
            ("cladefs::madsim", "uploaded_commits_survive_attrition", None),
            ("cladefs::madsim", "ephemeral_cells_survive_disk_faults",
             panic("the cell noted on hosts [0, 2], held on 2")))
        self.assertEqual(failure.known.board, 5721)
        self.assertEqual(str(failure), "seed 6: cladefs::madsim ephemeral_cells_survive_disk_faults:"
                                       " the cell noted on hosts [0, 2], held on 2: known, board 5721")

    def test_a_listed_panic_matches_however_many_hosts_it_names(self):
        [failure] = self.failures(("cladefs::madsim", "ephemeral_cells_survive_disk_faults",
                                   panic("the cell noted on hosts [0, 1, 2], held on 1")))
        self.assertEqual(failure.known.board, 5721)

    def test_another_panic_of_a_listed_test_is_new(self):
        [failure] = self.failures(("cladefs::madsim", "ephemeral_cells_survive_disk_faults",
                                   panic("blocks told committed, lost or other: [3] of [1, 2, 3]")))
        self.assertIsNone(failure.known)
        self.assertEqual(str(failure), "seed 6: cladefs::madsim ephemeral_cells_survive_disk_faults:"
                                       " blocks told committed, lost or other: [3] of [1, 2, 3]:"
                                       " MADSIM_TEST_SEED=6 replays it")

    def test_a_listed_panic_of_another_test_is_new(self):
        [failure] = self.failures(("cladefs::madsim", "ephemeral_cells_survive_crashes_and_migrations",
                                   panic("the cell noted on hosts [0, 1], held on 1")))
        self.assertIsNone(failure.known)

    def test_a_listed_panic_of_another_binary_is_new(self):
        [failure] = self.failures(("local_cell::madsim", "ephemeral_cells_survive_disk_faults",
                                   panic("the cell noted on hosts [0, 1], held on 1")))
        self.assertIsNone(failure.known)

    def test_a_failure_without_a_panic_is_new_said_as_nextest_says_it(self):
        [failure] = self.failures(("cladefs::madsim", "ephemeral_cells_survive_disk_faults",
                                   "test timed out after 300s"))
        self.assertIsNone(failure.known)
        self.assertEqual(failure.message, "test timed out after 300s")


class RunTest(unittest.TestCase):
    """main over fake_nextest.py's runs: each seed's outcome as $FAKE_NEXTEST/runs.json says."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.dir = Path(temporary.name)
        bin_dir = self.dir / "bin"
        bin_dir.mkdir()
        cargo = bin_dir / "cargo"
        cargo.write_text(f"#!/bin/sh\nexec python3 {FAKE_NEXTEST} \"$@\"\n")
        cargo.chmod(0o755)
        (self.dir / "known").write_text(KNOWN)
        (self.dir / "seeds").write_text("# SEED\n71 board 2934\n")
        self.junit = self.dir / "junit.xml"
        env = {"PATH": f"{bin_dir}:{os.environ['PATH']}", "FAKE_NEXTEST": str(self.dir),
               "FAKE_NEXTEST_JUNIT": str(self.junit), "GITHUB_ACTIONS": "true"}
        for patch in (mock.patch.dict(os.environ, env),
                      mock.patch.object(seeds, "JUNIT", self.junit),
                      mock.patch.object(seeds, "KNOWN", self.dir / "known"),
                      mock.patch.object(seeds, "REGRESSIONS", self.dir / "seeds")):
            patch.start()
            self.addCleanup(patch.stop)

    def run_seeds(self, runs, *args):
        """main, each seed's run as `runs` says ({seed: [(binary, test, text)], or a status}):
        its status, what it printed, and the seeds nextest ran."""
        (self.dir / "runs.json").write_text(json.dumps(runs))
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            try:
                status = seeds.main(["--first", "1", "--count", "3", *args])
            except SystemExit as exit:
                status = exit.code
        calls = [json.loads(line) for line in (self.dir / "calls").read_text().splitlines()]
        return status, out.getvalue() + err.getvalue(), calls

    def test_every_seed_runs_and_only_known_failures_pass(self):
        known = ("cladefs::madsim", "ephemeral_cells_survive_disk_faults",
                 panic("round 3: handing over: Errno(ENOENT)", 71))
        status, said, calls = self.run_seeds({"71": [known], "2": [known]}, "--no-fail-fast")
        self.assertEqual(status, 0, said)
        self.assertEqual([call["seed"] for call in calls], ["71", "1", "2", "3"])
        self.assertIn("--no-fail-fast", calls[0]["args"])
        self.assertEqual(calls[0]["args"][calls[0]["args"].index("--profile") + 1], "madsim")
        self.assertIn("madsim: 4 seeds, 0 new failures, 2 known\n"
                      "madsim: known, board 5721: seeds 2, 71\n", said)
        self.assertIn("::warning title=madsim: known, board 5721::seed 71: cladefs::madsim "
                      "ephemeral_cells_survive_disk_faults: round 3: handing over: "
                      "Errno(ENOENT): known, board 5721\n", said)

    def test_a_new_failure_fails_the_run_after_every_seed(self):
        new = ("cladefs::madsim", "uploaded_commits_survive_attrition",
               panic("blocks told uploaded, lost or other: [7] of [7]", 2))
        status, said, calls = self.run_seeds({"2": [new]})
        self.assertEqual(status, 1)
        self.assertEqual([call["seed"] for call in calls], ["71", "1", "2", "3"])
        self.assertIn("madsim: new: seed 2: cladefs::madsim uploaded_commits_survive_attrition: "
                      "blocks told uploaded, lost or other: [7] of [7]: "
                      "MADSIM_TEST_SEED=2 replays it\n", said)
        self.assertIn("::error title=madsim: a new failure::seed 2:", said)

    def test_the_first_fresh_seed_is_checked_to_replay(self):
        """Board 7583: the first fresh seed runs with MADSIM_TEST_CHECK_DETERMINISM, and its
        failure says how to replay it so."""
        diverged = ("cladefs::madsim", "uploaded_commits_survive_attrition",
                    panic("non-determinism detected at 0x2a", 1))
        status, said, calls = self.run_seeds({"1": [diverged]})
        self.assertEqual(status, 1)
        self.assertEqual([(call["seed"], call["checked"]) for call in calls],
                         [("71", False), ("1", True), ("2", False), ("3", False)])
        self.assertIn("madsim: new: seed 1: cladefs::madsim uploaded_commits_survive_attrition: "
                      "non-determinism detected at 0x2a: MADSIM_TEST_SEED=1 "
                      "MADSIM_TEST_CHECK_DETERMINISM=1 replays it\n", said)

    def test_a_run_failing_but_for_its_tests_ends_it(self):
        status, said, calls = self.run_seeds({"1": 101})
        self.assertEqual(status, "madsim: seed 1: cargo nextest failed (101), not a test")
        self.assertEqual([call["seed"] for call in calls], ["71", "1"])


if __name__ == "__main__":
    unittest.main()
