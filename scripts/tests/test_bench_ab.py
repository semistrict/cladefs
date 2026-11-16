# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/bench-ab.py: python3 -m unittest discover scripts/tests (scripts/test.sh)."""

import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent.parent / "bench-ab.py"
spec = importlib.util.spec_from_file_location("bench_ab", SCRIPT)
bench_ab = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench_ab)


def results(**workloads):
    """A results directory whose one case has `workloads`, each (higher_is_better, values)."""
    directory = Path(tempfile.mkdtemp())
    entries = [
        {"workload": name, "unit": "ops/s", "higher_is_better": higher, "values": values}
        for name, (higher, values) in workloads.items()
    ]
    (directory / "bench-fs-x.json").write_text(json.dumps({"results": entries}))
    return directory


class Table(unittest.TestCase):
    def test_pools_rounds_and_says_how_much_slower_b_is(self):
        a = [results(create=(True, [100, 110])), results(create=(True, [90]))]
        b = [results(create=(True, [50, 55, 45]))]
        self.assertEqual(
            bench_ab.table("a", a, "b", b).splitlines()[2],
            "| create | ops/s | 100 (90–110) | 50 (45–55) | 2.00× | no |",
        )

    def test_times_lower_are_better_and_ranges_overlap(self):
        a = [results(init=(False, [4.0, 3.0]))]
        b = [results(init=(False, [3.4, 3.8]))]
        self.assertEqual(
            bench_ab.table("a", a, "b", b).splitlines()[2],
            "| init | ops/s | 3.5 (3–4) | 3.6 (3.4–3.8) | 1.03× | yes |",
        )


def cases(**backends):
    """A results directory with a case per backend, each {workload: (higher_is_better, values)}."""
    directory = Path(tempfile.mkdtemp())
    for backend, workloads in backends.items():
        entries = [
            {"workload": name, "unit": "IOPS", "higher_is_better": higher, "values": values}
            for name, (higher, values) in workloads.items()
        ]
        report = {"backend": backend, "results": entries}
        (directory / f"bench-fs-{backend}.json").write_text(json.dumps(report))
    return directory


class ControlTable(unittest.TestCase):
    """TASK-276: with a control, cases apart, and a difference real only past the control's drift."""

    def rows(self, a, b):
        return bench_ab.control_table("a", a, "b", b, "ext4").splitlines()[2:]

    def test_a_difference_past_the_controls_drift_is_real(self):
        a = [cases(ext4={"read": (True, [100, 104])}, fxfs={"read": (True, [50, 52])})]
        b = [cases(ext4={"read": (True, [98, 102])}, fxfs={"read": (True, [70, 74])})]
        self.assertEqual(
            self.rows(a, b),
            ["| fxfs | read | IOPS | 51 (50–52) | 72 (70–74) | 0.71× | 1.02× | real |"],
        )

    def test_a_difference_the_control_moves_as_much_is_within_drift(self):
        a = [cases(ext4={"read": (True, [100, 101])}, fxfs={"read": (True, [50, 51])})]
        b = [cases(ext4={"read": (True, [75, 76])}, fxfs={"read": (True, [40, 41])})]
        self.assertEqual(
            self.rows(a, b),
            ["| fxfs | read | IOPS | 50.5 (50–51) | 40.5 (40–41) | 1.25× | 1.33× | within drift |"],
        )

    def test_overlapping_ranges_are_within_drift_and_no_control_is_said(self):
        a = [cases(ext4={"read": (True, [100])}, fxfs={"read": (True, [50, 80]), "map": (False, [2])})]
        b = [cases(ext4={"read": (True, [100])}, fxfs={"read": (True, [60, 90]), "map": (False, [3])})]
        self.assertEqual(
            self.rows(a, b),
            [
                "| fxfs | map | IOPS | 2 (2–2) | 3 (3–3) | 1.50× | - | no control |",
                "| fxfs | read | IOPS | 65 (50–80) | 75 (60–90) | 0.87× | 1.00× | within drift |",
            ],
        )


LOADER = "others wrote 186.0 MB/s to its disk over the round, bound 10 (nvme0n1 186.0)"


def measured(verdict=None, why=(), case="fxfs", **workloads):
    """A round's results directory: one case's workloads, each (unit, higher_is_better, values),
    and, with a verdict, its record of others' I/O beside it (scripts/contamination.py)."""
    directory = Path(tempfile.mkdtemp(prefix="r"))
    entries = [{"workload": name, "unit": unit, "higher_is_better": higher, "values": values}
               for name, (unit, higher, values) in workloads.items()]
    (directory / f"bench-{case}.json").write_text(json.dumps({"backend": case, "results": entries}))
    if verdict:
        (directory / "contamination.json").write_text(
            json.dumps({"verdict": verdict, "why": list(why)}))
    return directory


