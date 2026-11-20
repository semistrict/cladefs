# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/read_prediction.py (TASK-702.1): traces read as the daemon writes them, a
boot's units of what it inherited, the prefetch model, and each predictor replayed on boots."""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from script import SCRIPTS

sys.path.insert(0, str(SCRIPTS))
import read_prediction as rp  # noqa: E402

MIB = rp.MIB
OWN = 9  # the boots' own cell
BASE = 7  # the cell they inherited from


def line(at_ms, extent, offset, length=4096, extent_len=4 * MIB, waited_ms=30, kind="fault",
         cell=BASE):
    """A trace's line of a read, as crates/cladefs/src/cladefsd/reads.rs writes it."""
    return (f"{at_ms * 1000} {kind} {cell} {extent} {extent_len:x} {offset:x} {length:x} "
            f"{waited_ms * 1000}")


def trace_text(*lines, cell=OWN):
    return "\n".join([rp.HEADER, f"cell {cell} opened 1700000000000", *lines]) + "\n"


def boot(name, *lines, horizon=10.0):
    return rp.boot_of(rp.parse(name, trace_text(*lines)), horizon)


class Traces(unittest.TestCase):
    """Traces as the daemon writes them."""

    def test_a_trace_reads_back(self):
        trace = rp.parse("01-sandbox-b1-query.reads", trace_text(
            "5000 fault 7 11 300000 1000 1000 120", "9000 read 1a 1 0 0 200 3"))
        self.assertEqual(trace.cell, OWN)
        self.assertEqual(trace.reads, [
            rp.Read(at=0.005, kind="fault", extent=(7, 33), extent_len=0x300000, offset=0x1000,
                    len=0x1000, waited=0.00012),
            rp.Read(at=0.009, kind="read", extent=(42, 1), extent_len=0, offset=0, len=0x200,
                    waited=0.000003),
        ])

    def test_what_is_not_a_trace_is_refused(self):
        with self.assertRaisesRegex(ValueError, "x.reads: not a read trace"):
            rp.parse("x.reads", "cladefs boot profile 2\n")

    def test_traces_load_in_their_names_order(self):
        with tempfile.TemporaryDirectory() as directory:
            for name in ["02-sandbox-a1-query.reads", "01-sandbox-c1-notebook.reads", "notes.md"]:
                (Path(directory) / name).write_text(trace_text(line(1, 1, 0)))
            self.assertEqual([trace.name for trace in rp.load(directory)],
                             ["01-sandbox-c1-notebook.reads", "02-sandbox-a1-query.reads"])


class Boots(unittest.TestCase):
    """A boot's units: MiBs of the extents it inherited, first reads within the horizon."""

    def test_a_boots_units_are_its_inherited_extents_mibs(self):
        got = boot("b",
                   line(1, 1, 0),
                   line(2, 1, 4096, waited_ms=0),  # the same unit, read again
                   line(3, 1, MIB - 512, length=1024),  # across two units
                   line(4, 2, 0, kind="read", cell=OWN),  # its own extent
                   line(11_000, 3, 0))  # past the horizon
        self.assertEqual(got.first, {((BASE, 1), 0): (0.001, 0.03), ((BASE, 1), 1): (0.003, 0.03)})
        self.assertEqual(got.read, {((BASE, 1), 0), ((BASE, 1), 1), ((BASE, 3), 0)})
        self.assertEqual(got.extents[(BASE, 1)], [((BASE, 1), part) for part in range(4)])


class Fetches(unittest.TestCase):
    """The prefetch model."""

    def test_fetches_run_so_many_at_once_each_so_long_first_queued_first(self):
        model = rp.Model(latency=0.03, concurrency=2)
        done = rp.finishes([(0.0, "a"), (0.0, "b"), (0.0, "c"), (0.1, "d"), (0.0, "a")], model)
        self.assertEqual(done, {"a": 0.03, "b": 0.03, "c": 0.06, "d": 0.13})


