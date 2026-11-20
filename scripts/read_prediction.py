#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Compares predictions of a version's reads on traces of its boots (TASK-702.1).

A cold host's first boot of a version waits on its base a part at a time: the guest faults on a
page, the daemon fetches its part from the bucket, ~25 ms, and the guest faults on the next. What
each boot read is in the daemon's traces of its VMs' reads (`CLADEFSD_READ_TRACES`,
crates/cladefs/src/cladefsd/reads.rs): its `READ`s, its `MAP`s and its window's faults.
fxfs-guest-python-sandbox keeps one a sandbox, as `traces/NN-SANDBOX-WORKLOAD.reads`, in the order
they ran. This replays them in that order against each predictor, each predicting a boot's reads
from the boots before it, or from the boot's own reads so far, or both, and says per boot how much
of what it read in its first HORIZON seconds was predicted, how many MiB were fetched that it never
read, and what waiting the prediction would have left.

Reads are counted in units: a MiB of an extent, at offsets that are multiples of a MiB, as a part
of the bucket's objects is a MiB (extent_frame::PART), though an extent need not start at a part's.
Fxfs's extents are a MiB at most, so a unit is most often a whole extent. Only the units of extents
the boot's cell inherited count, not those of its own extents, which it wrote itself. A unit's size
is its share of its extent's length, as any trace gives it (a MiB where none does): the length the
daemon's index says, which a predictor may use.

Predictors:

- `none`: nothing fetched ahead: the daemon as it is.
- `next`: on the first read of an extent, the NEXT extents after it by sequence number fetched, as
  a file's extents are numbered in the order they were written.
- `first`: the first boot's units, kept for good: what the sandbox team's JuiceFS working set does.
- `union`: every unit any boot before read.
- `recent`: each unit scored by the share of the last KEEP boots that read it, each boot weighted by
  half again for each HALF_LIFE boots of its age; the units scoring THRESHOLD or more fetched,
  those read earliest first.
- `recent+next`: both.
- `oracle`: the boot's own units, known ahead: the bound.

The prefetch model: what a predictor names before the boot is queued as it opens, in order;
what `next` names, as the read that names it is made. CONCURRENCY fetches run at once, each
taking LATENCY. A unit read before its fetch finished waits for the rest of it; one never fetched
waits LATENCY, as a cold host's read of a part not in memory nor in the part cache does. The model
ignores what demand reads hold of the budget and what the daemon's read-ahead already finds: it
compares predictors, it does not predict wall time.

Usage: read_prediction.py [--horizon S] [--latency-ms MS] [--concurrency N] [--next N]
                          [--keep K] [--half-life B] [--threshold T] [--json] TRACES_DIR
"""

import argparse
import json
import math
import sys
from dataclasses import dataclass, field
from pathlib import Path

HEADER = "cladefs read trace 1"
MIB = 1 << 20
DIGITS = "0123456789abcdefghjkmnpqrstvwxyz"


def base32(text):
    """An ID as cladefs writes it in base 32 (crates/base32_id)."""
    value = 0
    for digit in text:
        value = value * 32 + DIGITS.index(digit)
    return value


@dataclass
class Read:
    at: float  # seconds since the store's opening
    kind: str
    extent: tuple  # (cell, sequence)
    extent_len: int
    offset: int
    len: int
    waited: float  # seconds


@dataclass
class Trace:
    name: str
    cell: int
    reads: list


def parse(name, text):
    """The trace of `name` whose text is `text`."""
    lines = text.splitlines()
    if not lines or lines[0] != HEADER:
        raise ValueError(f"{name}: not a read trace")
    said = lines[1].split(" ")
    if len(said) != 4 or said[0] != "cell" or said[2] != "opened":
        raise ValueError(f"{name}: a bad second line: {lines[1]!r}")
    reads = []
    for line in lines[2:]:
        at, kind, cell, sequence, extent_len, offset, length, waited = line.split(" ")
        reads.append(Read(
            at=int(at) / 1e6, kind=kind, extent=(base32(cell), base32(sequence)),
            extent_len=int(extent_len, 16), offset=int(offset, 16), len=int(length, 16),
            waited=int(waited) / 1e6))
    return Trace(name=name, cell=base32(said[1]), reads=reads)


def load(directory):
    """The traces in `directory`, in the order their names give."""
    return [parse(path.name, path.read_text())
            for path in sorted(Path(directory).glob("*.reads"))]


def units_of(read):
    """The units `read` touches."""
    first = read.offset // MIB
    last = (read.offset + max(read.len, 1) - 1) // MIB
    return [(read.extent, part) for part in range(first, last + 1)]


class Sizes:
    """The lengths of the extents the traces read, as they give them: each unit's size, and an
    extent's units."""

    def __init__(self, traces):
        self.lengths = {}
        for trace in traces:
            for read in trace.reads:
                if read.extent_len:
                    self.lengths[read.extent] = read.extent_len

    def units(self, extent):
        """`extent`'s units: its first alone if its length is unknown."""
        length = self.lengths.get(extent, MIB)
        return [(extent, part) for part in range(max(1, math.ceil(length / MIB)))]

    def size(self, unit):
        extent, part = unit
        length = self.lengths.get(extent, MIB)
        return max(0, min(MIB, length - part * MIB)) or MIB


