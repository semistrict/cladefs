#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Compares the two sides of an A/B of the fs bench round by round (TASK-441.1), as the nightly
reads one (scripts/nightly.py): RESULTS/NAME-rN, each side's round N (scripts/lib/
bench-ab-linux-host.sh), the sides interleaved in each round, so that what the host does over a
round moves both alike. scripts/bench-ab.py pools a side's rounds; this keeps them apart, as a
change of the code moves a metric the same way in every round, and the host's drift does not.

Each round, each side's value of a metric is the median of its iterations there, and B's against
A's is the ratio of the two (above 1, B is slower, scripts/bench-ab.py's `slower`). A round is left
out of the timings, and rerun once (CLADEFS_AB_RERUN, bench-ab-linux-host.sh), when:

- contamination.py marks either side's round contaminated (others wrote beside it, TASK-375); or
- its control moved: the median, across the control case's timings (ext4 on virtio-blk, which
  runs no cladefs), of their ratios is past CONTROL bound, either way. A host that moves shifts
  most of what runs on it one way; one of the control's workloads past its bound alone does not
  move the median. A round without the control is left out too, there being nothing to tell the
  host's drift from the code's.

Each round asked for is a slot, which its rerun (RESULTS/reruns.txt, `RERUN ROUND` lines) stands in
for if it was left out. A slot with neither round kept is lost, and every timing it had is NO
RESULT: a metric is reported moved only when it moved the same way past its bound in every slot,
and a lost one could have said otherwise. Moved: B slower (or faster) past BOUND in every slot;
steady otherwise.

