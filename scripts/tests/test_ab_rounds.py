# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/ab-rounds.py, the round-by-round comparison the nightly judges by (TASK-441.1),
on A/B results shaped as ubu1's (docs/measurements/ubu1-loop-baseline-2026-11-11's bench JSON,
contamination.py's records, scripts/counts.sh's outputs as the macOS runs of 2026-11-15 have them)."""

import json
import tempfile
import unittest
from pathlib import Path

from script import load

ab_rounds = load("ab-rounds")

# A workload of each kind the nightly measures, as the fs bench reports it: (unit, higher is
# better).
KINDS = {
    "fsync-append": ("IOPS", True),
    "create": ("ops/s", True),
    "stat-cold": ("ops/s", True),
    "seq-read-cold": ("MiB/s", True),
    "sync-loop": ("ops/s", True),
    "fork-read-usr": ("s", False),
}
# The control's workloads, steady unless a test moves them.
CONTROL = {"fsync-append": [13840.0, 13900.0], "create": [53430.0, 53500.0],
           "stat-cold": [179400.0, 179600.0], "seq-read-cold": [3938.0, 3950.0]}


def write_round(results, name, number, cases, verdict="clean", observations=None):
    """Side `name`'s round `number` in `results`: a bench-fs-BACKEND.json of each case, as the fs
    bench writes them, {backend: {workload: values}}, its observations {backend: {name: values}},
    and contamination.py's record, `verdict`."""
    directory = Path(results) / f"{name}-r{number}"
    directory.mkdir(parents=True)
    for backend, workloads in cases.items():
        report = {
            "case": f"bench-fs-{backend}",
            "backend": backend,
            "host_os": "linux",
            "vmm": "firecracker",
            "iterations": 2,
            "results": [{"workload": workload, "unit": KINDS[workload][0],
                         "higher_is_better": KINDS[workload][1], "values": values}
                        for workload, values in workloads.items()],
            "failures": {},
            "observations": {"mtime-ahead-of-guest-ms": [-1.637, -1.2],
                             **(observations or {}).get(backend, {})},
        }
        (directory / f"bench-fs-{backend}.json").write_text(json.dumps(report))
    why = [] if verdict == "clean" else ["others wrote 42.0 MB/s to its disk over the round"]
    (directory / "contamination.json").write_text(json.dumps({"verdict": verdict, "why": why}))
    return directory


def scaled(workloads, factor):
    return {workload: [value * factor for value in values] for workload, values in workloads.items()}