@dataclass
class Boot:
    """What a boot read of what it inherited: each unit's first read within the horizon, and every
    unit it read at all."""
    name: str
    first: dict  # unit -> (at, waited)
    read: set = field(default_factory=set)

    def in_order(self):
        """Its units read within the horizon, first read first."""
        return sorted(self.first, key=lambda unit: (self.first[unit][0], unit))


def boot_of(trace, horizon):
    """What `trace`'s boot read of what it inherited, its first `horizon` seconds' first reads."""
    boot = Boot(name=trace.name, first={})
    for read in sorted(trace.reads, key=lambda read: read.at):
        if read.extent[0] == trace.cell:
            continue
        for unit in units_of(read):
            boot.read.add(unit)
            if read.at <= horizon and unit not in boot.first:
                boot.first[unit] = (read.at, read.waited)
    return boot


@dataclass
class Model:
    latency: float
    concurrency: int


def finishes(queued, model):
    """When each unit of `queued`, (time, unit) in the order queued, is fetched: `concurrency`
    fetches at once, each `latency` long, first queued first; a unit queued twice fetched once."""
    free = [0.0] * model.concurrency
    done = {}
    for at, unit in sorted(queued, key=lambda queued: queued[0]):
        if unit in done:
            continue
        slot = min(range(len(free)), key=free.__getitem__)
        start = max(at, free[slot])
        free[slot] = start + model.latency
        done[unit] = free[slot]
    return done


class Predictor:
    """A predictor: what it fetches ahead of a boot, from the boots before it; what it fetches as
    the boot reads, from its reads so far; told each boot after it ran."""
    name = "none"

    def ahead(self):
        return []

    def along(self, boot, sizes):
        return []

    def ran(self, boot):
        pass


class Next(Predictor):
    def __init__(self, count):
        self.count = count
        self.name = "next"

    def along(self, boot, sizes):
        queued, seen = [], set()
        for unit in boot.in_order():
            extent = unit[0]
            if extent in seen:
                continue
            seen.add(extent)
            at = boot.first[unit][0]
            cell, sequence = extent
            for after in range(1, self.count + 1):
                queued += [(at, other) for other in sizes.units((cell, sequence + after))]
        return queued


class First(Predictor):
    name = "first"

    def __init__(self):
        self.units = None

    def ahead(self):
        return self.units or []

    def ran(self, boot):
        if self.units is None:
            self.units = boot.in_order()


class Recent(Predictor):
    def __init__(self, keep, half_life, threshold, name="recent"):
        self.keep, self.half_life, self.threshold, self.name = keep, half_life, threshold, name
        self.boots = []

    def ahead(self):
        boots = self.boots[-self.keep:]
        weights = [0.5 ** ((len(boots) - 1 - age) / self.half_life) for age in range(len(boots))]
        total = sum(weights)
        scores, when = {}, {}
        for weight, boot in zip(weights, boots):
            for unit, (at, _) in boot.first.items():
                scores[unit] = scores.get(unit, 0.0) + weight / total
                when.setdefault(unit, []).append(at)
        chosen = [unit for unit, score in scores.items() if score >= self.threshold - 1e-12]
        return sorted(chosen, key=lambda unit: (sum(when[unit]) / len(when[unit]), unit))

    def ran(self, boot):
        self.boots.append(boot)


def union():
    """Every unit any boot before read: no boot forgotten, any one read enough."""
    return Recent(keep=sys.maxsize, half_life=math.inf, threshold=0.0, name="union")


class Both(Predictor):
    """A predictor from history's prediction and one from the boot's reads, together."""

    def __init__(self, history, along):
        self.history, self.online = history, along
        self.name = f"{history.name}+{along.name}"

    def ahead(self):
        return self.history.ahead()

    def along(self, boot, sizes):
        return self.online.along(boot, sizes)

    def ran(self, boot):
        self.history.ran(boot)