Counts, sizes and ratios (contamination.COUNTS's units; the guest's observations of requests,
bytes and the daemon's counters; and each extent daemon's figures in the round's
logs/budgets.json, scripts/budgets.py's: what it wrote by kind for each guest byte, and its most
anonymous memory), which a load beside a round doesn't move, are compared in every round,
contaminated or not: changed when B's differs from A's past COUNT bound, the same way, in
every round both sides have. So too the exact count benchmarks each side ran once with its build
(scripts/counts.sh's outputs in RESULTS/NAME-counts, CLADEFS_AB_COUNTS): changed when B's differs
from A's past COUNT bound.

Bounds, fractions of A's value (`Bounds.of`): CLADEFS_AB_BOUND (default 0.10, the slowdown the
nightly is to report, TASK-441's mutant of 10%); CLADEFS_AB_CONTROL_BOUND (0.05: a host that drifted
more than half that can't tell a 10% change); CLADEFS_AB_COUNT_BOUND (0: counts are exact); and
CLADEFS_AB_BOUNDS, `METRIC=FRACTION` entries separated by `;`, METRIC a metric's name, `CASE
WORKLOAD` (`fxfs-guest-dax fsync-append`), or its workload's or count's alone, for each case,
before NAMED's: a daemon's most anonymous memory within 10%, a peak its stats sample every
interval rather than a count (budgets.py), which moves with what was resident as it sampled.

Usage:
  scripts/ab-rounds.py discarded [--control CASE] RESULTS A_NAME B_NAME
      the rounds to rerun, a number a line, and why each, on standard error
  scripts/ab-rounds.py compare [--control CASE] [--json FILE] RESULTS A_NAME B_NAME
      the comparison, as Markdown; with --json, as JSON in FILE too
"""

import argparse
import importlib.util
import json
import os
import re
import statistics
import sys
from dataclasses import dataclass, field
from pathlib import Path

HERE = Path(__file__).resolve().parent


def module(name, file):
    """scripts/FILE, by its path, which needs no module path naming this directory."""
    if name in sys.modules:
        return sys.modules[name]
    spec = importlib.util.spec_from_file_location(name, HERE / file)
    loaded = importlib.util.module_from_spec(spec)
    sys.modules[name] = loaded
    spec.loader.exec_module(loaded)
    return loaded


contamination = module("contamination", "contamination.py")
bench_ab = module("bench_ab", "bench-ab.py")

CONTROL = "ext4-guest-kernel"
# The guest's observations that are counts: what a load beside a round doesn't move. Requests,
# bytes and reads it counted, and the extent daemon's counters but its times.
COUNTED = re.compile(r"(-requests|-bytes|-reads)$|^extentd-(?!.*-(ms|ns|us|secs)$)")
ROUND = re.compile(r"-r([1-9][0-9]*)$")
# The bounds of metrics named apart by default, (METRIC, FRACTION): Bounds.of puts the caller's
# before them.
NAMED = (("max_anon", 0.10),)


@dataclass(frozen=True)
class Bounds:
    """How far B may be from A, as a fraction of A, before a metric moved: timings', the
    control's, counts', and those named apart, (METRIC, FRACTION) pairs."""

    timing: float = 0.10
    control: float = 0.05
    count: float = 0.0
    named: tuple = NAMED

    @classmethod
    def of(cls, environ):
        """The bounds `environ` sets, CLADEFS_AB_*BOUND*, the defaults otherwise."""

        def fraction(text, what):
            try:
                value = float(text)
            except ValueError:
                value = -1.0
            if not 0 <= value < 10:
                sys.exit(f"{what} is a fraction of A's value, such as 0.10: {text}")
            return value

        named = []
        for entry in environ.get("CLADEFS_AB_BOUNDS", "").split(";"):
            if not entry.strip():
                continue
            metric, equals, value = entry.rpartition("=")
            if not equals or not metric.strip():
                sys.exit(f"CLADEFS_AB_BOUNDS is METRIC=FRACTION entries, separated by ';': {entry}")
            named.append((metric.strip(), fraction(value, f"CLADEFS_AB_BOUNDS's {metric.strip()}")))
        return cls(
            fraction(environ.get("CLADEFS_AB_BOUND") or cls.timing, "CLADEFS_AB_BOUND"),
            fraction(environ.get("CLADEFS_AB_CONTROL_BOUND") or cls.control,
                     "CLADEFS_AB_CONTROL_BOUND"),
            fraction(environ.get("CLADEFS_AB_COUNT_BOUND") or cls.count, "CLADEFS_AB_COUNT_BOUND"),
            tuple(named) + NAMED)

    def of_metric(self, case, name, counted):
        """The bound of metric `name` of `case` (a case, or a count benchmark): one named for
        `CASE NAME`, else for NAME, else counts' or timings'."""
        for wanted in (f"{case} {name}", name):
            for metric, value in self.named:
                if metric == wanted:
                    return value
        return self.count if counted else self.timing


def past(ratio, bound):
    """Which way `ratio`, B's against A's, moved past `bound`: +1 up, -1 down, 0 within it.
    Strictly past: a ratio of exactly 1 + bound is within it."""
    if ratio > 1 + bound:
        return 1
    if ratio < 1 / (1 + bound):
        return -1
    return 0


def changed(a, b, bound):
    """Which way count `b` differs from `a` past `bound`, a fraction of `a`: +1, -1 or 0."""
    if abs(b - a) <= bound * abs(a):
        return 0
    return 1 if b > a else -1


@dataclass
class Round:
    """A side's round: each metric's values by (case, name), its timings' units and directions,
    and whether others wrote beside it, and why."""

    directory: Path
    timings: dict = field(default_factory=dict)
    counts: dict = field(default_factory=dict)
    kinds: dict = field(default_factory=dict)
    verdict: str = "unchecked"
    why: str = ""

    @classmethod
    def load(cls, directory):
        found = cls(Path(directory))
        record = contamination.load(directory)
        found.verdict, found.why = record["verdict"], "; ".join(record["why"])
        for path in sorted(Path(directory).glob("bench-*.json")):
            report = json.loads(path.read_text())
            case = report.get("backend", path.stem.removeprefix("bench-"))
            for result in report["results"]:
                key = (case, result["workload"])
                found.kinds[key] = (result["unit"], result["higher_is_better"])
                into = found.timings if contamination.skewed(result["unit"]) else found.counts
                into.setdefault(key, []).extend(result["values"])
            for name, values in report.get("observations", {}).items():
                if COUNTED.search(name):
                    found.counts.setdefault((case, name), []).extend(values)
        budgets = Path(directory) / "logs" / "budgets.json"
        if budgets.exists():
            for daemon in json.loads(budgets.read_text()):
                case, figures = f"daemon {daemon['daemon']}", daemon["figures"]
                if "max_anon" in figures:
                    found.counts[(case, "max_anon")] = [figures["max_anon"]]
                    found.kinds[(case, "max_anon")] = ("bytes", False)
                for kind, value in figures.get("per_guest_byte", {}).items():
                    key = (case, f"written per guest byte, {kind}")
                    found.counts[key] = [value]
                    found.kinds[key] = ("x", False)
        return found


def rounds(results, name):
    """Side `name`'s rounds in `results`, by number."""
    found = {}
    for directory in Path(results).glob(f"{name}-r*"):
        number = ROUND.search(directory.name)
        if directory.is_dir() and number and directory.name == f"{name}-r{number[1]}":
            found[int(number[1])] = directory
    return dict(sorted(found.items()))


def reruns(results):
    """Each rerun's round, by the number of the round it reran (RESULTS/reruns.txt)."""
    path = Path(results) / "reruns.txt"
    found = {}
    if path.exists():
        for line in path.read_text().splitlines():
            rerun, original = (int(word) for word in line.split())
            found[original] = rerun
    return found


def drift(a, b, control):
    """The control's drift in a round, B's against A's: the median of its timings' ratios; None
    if neither side's round measured it."""
    ratios = [bench_ab.slower(a.timings[key], b.timings[key], a.kinds[key][1])
              for key in a.timings if key[0] == control and key in b.timings]
    return statistics.median(ratios) if ratios else None


def left_out(a, b, control, bounds):
    """Why a round, `a` and `b` its sides', is left out of the timings, or None if it is kept."""
    for side in (a, b):
        if side.verdict == "contaminated":
            return f"{side.directory.name} contaminated: {side.why}"
    if control is None:
        return None
    moved = drift(a, b, control)
    if moved is None:
        return f"no {control} (the control) measured"
    if past(moved, bounds.control):
        return (f"the control ({control}) moved {(moved - 1) * 100:+.1f}%, past "
                f"{bounds.control * 100:g}%")
    return None


@dataclass
class Sides:
    """An A/B's results: each side's rounds, and the reruns."""

    results: Path
    a_name: str
    b_name: str
    a: dict
    b: dict
    reruns: dict

    @classmethod
    def load(cls, results, a_name, b_name):
        loaded = {name: {n: Round.load(path) for n, path in rounds(results, name).items()}
                  for name in (a_name, b_name)}
        return cls(Path(results), a_name, b_name, loaded[a_name], loaded[b_name],
                   reruns(results))

    def numbers(self):
        """The rounds both sides have."""
        return [n for n in self.a if n in self.b]

    def slots(self):
        """The rounds asked for: those no rerun was of another round."""
        rerun = set(self.reruns.values())
        return [n for n in self.numbers() if n not in rerun]

    def judged(self, control, bounds):
        """Why each round was left out of the timings, None if kept, by number."""
        return {n: left_out(self.a[n], self.b[n], control, bounds) for n in self.numbers()}


def discarded(sides, control, bounds):
    """The slots to rerun: the rounds asked for that were left out, none rerun yet, by number,
    with why."""
    judged = sides.judged(control, bounds)
    return {n: judged[n] for n in sides.slots() if judged[n] and n not in sides.reruns}


def median(values):
    return statistics.median(values) if values else None


def timing_rows(sides, control, bounds, judged):
    """Each timing both sides measured, but the control's, judged over the slots."""
    keys = sorted({key for n in sides.numbers() for key in sides.a[n].timings
                   if key[0] != control}
                  | {key for n in sides.numbers() for key in sides.b[n].timings
                     if key[0] != control})
    rows = []
    for key in keys:
        case, workload = key
        kind = next(side[n].kinds[key] for side in (sides.a, sides.b) for n in side
                    if key in side[n].kinds)
        bound = bounds.of_metric(case, workload, False)
        slots, ways, lost = [], [], []
        for slot in sides.slots():
            taken = None
            tried = [slot] + ([sides.reruns[slot]] if slot in sides.reruns else [])
            for n in tried:
                if judged.get(n) is None and n in sides.a and n in sides.b:
                    taken = n
                    break
            if taken is None:
                lost.append(f"r{slot}: " + "; ".join(f"r{n} {judged.get(n)}" for n in tried))
                continue
            a, b = median(sides.a[taken].timings.get(key)), median(sides.b[taken].timings.get(key))
            if a is None or b is None:
                missing = sides.a_name if a is None else sides.b_name
                lost.append(f"r{slot}: {missing} did not measure it in r{taken}")
                slots.append({"slot": slot, "round": taken, "a": a, "b": b})
                continue
            ratio = bench_ab.slower([a], [b], kind[1])
            way = past(ratio, bound)
            ways.append(way)
            slots.append({"slot": slot, "round": taken, "a": a, "b": b, "ratio": ratio,
                          "way": way})
        if lost:
            verdict = "no result"
        elif ways and all(way == 1 for way in ways):
            verdict = "slower"
        elif ways and all(way == -1 for way in ways):
            verdict = "faster"
        else:
            verdict = "steady"
        rows.append({"kind": "timing", "case": case, "name": workload, "unit": kind[0],
                     "higher_is_better": kind[1], "bound": bound, "verdict": verdict,
                     "rounds": slots, "lost": lost})
    return rows


def count_rows(sides, bounds):
    """Each count both sides measured, compared in every round both have."""
    keys = sorted({key for n in sides.numbers() for side in (sides.a, sides.b)
                   for key in side[n].counts})
    rows = []
    for key in keys:
        case, name = key
        bound = bounds.of_metric(case, name, True)
        compared, ways = [], []
        for n in sides.numbers():
            a, b = median(sides.a[n].counts.get(key)), median(sides.b[n].counts.get(key))
            if a is None or b is None:
                continue
            way = changed(a, b, bound)
            ways.append(way)
            compared.append({"round": n, "a": a, "b": b, "way": way})
        unit = next((side[n].kinds[key][0] for side in (sides.a, sides.b) for n in side
                     if key in side[n].kinds), "count")
        rows.append({"kind": "count", "case": case, "name": name, "unit": unit, "bound": bound,
                     "verdict": verdict_of(ways), "rounds": compared, "lost": []})
    return rows


def verdict_of(ways):
    """A count's verdict over what each round said: higher or lower in every one, else the same;
    not measured with none."""
    if not ways:
        return "not measured"
    if all(way == 1 for way in ways):
        return "higher"
    if all(way == -1 for way in ways):
        return "lower"
    return "same"


# What of a count benchmark's lines (scripts/counts.sh) are no counts: times, and what follows
# from them (how many requests overlapped, round trips' worth of latency waited).
TIMED = frozenset({"in-series", "at-once", "wall-ms", "bucket-ms", "open",
                   "marginal-per-fork-per-extent"})
PAIR = re.compile(r"([A-Za-z][\w-]*)=(\d+)(?: \(([^)]*)\))?")
# The gc-sweep tables' columns that name a row, and those that are no counts.
NAMING = ("side", "at once", "round", "sweep", "pass")
UNCOUNTED = frozenset({"seconds", "peak MiB"})


def pairs(text, prefix):
    """Each `KEY=INTEGER` of `text`, but times, as (PREFIX KEY, value), a parenthesized group's
    keys after their own's: `store-map=7 (keys=3 table=4)` is store-map, store-map.keys and
    store-map.table."""
    text = re.sub(r"again=\[[^\]]*\]", "", text)
    for match in PAIR.finditer(text):
        key, value, inner = match[1], match[2], match[3]
        if key in TIMED:
            continue
        yield f"{prefix} {key}", float(value)
        for inner_key, inner_value, _ in PAIR.findall(inner or ""):
            yield f"{prefix} {key}.{inner_key}", float(inner_value)


def lifecycle(text):
    """bucket_lifecycle_by_phase's counts: each SETUP, PHASE and TOTAL line's requests, bytes and
    requests by kind, by its extents and phase."""
    for line in text.splitlines():
        found = re.match(r"(SETUP|PHASE|TOTAL) (extents=\d+ [^:]+): (.*)$", line)
        if found:
            yield from pairs(found[3], f"{found[1]} {found[2]}:")


def fork_memory(text):
    """bench_fork_extent_memory's counts: each line's bytes held, by its extents, parent and
    forks open, and each owner's of a fork."""
    for line in text.splitlines():
        found = re.match(r"fork memory: extents=(\d+) parent=(\S+) (forks=\d+|of a fork:) (.*)$",
                         line)
        if found:
            yield from pairs(found[4], f"extents={found[1]} parent={found[2]} {found[3]}")


def tables(text, prefix):
    """A gc-sweep table's counts (gc-sweep-requests.md, gc-sweep-cell.md): each row's counted
    columns, by the columns naming the row."""
    header = None
    for line in text.splitlines():
        if not line.startswith("|") or set(line) <= set("|-"):
            continue
        cells = [cell.strip() for cell in line.strip("|").split("|")]
        if header is None:
            header = cells
            continue
        row = dict(zip(header, cells))
        named = " ".join(f"{column}={row[column]}" for column in NAMING if column in row)
        for column in header:
            if column in NAMING or column in UNCOUNTED:
                continue
            try:
                value = float(row[column].replace(",", ""))
            except ValueError:
                continue
            yield f"{prefix} {named} {column}", value


# Each count benchmark's outputs, as scripts/counts.sh leaves them in its directory: the file, and
# how its counts are read.
COUNT_BENCHES = {
    "lifecycle": ("lifecycle.out", lifecycle),
    "fork-memory": ("fork-memory.out", fork_memory),
    "gc-deleted": ("gc-deleted/gc-sweep-requests.md", lambda text: tables(text, "sweep")),
    "gc-clones": ("gc-clones/gc-sweep-requests.md", lambda text: tables(text, "sweep")),
    "gc-clones-cell": ("gc-clones/gc-sweep-cell.md", lambda text: tables(text, "pass")),
}


def counted(directory):
    """What each count benchmark in `directory` counted, {bench: {name: value}}, those it has;
    and each benchmark's status as scripts/counts.sh recorded it, {bench: status}."""
    directory = Path(directory)
    found, statuses = {}, {}
    status = directory / "status.txt"
    if status.exists():
        for line in status.read_text().splitlines():
            name, _, value = line.partition(" ")
            statuses[name] = int(value)
    for bench, (file, read) in COUNT_BENCHES.items():
        path = directory / file
        if path.exists():
            # A name a benchmark says again (a phase it runs twice) is told apart by its turn.
            values, seen = {}, {}
            for name, value in read(path.read_text()):
                seen[name] = seen.get(name, 0) + 1
                values[name if seen[name] == 1 else f"{name} #{seen[name]}"] = value
            found[bench] = values
    return found, statuses


def bench_rows(sides, bounds):
    """Each count benchmark's counts (scripts/counts.sh), A's against B's, with its build."""
    a_dir, b_dir = (sides.results / f"{name}-counts" for name in (sides.a_name, sides.b_name))
    if not a_dir.exists() and not b_dir.exists():
        return []
    (a, a_status), (b, b_status) = counted(a_dir), counted(b_dir)
    rows = []
    for bench in COUNT_BENCHES:
        names = sorted(set(a.get(bench, {})) | set(b.get(bench, {})))
        for name in names:
            bound = bounds.of_metric(bench, name, True)
            av, bv = a.get(bench, {}).get(name), b.get(bench, {}).get(name)
            if av is None or bv is None:
                verdict, way = "not measured", None
            else:
                way = changed(av, bv, bound)
                verdict = verdict_of([way])
            rows.append({"kind": "count", "case": bench, "name": name, "unit": "count",
                         "bound": bound, "verdict": verdict,
                         "rounds": [{"round": "build", "a": av, "b": bv, "way": way}],
                         "lost": []})
    return rows


def broken(sides):
    """What failed on B that didn't on A: each failure kind B's rounds had and A's never did, and
    each case's results some B round lacks that every A round has."""
    found = []
    a_dirs = [round_.directory for round_ in sides.a.values()]
    b_dirs = [round_.directory for round_ in sides.b.values()]
    (a_failed, _), (b_failed, b_said) = bench_ab.failures(a_dirs), bench_ab.failures(b_dirs)
    for kind in sorted(b_failed):
        if not a_failed.get(kind):
            found.append({"what": f"failed {kind}",
                          "said": f"{b_failed[kind]} of {len(b_dirs)} {sides.b_name} rounds failed "
                                  f"{kind}, no {sides.a_name} round did",
                          "details": [text for text in b_said if kind in text] or b_said})
    every_a = None
    for directory in a_dirs:
        names = {path.name for path in directory.glob("bench-*.json")}
        every_a = names if every_a is None else every_a & names
    for name in sorted(every_a or ()):
        lacking = [directory.name for directory in b_dirs if not (directory / name).exists()]
        if lacking:
            found.append({"what": f"no {name}",
                          "said": f"{', '.join(lacking)} wrote no {name}, which every "
                                  f"{sides.a_name} round did: the case failed",
                          "details": []})
    a_counts, b_counts = (sides.results / f"{name}-counts" for name in (sides.a_name, sides.b_name))
    if b_counts.exists():
        a_status, b_status = counted(a_counts)[1], counted(b_counts)[1]
        for bench, status in sorted(b_status.items()):
            if status and not a_status.get(bench):
                found.append({"what": f"count benchmark {bench} failed",
                              "said": f"scripts/counts.sh's {bench} ended {status} on "
                                      f"{sides.b_name}, not on {sides.a_name} (see "
                                      f"{b_counts.name}/{bench}.out)",
                              "details": []})
    return found


def compare(results, a_name, b_name, control=CONTROL, bounds=Bounds()):
    """The comparison of the A/B in `results`, its sides `a_name` and `b_name`, as a dict:
    each round kept or why not, each timing's and count's verdict with its rounds' numbers, and
    what broke on B."""
    sides = Sides.load(results, a_name, b_name)
    judged = sides.judged(control, bounds)
    rows = timing_rows(sides, control, bounds, judged) + count_rows(sides, bounds) + \
        bench_rows(sides, bounds)
    return {
        "a": a_name,
        "b": b_name,
        "control": control,
        "bounds": {"timing": bounds.timing, "control": bounds.control, "count": bounds.count,
                   "named": dict(bounds.named)},
        "rounds": {str(n): {"kept": judged[n] is None, "why": judged[n],
                            "rerun_of": next((str(o) for o, r in sides.reruns.items() if r == n),
                                             None)}
                   for n in sides.numbers()},
        "slots": sides.slots(),
        "metrics": rows,
        "broken": broken(sides),
    }


MOVED = ("slower", "faster", "higher", "lower")


def moved(comparison):
    """The metrics of `comparison` that moved, or changed."""
    return [row for row in comparison["metrics"] if row["verdict"] in MOVED]


def no_result(comparison):
    return [row for row in comparison["metrics"] if row["verdict"] == "no result"]


def number(value):
    if value is None:
        return "-"
    if float(value).is_integer() and abs(value) < 1e15:
        return f"{int(value):,}"
    return f"{value:,.4g}"


def metric(row):
    return f"{row['case']} {row['name']}"


def rounds_line(row, a_name, b_name):
    """Each round's numbers of `row`, a line."""
    said = []
    for each in row["rounds"]:
        where = f"r{each['round']}" if isinstance(each["round"], int) else each["round"]
        if each.get("slot") not in (None, each["round"]):
            where += f" (for r{each['slot']})"
        ratio = f" ({each['ratio']:.3f}×)" if "ratio" in each else ""
        said.append(f"{where}: {a_name} {number(each['a'])}, {b_name} {number(each['b'])}{ratio}")
    return "; ".join(said)


def markdown(comparison):
    """`comparison` as Markdown: what moved first, then what has no result and why, the rounds
    left out, what broke, then every metric's verdict."""
    a, b = comparison["a"], comparison["b"]
    bounds = comparison["bounds"]
    lines = [f"# {b} against {a}, round by round", "",
             f"Bounds: timings {bounds['timing'] * 100:g}%, the control ({comparison['control']}) "
             f"{bounds['control'] * 100:g}%, counts {bounds['count'] * 100:g}%"
             + "".join(f"; {name} {value * 100:g}%" for name, value in bounds["named"].items())
             + ".", ""]
    went = moved(comparison)
    lines += ["## Moved", ""]
    lines += [f"- **{metric(row)}** ({row['unit']}): {row['verdict']} past "
              f"{row['bound'] * 100:g}% in every round. {rounds_line(row, a, b)}"
              for row in went] or ["Nothing."]
    if comparison["broken"]:
        lines += ["", "## Broke", ""]
        lines += [f"- **{each['what']}**: {each['said']}" for each in comparison["broken"]]
    lost = no_result(comparison)
    if lost:
        lines += ["", "## NO RESULT", ""]
        lines += [f"- {metric(row)}: {'; '.join(row['lost'])}" for row in lost]
    out = [(n, each) for n, each in comparison["rounds"].items() if not each["kept"]]
    lines += ["", "## Rounds", ""]
    lines += [f"- r{n}{' (rerun of r' + each['rerun_of'] + ')' if each['rerun_of'] else ''}: "
              + ("kept" if each["kept"] else f"left out: {each['why']}")
              for n, each in comparison["rounds"].items()]
    if out and not any(each["kept"] for each in comparison["rounds"].values()):
        lines += ["", "Every round was left out: the timings have no result."]
    lines += ["", "## Every metric", "", f"| Metric | Unit | Bound | Verdict | {b} vs {a}, each round |",
              "|---|---|---|---|---|"]
    for row in comparison["metrics"]:
        lines.append(f"| {metric(row)} | {row['unit']} | {row['bound'] * 100:g}% | "
                     f"{row['verdict']} | {rounds_line(row, a, b)} |")
    return "\n".join(lines) + "\n"


def main(argv, environ=os.environ):
    parser = argparse.ArgumentParser(description=__doc__.strip().splitlines()[0])
    parser.add_argument("action", choices=["discarded", "compare"])
    parser.add_argument("--control", default=CONTROL)
    parser.add_argument("--json")
    parser.add_argument("results")
    parser.add_argument("a_name")
    parser.add_argument("b_name")
    args = parser.parse_args(argv)
    control = args.control or None
    bounds = Bounds.of(environ)
    if args.action == "discarded":
        for n, why in discarded(Sides.load(args.results, args.a_name, args.b_name), control,
                                bounds).items():
            print(n)
            print(f"r{n} left out: {why}", file=sys.stderr)
        return 0
    comparison = compare(args.results, args.a_name, args.b_name, control, bounds)
    if args.json:
        Path(args.json).write_text(json.dumps(comparison, indent=1) + "\n")
    print(markdown(comparison), end="")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