class Timings(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.results = Path(scratch.name)

    def night(self, head_factors, control_factors=None, verdicts=None, reruns=None):
        """Three rounds: the base's fxfs-guest-dax fsync-append 6,077 IOPS, the head's that times
        each round's factor; the control's the base's times its factor; each round's verdict."""
        control_factors = control_factors or {}
        verdicts = verdicts or {}
        for number, factor in head_factors.items():
            base = {"fsync-append": [6077.151, 6100.0]}
            write_round(self.results, "base", number,
                        {"ext4-guest-kernel": CONTROL, "fxfs-guest-dax": base})
            write_round(self.results, "head", number,
                        {"ext4-guest-kernel": scaled(CONTROL, control_factors.get(number, 1.0)),
                         "fxfs-guest-dax": scaled(base, factor)},
                        verdict=verdicts.get(number, "clean"))
        if reruns:
            (self.results / "reruns.txt").write_text(
                "".join(f"{rerun} {original}\n" for rerun, original in reruns.items()))

    def row(self, comparison, case="fxfs-guest-dax", name="fsync-append"):
        return next(row for row in comparison["metrics"]
                    if (row["case"], row["name"]) == (case, name))

    def test_slower_past_the_bound_in_every_round_moved(self):
        self.night({1: 0.8, 2: 0.85, 3: 0.75})
        row = self.row(ab_rounds.compare(self.results, "base", "head"))
        self.assertEqual(row["verdict"], "slower")
        self.assertEqual([(each["round"], round(each["ratio"], 4)) for each in row["rounds"]],
                         [(1, 1.25), (2, 1.1765), (3, 1.3333)])
        self.assertEqual(row["lost"], [])

    def test_faster_in_every_round_moved_too(self):
        self.night({1: 1.3, 2: 1.2, 3: 1.25})
        self.assertEqual(self.row(ab_rounds.compare(self.results, "base", "head"))["verdict"],
                         "faster")

    def test_slower_in_some_rounds_only_is_steady(self):
        self.night({1: 0.8, 2: 0.97, 3: 0.75})
        row = self.row(ab_rounds.compare(self.results, "base", "head"))
        self.assertEqual((row["verdict"], [each["way"] for each in row["rounds"]]),
                         ("steady", [1, 0, 1]))

    def test_the_bound_is_strict(self):
        """1.10 times slower is within a bound of 10%; past it is 1.11."""
        self.night({1: 1 / 1.1, 2: 1 / 1.11, 3: 1 / 1.11})
        row = self.row(ab_rounds.compare(self.results, "base", "head"))
        self.assertEqual((row["verdict"], [each["way"] for each in row["rounds"]]),
                         ("steady", [0, 1, 1]))

    def test_a_round_whose_control_moved_is_left_out_and_its_rerun_stands_for_it(self):
        self.night({1: 0.8, 2: 1.0, 3: 0.8, 4: 0.8}, control_factors={2: 0.9},
                   reruns={4: 2})
        comparison = ab_rounds.compare(self.results, "base", "head")
        self.assertEqual(comparison["rounds"]["2"],
                         {"kept": False, "rerun_of": None,
                          "why": "the control (ext4-guest-kernel) moved +11.1%, past 5%"})
        self.assertEqual(comparison["rounds"]["4"], {"kept": True, "rerun_of": "2", "why": None})
        row = self.row(comparison)
        self.assertEqual((row["verdict"], [(each["slot"], each["round"]) for each in row["rounds"]]),
                         ("slower", [(1, 1), (2, 4), (3, 3)]))

    def test_a_control_within_its_bound_keeps_the_round(self):
        self.night({1: 0.8, 2: 0.8, 3: 0.8}, control_factors={2: 0.96})
        comparison = ab_rounds.compare(self.results, "base", "head")
        self.assertEqual(comparison["rounds"]["2"], {"kept": True, "rerun_of": None, "why": None})
        self.assertEqual(self.row(comparison)["verdict"], "slower")

    def test_one_control_workload_past_its_bound_does_not_move_the_control(self):
        """The control moved is its timings' median ratio: one noisy workload of four is not the
        host."""
        self.night({1: 0.8, 2: 0.8, 3: 0.8})
        head = self.results / "head-r2" / "bench-fs-ext4-guest-kernel.json"
        report = json.loads(head.read_text())
        report["results"][0]["values"] = [10000.0, 10000.0]
        head.write_text(json.dumps(report))
        comparison = ab_rounds.compare(self.results, "base", "head")
        self.assertEqual(comparison["rounds"]["2"]["kept"], True)

    def test_a_contaminated_round_rerun_contaminated_again_is_no_result(self):
        self.night({1: 0.8, 2: 0.8, 3: 0.8, 4: 0.8}, verdicts={2: "contaminated", 4: "contaminated"},
                   reruns={4: 2})
        comparison = ab_rounds.compare(self.results, "base", "head")
        row = self.row(comparison)
        self.assertEqual(row["verdict"], "no result")
        self.assertEqual(row["lost"], [
            "r2: r2 head-r2 contaminated: others wrote 42.0 MB/s to its disk over the round; "
            "r4 head-r4 contaminated: others wrote 42.0 MB/s to its disk over the round"])

    def test_a_round_left_out_and_not_rerun_is_no_result(self):
        self.night({1: 0.8, 2: 0.8, 3: 0.8}, verdicts={3: "contaminated"})
        self.assertEqual(self.row(ab_rounds.compare(self.results, "base", "head"))["verdict"],
                         "no result")

    def test_the_rounds_to_rerun_are_those_left_out_and_not_rerun_yet(self):
        self.night({1: 0.8, 2: 0.8, 3: 0.8}, control_factors={1: 1.2}, verdicts={3: "contaminated"})
        sides = ab_rounds.Sides.load(self.results, "base", "head")
        self.assertEqual(ab_rounds.discarded(sides, "ext4-guest-kernel", ab_rounds.Bounds()), {
            1: "the control (ext4-guest-kernel) moved -16.7%, past 5%",
            3: "head-r3 contaminated: others wrote 42.0 MB/s to its disk over the round"})
        write_round(self.results, "base", 4, {"ext4-guest-kernel": CONTROL})
        write_round(self.results, "head", 4, {"ext4-guest-kernel": CONTROL})
        (self.results / "reruns.txt").write_text("4 1\n")
        sides = ab_rounds.Sides.load(self.results, "base", "head")
        self.assertEqual(ab_rounds.discarded(sides, "ext4-guest-kernel", ab_rounds.Bounds()), {
            3: "head-r3 contaminated: others wrote 42.0 MB/s to its disk over the round"})

    def test_a_round_without_the_control_is_left_out(self):
        self.night({1: 0.8, 2: 0.8, 3: 0.8})
        (self.results / "head-r1" / "bench-fs-ext4-guest-kernel.json").unlink()
        comparison = ab_rounds.compare(self.results, "base", "head")
        self.assertEqual(comparison["rounds"]["1"]["why"],
                         "no ext4-guest-kernel (the control) measured")

    def test_a_case_the_head_did_not_finish_broke(self):
        self.night({1: 1.0, 2: 1.0, 3: 1.0})
        (self.results / "head-r3" / "bench-fs-fxfs-guest-dax.json").unlink()
        comparison = ab_rounds.compare(self.results, "base", "head")
        self.assertEqual([each["what"] for each in comparison["broken"]],
                         ["no bench-fs-fxfs-guest-dax.json"])
        self.assertEqual(self.row(comparison)["lost"], ["r3: head did not measure it in r3"])

    def test_bounds_are_set_by_name(self):
        bounds = ab_rounds.Bounds.of({"CLADEFS_AB_BOUND": "0.2", "CLADEFS_AB_CONTROL_BOUND": "0.03",
                                      "CLADEFS_AB_BOUNDS": "fsync-append=0.3; ext4-guest-kernel "
                                                           "create=0.5"})
        self.assertEqual(bounds, ab_rounds.Bounds(0.2, 0.03, 0.0, (("fsync-append", 0.3),
                                                                  ("ext4-guest-kernel create", 0.5),
                                                                  ("max_anon", 0.1))))
        self.assertEqual(bounds.of_metric("fxfs-guest-dax", "fsync-append", False), 0.3)
        self.assertEqual(bounds.of_metric("fxfs-guest-dax", "create", False), 0.2)
        self.assertEqual(bounds.of_metric("ext4-guest-kernel", "create", False), 0.5)
        self.night({1: 0.8, 2: 0.8, 3: 0.8})
        self.assertEqual(self.row(ab_rounds.compare(self.results, "base", "head",
                                                    bounds=bounds))["verdict"], "steady")


# What bucket_lifecycle_by_phase and bench_fork_extent_memory print, as the macOS runs of
# 2026-11-15 did (docs/measurements/macos-bucket-lifecycle-2026-11-15,
# macos-fork-extent-memory-2026-11-15), and gc-sweep.sh's tables (macos-gc-dead-cells-2026-11-15).
LIFECYCLE = """\
SETUP extents=1024 point of the base: requests=19 in-series=- at-once=1 wall-ms=971 bucket-ms=0 up=1712036 down=856271 [create=6 get=7 get-range=1 list=3 update=2] again=[get tenants/0000000000000/cells/point's/branches/point x4]
PHASE extents=1024 cold fork: requests=6 in-series=6.4 at-once=1 wall-ms=370 bucket-ms=192 up=170 down=158 [create=2 get=3 update=1] again=[get tenants/0000000000000/cells/cold-fork's/branches/cold-fork x2]
  REQUEST +0..32ms get tenants/0000000000000/cells/cold-fork's/branches/cold-fork up=0 down=0
"""
FORK_MEMORY = """\
fork memory: extents=1024 parent=point forks=20 held=5475939 per-fork=273796 marginal-per-fork=274175 marginal-per-fork-per-extent=267.7 open=125.29ms
fork memory: extents=1024 parent=point of a fork: store-map=242696 (keys=109568 table=133128) writer-map-unshared=234144 (keys=109568 nodes=124576); left-after-close=73541
"""
SWEEPS = """\
| side | at once | round | sweep | seconds | passed | skipped | get | get-range | list | list-delimited | create | update | delete-batch | objects left | peak MiB |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| this | 16 | 1 | 3 | 1.2 | 0 | 100 | 200 | 0 | 100 | 2 | 0 | 200 | 0 | 400 | 12 |
"""


def counts_dir(directory, lifecycle=LIFECYCLE, fork_memory=FORK_MEMORY, sweeps=SWEEPS,
               statuses=None):
    """scripts/counts.sh's outputs in `directory`."""
    directory.mkdir(parents=True)
    (directory / "lifecycle.out").write_text(lifecycle)
    (directory / "fork-memory.out").write_text(fork_memory)
    (directory / "gc-deleted").mkdir()
    (directory / "gc-deleted" / "gc-sweep-requests.md").write_text(sweeps)
    statuses = statuses or {"lifecycle": 0, "fork-memory": 0, "gc-deleted": 0, "gc-clones": 0}
    (directory / "status.txt").write_text("".join(f"{name} {status}\n"
                                                  for name, status in statuses.items()))


class Counts(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.results = Path(scratch.name)

    def rows(self, comparison, verdicts=None):
        return {(row["case"], row["name"]): row["verdict"] for row in comparison["metrics"]
                if row["kind"] == "count" and (verdicts is None or row["verdict"] in verdicts)}

    def test_the_count_benchmarks_are_read_as_counts_and_no_times(self):
        counts_dir(self.results / "base-counts")
        found, statuses = ab_rounds.counted(self.results / "base-counts")
        self.assertEqual(found["lifecycle"], {
            "SETUP extents=1024 point of the base: requests": 19,
            "SETUP extents=1024 point of the base: up": 1712036,
            "SETUP extents=1024 point of the base: down": 856271,
            "SETUP extents=1024 point of the base: create": 6,
            "SETUP extents=1024 point of the base: get": 7,
            "SETUP extents=1024 point of the base: get-range": 1,
            "SETUP extents=1024 point of the base: list": 3,
            "SETUP extents=1024 point of the base: update": 2,
            "PHASE extents=1024 cold fork: requests": 6,
            "PHASE extents=1024 cold fork: up": 170,
            "PHASE extents=1024 cold fork: down": 158,
            "PHASE extents=1024 cold fork: create": 2,
            "PHASE extents=1024 cold fork: get": 3,
            "PHASE extents=1024 cold fork: update": 1,
        })
        self.assertEqual(found["fork-memory"], {
            "extents=1024 parent=point forks=20 held": 5475939,
            "extents=1024 parent=point forks=20 per-fork": 273796,
            "extents=1024 parent=point forks=20 marginal-per-fork": 274175,
            "extents=1024 parent=point of a fork: store-map": 242696,
            "extents=1024 parent=point of a fork: store-map.keys": 109568,
            "extents=1024 parent=point of a fork: store-map.table": 133128,
            "extents=1024 parent=point of a fork: writer-map-unshared": 234144,
            "extents=1024 parent=point of a fork: writer-map-unshared.keys": 109568,
            "extents=1024 parent=point of a fork: writer-map-unshared.nodes": 124576,
            "extents=1024 parent=point of a fork: left-after-close": 73541,
        })
        self.assertEqual(found["gc-deleted"], {
            f"sweep side=this at once=16 round=1 sweep=3 {column}": value
            for column, value in [("passed", 0), ("skipped", 100), ("get", 200), ("get-range", 0),
                                  ("list", 100), ("list-delimited", 2), ("create", 0),
                                  ("update", 200), ("delete-batch", 0), ("objects left", 400)]})
        self.assertEqual(statuses, {"lifecycle": 0, "fork-memory": 0, "gc-deleted": 0,
                                    "gc-clones": 0})

    def test_a_count_benchmark_differing_by_one_changed(self):
        counts_dir(self.results / "base-counts")
        counts_dir(self.results / "head-counts", lifecycle=LIFECYCLE.replace(
            "requests=6 in-series", "requests=7 in-series").replace("get=3 update", "get=4 update"))
        comparison = ab_rounds.compare(self.results, "base", "head")
        self.assertEqual(self.rows(comparison, ab_rounds.MOVED), {
            ("lifecycle", "PHASE extents=1024 cold fork: requests"): "higher",
            ("lifecycle", "PHASE extents=1024 cold fork: get"): "higher"})
        self.assertEqual(len(self.rows(comparison)), 34)

    def test_a_count_benchmark_the_base_lacks_is_not_measured_and_a_failure_broke(self):
        counts_dir(self.results / "base-counts", statuses={"lifecycle": 0, "fork-memory": 4,
                                                           "gc-deleted": 0, "gc-clones": 0})
        (self.results / "base-counts" / "fork-memory.out").write_text("error: no test\n")
        counts_dir(self.results / "head-counts", statuses={"lifecycle": 101, "fork-memory": 0,
                                                           "gc-deleted": 0, "gc-clones": 0})
        comparison = ab_rounds.compare(self.results, "base", "head")
        self.assertEqual(
            self.rows(comparison, {"not measured"})[
                ("fork-memory", "extents=1024 parent=point forks=20 held")], "not measured")
        self.assertEqual(comparison["broken"], [{
            "what": "count benchmark lifecycle failed",
            "said": "scripts/counts.sh's lifecycle ended 101 on head, not on base (see "
                    "head-counts/lifecycle.out)",
            "details": []}])

    def observed(self, head_requests, verdicts=None):
        verdicts = verdicts or {}
        for number, requests in enumerate(head_requests, 1):
            for name, value in (("base", 1832.0), ("head", requests)):
                write_round(self.results, name, number,
                            {"ext4-guest-kernel": CONTROL,
                             "fxfs-guest-dax-fork-cold": {"fork-read-usr": [2.1, 2.2]}},
                            verdict=verdicts.get(number, "clean") if name == "head" else "clean",
                            observations={"fxfs-guest-dax-fork-cold": {
                                "guest-read-requests": [value, value],
                                "guest-read-ms": [812.5, 799.0],
                                "extentd-fetched-bytes": [1048576.0, 1048576.0]}})

    def test_a_count_one_higher_in_every_round_changed_contaminated_rounds_too(self):
        self.observed([1833.0, 1833.0, 1833.0], verdicts={2: "contaminated"})
        comparison = ab_rounds.compare(self.results, "base", "head")
        self.assertEqual(self.rows(comparison), {
            ("fxfs-guest-dax-fork-cold", "extentd-fetched-bytes"): "same",
            ("fxfs-guest-dax-fork-cold", "guest-read-requests"): "higher"})
        row = next(row for row in comparison["metrics"] if row["name"] == "guest-read-requests")
        self.assertEqual([(each["round"], each["a"], each["b"]) for each in row["rounds"]],
                         [(1, 1832.0, 1833.0), (2, 1832.0, 1833.0), (3, 1832.0, 1833.0)])

    def test_a_count_that_changed_in_some_rounds_is_the_same(self):
        self.observed([1833.0, 1832.0, 1833.0])
        self.assertEqual(self.rows(comparison := ab_rounds.compare(self.results, "base", "head"))[
            ("fxfs-guest-dax-fork-cold", "guest-read-requests")], "same")
        self.assertEqual(ab_rounds.moved(comparison), [])

    def test_each_daemons_budget_figures_are_counts(self):
        """scripts/budgets.py's figures in each round's logs/budgets.json: what a daemon wrote by
        kind for each guest byte, exact; its most anonymous memory, a sampled peak, within 10%."""
        for number in (1, 2, 3):
            for name, framed, anon in (("base", 1.002, 1046523000), ("head", 2.004, 1098849150)):
                directory = write_round(self.results, name, number, {"ext4-guest-kernel": CONTROL})
                (directory / "logs").mkdir()
                (directory / "logs" / "budgets.json").write_text(json.dumps([{
                    "daemon": "extentd-stats.txt", "checks": [],
                    "figures": {"max_anon": anon, "anon_budget": 2147483648,
                                "per_guest_byte": {"raw": 1.0, "framed": framed, "io": 1.204}}}]))
        comparison = ab_rounds.compare(self.results, "base", "head")
        self.assertEqual(self.rows(comparison), {
            ("daemon extentd-stats.txt", "max_anon"): "same",
            ("daemon extentd-stats.txt", "written per guest byte, framed"): "higher",
            ("daemon extentd-stats.txt", "written per guest byte, io"): "same",
            ("daemon extentd-stats.txt", "written per guest byte, raw"): "same"})

    def test_a_count_bound_lets_counts_move_within_it(self):
        self.observed([1833.0, 1833.0, 1833.0])
        comparison = ab_rounds.compare(self.results, "base", "head",
                                       bounds=ab_rounds.Bounds(count=0.01))
        self.assertEqual(self.rows(comparison)[
            ("fxfs-guest-dax-fork-cold", "guest-read-requests")], "same")


class Markdown(unittest.TestCase):
    def test_what_moved_comes_first_with_each_rounds_numbers(self):
        with tempfile.TemporaryDirectory() as scratch:
            results = Path(scratch)
            for number in (1, 2):
                write_round(results, "base", number, {"ext4-guest-kernel": CONTROL,
                                                      "fxfs-guest-dax": {"create": [55435.9]}})
                write_round(results, "head", number, {"ext4-guest-kernel": CONTROL,
                                                      "fxfs-guest-dax": {"create": [44348.7]}})
            text = ab_rounds.markdown(ab_rounds.compare(results, "base", "head"))
        self.assertEqual(text.splitlines()[:7], [
            "# head against base, round by round",
            "",
            "Bounds: timings 10%, the control (ext4-guest-kernel) 5%, counts 0%; max_anon 10%.",
            "",
            "## Moved",
            "",
            "- **fxfs-guest-dax create** (ops/s): slower past 10% in every round. r1: base 5.544e+04, "
            "head 4.435e+04 (1.250×); r2: base 5.544e+04, head 4.435e+04 (1.250×)",
        ])


if __name__ == "__main__":
    unittest.main()
