#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Compares two sides of an A/B of the fs bench (scripts/bench-ab-host.sh), each one or more of its
results directories, as rounds alternating between the two make them: each workload's values
pooled across a side's directories and cases, their medians and ranges, how many times slower B
is than A (above 1, B is slower), and whether the two ranges overlap, which a difference within
noise does. It prints a Markdown table.

With --control CASE, each case's workloads are compared apart, and against the control: CASE, a
case whose code is the same on both sides (ext4 on virtio-blk, as `ext4-guest-kernel`), whose own
B-vs-A ratio for the same workload is the drift between the sides' runs. A difference is called
real only where the two sides' ranges don't overlap and it is larger than the control's drift for
that workload (TASK-276): the rounds interleave the sides on one host, and what still moves the
control moves the others as much.

Without one, it says too which workloads moved, their ranges apart, and lists those one side
alone measured. And each side's failures: of each kind, how many of its directories (its rounds)
had one, and what each said, if they say (a qualification's checks, scripts/qualify.sh, TASK-330.4).

A round that lacks a result another round has (a case's bench-*.json), or whose part says it didn't
run (a failure of a kind `PART.not-run`), compared nothing for it: that fails the comparison, said
first, before the tables, and it exits 1. A qualification's A/B ran hours with its sandboxes' part
started on neither side, the tables just without its rows.

A round others wrote beside (its contamination.json, scripts/contamination.py, TASK-375:
`fxjob --bench` doesn't isolate the disk) is contaminated: its timings and rates are left out of
the comparison, said before the tables and in each cell they are missing from, as a load beside a
round skews them; its counts and sizes (bytes written for each guest byte, peak memory, requests)
stay, as it doesn't skew those. A metric with no clean round on a side is NO RESULT, never a ratio
nor "within drift": said first, and it exits 1. The unchecked rounds (no record, or a
host where others' I/O can't be told from the round's own) are listed, their timings kept.

Usage: scripts/bench-ab.py [--control CASE] A_NAME A_DIR... -- B_NAME B_DIR...
"""

import importlib.util
import json
import math
import statistics
import sys
from dataclasses import dataclass, field
from pathlib import Path

# By its path, which needs no module path naming this directory.
spec = importlib.util.spec_from_file_location(
    "contamination", Path(__file__).resolve().with_name("contamination.py")
)
contamination = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = contamination
spec.loader.exec_module(contamination)


def measured(directories):
    """What `directories`' rounds measured: each result of each round's cases (bench-*.json), with
    its round's directory and its case, the report's backend."""
    for directory in directories:
        for path in sorted(Path(directory).glob("bench-*.json")):
            report = json.loads(path.read_text())
            for result in report["results"]:
                yield directory, report.get("backend", path.stem), result


def verdicts(directories):
    """The rounds of `directories` by what their records say of them (contamination.load):
    {verdict: {why: [round]}}."""
    by = {}
    for directory in directories:
        record = contamination.load(directory)
        by.setdefault(record["verdict"], {}).setdefault("; ".join(record["why"]), []).append(
            Path(directory).name)
    return by


@dataclass
class Side:
    """A side's rounds' measurements, by workload, or by (case, workload): `values`, pooled, but
    those a load beside their round skews (contamination.skewed) of the rounds others wrote
    beside; `left`, how many of those rounds each is missing from; and each workload's unit and
    direction, `kinds`."""

    values: dict = field(default_factory=dict)
    left: dict = field(default_factory=dict)
    kinds: dict = field(default_factory=dict)

    @classmethod
    def of(cls, directories, key):
        """The side whose rounds are `directories`, its measurements by `key` of each's case and
        workload."""
        side, contaminated = cls(), set()
        for directory in directories:
            if contamination.load(directory)["verdict"] == "contaminated":
                contaminated.add(directory)
        left = {}
        for directory, case, result in measured(directories):
            name = key(case, result["workload"])
            side.kinds[result["workload"]] = (result["unit"], result["higher_is_better"])
            if directory in contaminated and contamination.skewed(result["unit"]):
                left.setdefault(name, set()).add(directory)
            else:
                side.values.setdefault(name, []).extend(result["values"])
        side.left = {name: len(rounds) for name, rounds in left.items()}
        return side

    def has(self, name):
        """Whether a round of it measured `name`, left out or not."""
        return name in self.values or name in self.left

    def cell(self, name):
        """What it measured of `name`, as a table's cell."""
        if name not in self.values:
            return (f"NO RESULT [contaminated rounds left out: {self.left[name]}]"
                    if name in self.left else "-")
        left = f" [contaminated rounds left out: {self.left[name]}]" if name in self.left else ""
        return described(self.values[name]) + left

    def no_result(self):
        """What it has no clean round of."""
        return [name for name in self.left if name not in self.values]


def pooled(directories):
    """The side of `directories`, each workload's values pooled across its rounds and cases."""
    return Side.of(directories, lambda case, workload: workload)


def by_case(directories):
    """The side of `directories`, each case's workloads' values pooled across its rounds, by
    (case, workload)."""
    return Side.of(directories, lambda case, workload: (case, workload))


def slower(a, b, higher_is_better):
    """How many times slower B's values are than A's, by their medians: above 1, B is slower."""
    a_median, b_median = statistics.median(a), statistics.median(b)
    return a_median / b_median if higher_is_better else b_median / a_median


def overlap(a, b):
    return max(min(a), min(b)) <= min(max(a), max(b))


def verdict(ratio, overlaps, control):
    """Whether a difference of `ratio` is real: the ranges apart, and further from 1 than the
    control's own ratio, `control` (None if the control has no such workload)."""
    if control is None:
        return "no control"
    if overlaps or abs(math.log(ratio)) <= abs(math.log(control)):
        return "within drift"
    return "real"


def control_table(a_name, a_dirs, b_name, b_dirs, control):
    a, b = by_case(a_dirs), by_case(b_dirs)
    lines = [
        f"| Case | Workload | Unit | {a_name} | {b_name} | {b_name} vs {a_name} | "
        f"{control} {b_name} vs {a_name} | Verdict |",
        "|---|---|---|---|---|---|---|---|",
    ]
    for case, workload in sorted(name for name in {**a.values, **a.left} if b.has(name)):
        if case == control:
            continue
        unit, higher_is_better = a.kinds[workload]
        name, of_control = (case, workload), (control, workload)
        drift = None
        if of_control in a.values and of_control in b.values:
            drift = slower(a.values[of_control], b.values[of_control], higher_is_better)
        ratio, said = "-", "NO RESULT"
        if name in a.values and name in b.values:
            times = slower(a.values[name], b.values[name], higher_is_better)
            ratio = f"{times:.2f}×"
            said = verdict(times, overlap(a.values[name], b.values[name]), drift)
        lines.append(
            f"| {case} | {workload} | {unit} | {a.cell(name)} | {b.cell(name)} | {ratio} | "
            f"{'-' if drift is None else f'{drift:.2f}×'} | {said} |"
        )
    return "\n".join(lines)


def described(values):
    return f"{statistics.median(values):,.4g} ({min(values):,.4g}–{max(values):,.4g})"


def table(a_name, a_dirs, b_name, b_dirs):
    a, b = pooled(a_dirs), pooled(b_dirs)
    lines = [
        f"| Workload | Unit | {a_name} | {b_name} | {b_name} vs {a_name} | Ranges overlap |",
        "|---|---|---|---|---|---|",
    ]
    # A's workloads, then those B alone measured.
    kinds = dict(a.kinds)
    kinds.update((workload, kind) for workload, kind in b.kinds.items() if workload not in kinds)
    for workload, (unit, higher_is_better) in kinds.items():
        ratio = overlaps = "-"
        if workload in a.values and workload in b.values:
            ratio = f"{slower(a.values[workload], b.values[workload], higher_is_better):.2f}×"
            overlaps = "yes" if overlap(a.values[workload], b.values[workload]) else "no"
        elif a.has(workload) and b.has(workload):
            overlaps = "NO RESULT"
        lines.append(f"| {workload} | {unit} | {a.cell(workload)} | {b.cell(workload)} | {ratio} | "
                     f"{overlaps} |")
    return "\n".join(lines)


def moved(a_dirs, b_dirs):
    """The workloads both sides measured whose ranges are apart."""
    a, b = pooled(a_dirs).values, pooled(b_dirs).values
    return [workload for workload in a if workload in b and not overlap(a[workload], b[workload])]


def others_beside(a_name, a_dirs, b_name, b_dirs, sides):
    """What the rounds' records of others' I/O beside them (scripts/contamination.py) mean for
    the comparison, as its sections, and what of them fails it, a line each: a metric with no
    clean round on a side of `sides`, the two sides as the table pools them."""
    sections, failed = [], []
    for name, side in zip((a_name, b_name), sides):
        missing = side.no_result()
        if missing:
            metrics = ", ".join(sorted(" ".join(metric) if isinstance(metric, tuple) else metric
                                       for metric in missing))
            failed.append(f"{name} has no clean round of: {metrics}")
    if failed:
        sections.append("## NO RESULT: every round that measured these was contaminated\n\n"
                        + "\n".join(f"- {line}" for line in failed))
    by = verdicts(a_dirs + b_dirs)
    for verdict_, heading in (
            ("contaminated", "## CONTAMINATED: others wrote beside these rounds; their timings "
                             "are left out, their counts and sizes kept"),
            ("unchecked", "## UNCHECKED: whether others wrote beside these rounds is not "
                          "known; their timings are kept")):
        if verdict_ in by:
            sections.append(heading + "\n\n" + "\n".join(
                f"- {', '.join(rounds)}: {why}" for why, rounds in by[verdict_].items()))
    return sections, failed


def failures(directories):
    """Of each kind of failure in `directories`' results, how many of the directories had one; and
    what each failure said, by directory."""
    rounds, said = {}, []
    for directory in directories:
        kinds = set()
        for path in sorted(Path(directory).glob("bench-*.json")):
            report = json.loads(path.read_text())
            kinds.update(kind for kind, count in report.get("failures", {}).items() if count)
            said.extend(f"{Path(directory).name}: {text}" for text in report.get("failed", []))
        for kind in kinds:
            rounds[kind] = rounds.get(kind, 0) + 1
    return rounds, said


def failure_table(a_name, a_dirs, b_name, b_dirs):
    """Each side's failures (`failures`), a table of their kinds, then what each said; empty if
    neither side failed."""
    (a, a_said), (b, b_said) = failures(a_dirs), failures(b_dirs)
    if not a and not b:
        return ""
    lines = [f"| Failed | {a_name} | {b_name} |", "|---|---|---|"]
    for kind in sorted(set(a) | set(b)):
        lines.append(f"| {kind} | {a.get(kind, 0)} of {len(a_dirs)} | "
                     f"{b.get(kind, 0)} of {len(b_dirs)} |")
    said = a_said + b_said
    if said:
        lines += [""] + [f"- {text}" for text in said]
    return "\n".join(lines)


def not_run(directories):
    """What `directories`' rounds didn't run, a line each: each result another round has that a
    round lacks, and each part a round says didn't run (`PART.not-run`)."""
    results = {Path(directory): {path.name for path in Path(directory).glob("bench-*.json")}
               for directory in directories}
    every = set().union(*results.values()) if results else set()
    said = []
    for directory, has in results.items():
        if not has:
            said.append(f"{directory.name}: no result at all")
        for name in sorted(every - has):
            said.append(f"{directory.name}: no {name}")
        for name in sorted(has):
            report = json.loads((directory / name).read_text())
            for kind, count in sorted(report.get("failures", {}).items()):
                if count and kind.endswith(".not-run"):
                    said.append(f"{directory.name}: {kind.removesuffix('.not-run')} not run")
    return said


def main():
    args = sys.argv[1:]
    control = None
    if args[:1] == ["--control"] and len(args) > 1:
        control, args = args[1], args[2:]
    if "--" not in args or args.index("--") < 2 or len(args) - args.index("--") < 3:
        sys.exit(__doc__.strip().splitlines()[-1])
    cut = args.index("--")
    a_name, a_dirs, b_name, b_dirs = args[0], args[1:cut], args[cut + 1], args[cut + 2 :]
    missing = not_run(a_dirs + b_dirs)
    if missing:
        print("## NOT RUN: this comparison is incomplete\n")
        print("\n".join(f"- {line}" for line in missing) + "\n")
        print("NOT RUN: " + "; ".join(missing), file=sys.stderr)
    sides = (by_case if control else pooled)(a_dirs), (by_case if control else pooled)(b_dirs)
    sections, no_result = others_beside(a_name, a_dirs, b_name, b_dirs, sides)
    if no_result:
        print("NO RESULT: " + "; ".join(no_result), file=sys.stderr)
    for section in sections:
        print(section + "\n")
    if control:
        print(control_table(a_name, a_dirs, b_name, b_dirs, control))
    else:
        print(table(a_name, a_dirs, b_name, b_dirs))
        print(f"\nMoved, their ranges apart: {', '.join(moved(a_dirs, b_dirs)) or 'none'}.")
    failed = failure_table(a_name, a_dirs, b_name, b_dirs)
    if failed:
        print(f"\n{failed}")
    return 1 if missing or no_result else 0


if __name__ == "__main__":
    sys.exit(main())