class Oracle(Predictor):
    name = "oracle"

    def __init__(self):
        self.next = None

    def ahead(self):
        return self.next.in_order()


@dataclass
class Outcome:
    """How a predictor did on a boot: the units it read in the horizon, how many it predicted,
    the MiB fetched that the boot never read, and the waiting the model leaves, in seconds,
    against the waiting with no prediction."""
    boot: str
    units: int
    predicted: int
    unused_mib: float
    waited: float
    unpredicted_wait: float

    @property
    def recall(self):
        return self.predicted / self.units if self.units else 1.0


def replay(boots, predictor, model, sizes):
    """`predictor`'s outcome on each of `boots`, in turn."""
    outcomes = []
    for boot in boots:
        if isinstance(predictor, Oracle):
            predictor.next = boot
        queued = [(0.0, unit) for unit in predictor.ahead()]
        queued += predictor.along(boot, sizes)
        done = finishes(queued, model)
        waited = 0.0
        for unit, (at, _) in boot.first.items():
            finished = done.get(unit)
            waited += (model.latency if finished is None
                       else min(model.latency, max(0.0, finished - at)))
        unused = sum(sizes.size(unit) for unit in done if unit not in boot.read) / MIB
        outcomes.append(Outcome(
            boot=boot.name, units=len(boot.first),
            predicted=sum(unit in done for unit in boot.first), unused_mib=unused,
            waited=waited, unpredicted_wait=len(boot.first) * model.latency))
        predictor.ran(boot)
    return outcomes


def predictors(params):
    def recent():
        return Recent(params.keep, params.half_life, params.threshold)
    return [Predictor(), Next(params.next), First(), union(), recent(),
            Both(recent(), Next(params.next)), Oracle()]


@dataclass
class Params:
    next: int = 4
    keep: int = 8
    half_life: float = 2.0
    threshold: float = 0.25


def report(boots, model, sizes, params):
    """Each predictor's outcomes on `boots`, by name, in the order they are compared."""
    return {predictor.name: replay(boots, predictor, model, sizes)
            for predictor in predictors(params)}


def text(results):
    """The comparison as a table: a row a predictor and boot, then a predictor's means."""
    rows = ["predictor        boot                              units recall unused_mib waited_s"]
    for name, outcomes in results.items():
        for outcome in outcomes:
            rows.append(f"{name:<16} {outcome.boot:<33} {outcome.units:>5} "
                        f"{outcome.recall:>6.2f} {outcome.unused_mib:>10.1f} "
                        f"{outcome.waited:>8.2f}")
    rows.append("")
    rows.append("predictor        mean_recall  unused_mib  waited_s (of none's)")
    for name, outcomes in results.items():
        later = outcomes[1:] or outcomes
        recall = sum(outcome.recall for outcome in later) / len(later)
        unused = sum(outcome.unused_mib for outcome in later) / len(later)
        waited = sum(outcome.waited for outcome in later)
        none = sum(outcome.unpredicted_wait for outcome in later)
        rows.append(f"{name:<16} {recall:>11.2f} {unused:>11.1f} {waited:>9.2f} ({none:.2f})")
    rows.append("Means are of every boot but the first, which no history predicts.")
    return "\n".join(rows)


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("traces", type=Path)
    parser.add_argument("--horizon", type=float, default=10.0)
    parser.add_argument("--latency-ms", type=float, default=30.0)
    parser.add_argument("--concurrency", type=int, default=32)
    parser.add_argument("--next", type=int, default=Params.next)
    parser.add_argument("--keep", type=int, default=Params.keep)
    parser.add_argument("--half-life", type=float, default=Params.half_life)
    parser.add_argument("--threshold", type=float, default=Params.threshold)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    traces = load(args.traces)
    if not traces:
        sys.exit(f"no traces (*.reads) in {args.traces}")
    boots = [boot_of(trace, args.horizon) for trace in traces]
    model = Model(latency=args.latency_ms / 1000, concurrency=args.concurrency)
    params = Params(args.next, args.keep, args.half_life, args.threshold)
    results = report(boots, model, Sizes(traces), params)
    if args.json:
        print(json.dumps({name: [outcome.__dict__ | {"recall": outcome.recall}
                                 for outcome in outcomes]
                          for name, outcomes in results.items()}, indent=1))
    else:
        print(text(results))


if __name__ == "__main__":
    main(sys.argv[1:])