def compared(a, b, *options):
    """bench-ab.py's status, output and errors comparing rounds `a`, as side pin, with `b`, main."""
    done = subprocess.run([sys.executable, str(SCRIPT), *options, "pin", *map(str, a), "--",
                           "main", *map(str, b)], capture_output=True, text=True)
    return done.returncode, done.stdout.splitlines(), done.stderr


class Contaminated(unittest.TestCase):
    """TASK-375: a round others wrote beside has its timings left out, said in the table, and its
    counts kept; with no clean round on a side there is no result, never a ratio."""

    def test_a_contaminated_rounds_timings_are_left_out_on_its_side_and_its_counts_kept(self):
        a = [measured("clean", create=("ops/s", True, [100, 110]), written=("x", False, [1.0])),
             measured("contaminated", [LOADER], create=("ops/s", True, [40]),
                      written=("x", False, [1.02]))]
        b = [measured("clean", create=("ops/s", True, [50, 55]), written=("x", False, [1.5])),
             measured("clean", create=("ops/s", True, [45]), written=("x", False, [1.5]))]
        self.assertEqual(bench_ab.table("pin", a, "main", b).splitlines()[2:], [
            "| create | ops/s | 105 (100–110) [contaminated rounds left out: 1] | 50 (45–55) | "
            "2.10× | no |",
            "| written | x | 1.01 (1–1.02) | 1.5 (1.5–1.5) | 1.49× | no |",
        ])
        self.assertEqual(bench_ab.moved(a, b), ["create", "written"])
        status, out, err = compared(a, b)
        self.assertEqual((status, err), (0, ""))
        self.assertEqual(out[:5], [
            "## CONTAMINATED: others wrote beside these rounds; their timings are left out, their "
            "counts and sizes kept", "", f"- {a[1].name}: {LOADER}", "",
            "| Workload | Unit | pin | main | main vs pin | Ranges overlap |"])

    def test_contaminated_rounds_on_both_sides_are_left_out_of_both(self):
        """Left in, pin's 100-110 and 40 would overlap main's 50-55 and 300: nothing moved."""
        a = [measured("clean", make=("s", False, [100, 110])),
             measured("contaminated", [LOADER], make=("s", False, [400]))]
        b = [measured("contaminated", [LOADER], make=("s", False, [300]), peak=("MiB", False, [9])),
             measured("clean", make=("s", False, [50, 55]), peak=("MiB", False, [7]))]
        self.assertEqual(bench_ab.table("pin", a, "main", b).splitlines()[2:], [
            "| make | s | 105 (100–110) [contaminated rounds left out: 1] | "
            "52.5 (50–55) [contaminated rounds left out: 1] | 0.50× | no |",
            "| peak | MiB | - | 8 (7–9) | - | - |",
        ])
        self.assertEqual(bench_ab.moved(a, b), ["make"])
        status, out, err = compared(a, b)
        self.assertEqual((status, err, out[2]), (0, "", f"- {a[1].name}, {b[0].name}: {LOADER}"))

    def test_no_clean_round_on_a_side_is_no_result_and_fails_the_comparison(self):
        """Never a ratio, nor "no change": pin's two rounds, both beside a loader, would say main
        1.00x of it."""
        a = [measured("contaminated", [LOADER], make=("s", False, [50]), written=("x", False, [2.0])),
             measured("contaminated", ["others wrote 70.0 MB/s to its disk in the minute 120 s in, "
                                       "bound 50"], make=("s", False, [55]),
                      written=("x", False, [2.0]))]
        b = [measured("clean", make=("s", False, [50, 55]), written=("x", False, [1.0]))]
        self.assertEqual(bench_ab.table("pin", a, "main", b).splitlines()[2:], [
            "| make | s | NO RESULT [contaminated rounds left out: 2] | 52.5 (50–55) | - | "
            "NO RESULT |",
            "| written | x | 2 (2–2) | 1 (1–1) | 0.50× | no |",
        ])
        self.assertEqual(bench_ab.moved(a, b), ["written"])
        status, out, err = compared(a, b)
        self.assertEqual((status, err), (1, "NO RESULT: pin has no clean round of: make\n"))
        self.assertEqual(out[:9], [
            "## NO RESULT: every round that measured these was contaminated", "",
            "- pin has no clean round of: make", "",
            "## CONTAMINATED: others wrote beside these rounds; their timings are left out, their "
            "counts and sizes kept", "", f"- {a[0].name}: {LOADER}",
            f"- {a[1].name}: others wrote 70.0 MB/s to its disk in the minute 120 s in, bound 50",
            ""])
        self.assertEqual(out[-1], "Moved, their ranges apart: written.")

    def test_no_clean_round_on_either_side(self):
        a = [measured("contaminated", [LOADER], make=("s", False, [50]))]
        b = [measured("contaminated", [LOADER], make=("s", False, [70]))]
        self.assertEqual(bench_ab.table("pin", a, "main", b).splitlines()[2:], [
            "| make | s | NO RESULT [contaminated rounds left out: 1] | "
            "NO RESULT [contaminated rounds left out: 1] | - | NO RESULT |"])
        status, out, err = compared(a, b)
        self.assertEqual((status, err), (1, "NO RESULT: pin has no clean round of: make; main has "
                                            "no clean round of: make\n"))
        self.assertEqual(out[2:4], ["- pin has no clean round of: make",
                                    "- main has no clean round of: make"])
        self.assertEqual(out[-1], "Moved, their ranges apart: none.")

    def test_with_a_control_the_rounds_control_is_left_out_with_it(self):
        def round_(verdict, fxfs, ext4):
            directory = measured(verdict, [LOADER] if verdict == "contaminated" else [],
                                 read=("IOPS", True, fxfs), stored=("GiB", False, [3]))
            other = measured(case="ext4", read=("IOPS", True, ext4))
            (other / "bench-ext4.json").rename(directory / "bench-ext4.json")
            return directory

        a = [round_("clean", [50, 52], [100, 104]), round_("contaminated", [20], [30])]
        b = [round_("clean", [70, 74], [98, 102])]
        self.assertEqual(bench_ab.control_table("pin", a, "main", b, "ext4").splitlines()[2:], [
            "| fxfs | read | IOPS | 51 (50–52) [contaminated rounds left out: 1] | 72 (70–74) | "
            "0.71× | 1.02× | real |",
            "| fxfs | stored | GiB | 3 (3–3) | 3 (3–3) | 1.00× | - | no control |",
        ])
        a = [round_("contaminated", [20], [30])]
        self.assertEqual(bench_ab.control_table("pin", a, "main", b, "ext4").splitlines()[2:], [
            "| fxfs | read | IOPS | NO RESULT [contaminated rounds left out: 1] | 72 (70–74) | - "
            "| - | NO RESULT |",
            "| fxfs | stored | GiB | 3 (3–3) | 3 (3–3) | 1.00× | - | no control |",
        ])
        status, _, err = compared(a, b, "--control", "ext4")
        self.assertEqual((status, err), (1, "NO RESULT: pin has no clean round of: ext4 read, "
                                            "fxfs read\n"))

    def test_rounds_not_checked_are_said_and_their_timings_kept(self):
        """A round with no record (one of before it, or run by hand), and one on a host where
        others' I/O can't be told from its own, are not clean: the comparison says which."""
        cgroup = "cgroup v1: no cgroup's io.stat is the round's own"
        a = [measured(make=("s", False, [100])), measured("unchecked", [cgroup], make=("s", False, [110]))]
        b = [measured(make=("s", False, [50])), measured("clean", make=("s", False, [55]))]
        status, out, err = compared(a, b)
        self.assertEqual((status, err), (0, ""))
        self.assertEqual(out[:7], [
            "## NOT CHECKED: whether others wrote beside these rounds is not known; their timings "
            "are kept", "",
            f"- {a[0].name}, {b[0].name}: no record of others' I/O beside it (contamination.json)",
            f"- {a[1].name}: {cgroup}", "",
            "| Workload | Unit | pin | main | main vs pin | Ranges overlap |", "|---|---|---|---|---|---|"])
        self.assertEqual(out[7], "| make | s | 105 (100–110) | 52.5 (50–55) | 0.50× | no |")

    def test_every_round_clean_says_nothing_of_it(self):
        a = [measured("clean", make=("s", False, [100]))]
        b = [measured("clean", make=("s", False, [50]))]
        self.assertEqual(compared(a, b), (0, [
            "| Workload | Unit | pin | main | main vs pin | Ranges overlap |",
            "|---|---|---|---|---|---|", "| make | s | 100 (100–100) | 50 (50–50) | 0.50× | no |",
            "", "Moved, their ranges apart: make."], ""))


if __name__ == "__main__":
    unittest.main()