class Replays(unittest.TestCase):
    """Each predictor on boots: what it predicted, fetched for nothing, and left waiting."""

    model = rp.Model(latency=0.03, concurrency=64)

    def boots(self):
        # Two boots of one workload, then one of another: the first two read extent 1's first
        # two MiBs (of four), the third extent 2's first.
        return [
            boot("1", line(100, 1, 0), line(200, 1, MIB)),
            boot("2", line(100, 1, 0), line(200, 1, MIB)),
            boot("3", line(100, 2, 0)),
        ]

    def outcomes(self, predictor):
        return [(outcome.units, outcome.predicted, outcome.unused, round(outcome.waited, 3))
                for outcome in rp.replay(self.boots(), predictor, self.model)]

    def test_none_predicts_nothing_and_leaves_every_wait(self):
        self.assertEqual(self.outcomes(rp.Predictor()),
                         [(2, 0, 0, 0.06), (2, 0, 0, 0.06), (1, 0, 0, 0.03)])

    def test_extent_fetches_the_rest_of_an_extent_on_its_first_read(self):
        # Boot 1: at 0.1 s its other three MiBs are fetched, the second by 0.13 s, read at
        # 0.2 s; the last two never read.
        self.assertEqual(self.outcomes(rp.Extent()),
                         [(2, 1, 2, 0.03), (2, 1, 2, 0.03), (1, 0, 3, 0.03)])

    def test_first_keeps_the_first_boots_units(self):
        self.assertEqual(self.outcomes(rp.First()),
                         [(2, 0, 0, 0.06), (2, 2, 0, 0.0), (1, 0, 2, 0.03)])

    def test_recent_scores_units_by_the_share_of_recent_boots_that_read_them(self):
        # Before boot 3, of the last two, both read extent 1's: fetched, for nothing.
        self.assertEqual(self.outcomes(rp.Recent(keep=2, half_life=1.0, threshold=0.5)),
                         [(2, 0, 0, 0.06), (2, 2, 0, 0.0), (1, 0, 2, 0.03)])

    def test_recent_forgets_boots_past_those_it_keeps_and_weighs_old_ones_less(self):
        recent = rp.Recent(keep=2, half_life=1.0, threshold=0.5)
        for name, unit in [("a", 0), ("b", 1), ("c", 2)]:
            recent.ran(boot(name, line(100, 1, unit * MIB)))
        # Of b and c, c weighs 2/3 and b 1/3: c's alone scores half or more; a is forgotten.
        self.assertEqual(recent.ahead(), [((BASE, 1), 2)])

    def test_the_oracle_predicts_each_boot_whole(self):
        self.assertEqual(self.outcomes(rp.Oracle()),
                         [(2, 2, 0, 0.0), (2, 2, 0, 0.0), (1, 1, 0, 0.0)])

    def test_the_report_compares_every_predictor(self):
        results = rp.report(self.boots(), self.model, keep=8, half_life=2.0, threshold=0.25)
        self.assertEqual(list(results),
                         ["none", "extent", "first", "recent", "recent+extent", "oracle"])
        table = rp.text(results)
        self.assertIn("oracle                  1.00         0.0      0.00 (0.09)", table)


class Command(unittest.TestCase):
    """The command, on a directory of traces."""

    def test_it_prints_the_comparison(self):
        with tempfile.TemporaryDirectory() as directory:
            # Read 0.1 s after the opening, after a fetch queued at it lands.
            (Path(directory) / "01-a.reads").write_text(trace_text(line(100, 1, 0)))
            (Path(directory) / "02-b.reads").write_text(trace_text(line(100, 1, 0)))
            ran = subprocess.run([sys.executable, str(SCRIPTS / "read_prediction.py"), directory],
                                 capture_output=True, text=True, check=True)
        self.assertIn("first                   1.00         0.0      0.00 (0.03)", ran.stdout)

    def test_it_refuses_a_directory_of_no_traces(self):
        with tempfile.TemporaryDirectory() as directory:
            ran = subprocess.run([sys.executable, str(SCRIPTS / "read_prediction.py"), directory],
                                 capture_output=True, text=True)
        self.assertEqual(ran.returncode, 1)
        self.assertEqual(ran.stderr, f"no traces (*.reads) in {directory}\n")


if __name__ == "__main__":
    unittest.main()
