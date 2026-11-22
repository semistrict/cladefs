#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""How many cells a host held (board 5586): the cells-per-host cases' results in a test-host.sh
results directory, as each case says them on its log (guest/tests/test_cases/src/test_cells_per_host.rs):
each step's `cells-per-host: step=... vms=...` line, and how it ended,
`cells-per-host: held=N limit=LIMIT`. Writes a Markdown summary to standard output: each case's VMs
held, the limit that stopped it, and its last held step's memory per VM and probes' p99; then each
case's steps. With --json, the same as JSON.

It fails if a case said no end, as one that failed doesn't, or held fewer VMs than its floor:
FLOORS', from the recorded measurement (docs/measurements), or --floor CASE=N's.

Usage: scripts/cells-per-host.py [--json PATH] [--floor CASE=N]... RESULTS
"""

import argparse
import json
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from guest_results import case_lines, case_logs  # noqa: E402

CASES = ("cells-per-host", "cells-per-host-dax")

# The fewest VMs each case may hold, by case: none until a run on the test host is recorded
# (docs/measurements), then that run's with a margin.
FLOORS = {}

PREFIX = "cells-per-host: "
FIELD = re.compile(r"(\w+)=(\S+)")

# The columns of a step, in the order the summary says them.
COLUMNS = ("step", "vms", "added", "room_mib", "per_vm_mib", "ready_s", "boot_p99_ms",
           "tick_p99_ms", "echo_p99_ms", "cat_p99_ms", "fsync_p99_ms", "past")

# Where a VM's memory goes, as a step says it, each the mean of its VMs, in MiB (board 5649): the
# guest's (its RAM its kernel never had, not available, slab, stacks, page tables, page cache, its
# window's struct pages), its VM process's (share, anonymous, shared, page tables), KVM's page
# tables for it, and the daemon's anonymous memory and extents' pages over the VMs.
BREAKDOWN = ("guest_reserved_mib", "guest_used_mib", "guest_slab_mib", "guest_stack_mib",
             "guest_pt_mib", "guest_cached_mib", "guest_memmap_mib", "vmm_pss_mib", "vmm_anon_mib",
             "vmm_shmem_mib", "vmm_pt_mib", "kvm_pt_mib", "daemon_anon_mib", "daemon_cache_mib")


def number(text):
    """`text` as an int, or a float, or as it is."""
    for kind in (int, float):
        try:
            return kind(text)
        except ValueError:
            pass
    return text


def parse(lines):
    """A case's steps and how it ended, of its log's `lines`: (steps, end), each step its fields
    by name, the end its `held` and `limit`, or None if it said none."""
    steps, end = [], None
    for line in lines:
        at = line.find(PREFIX)
        if at < 0:
            continue
        fields = {name: number(value) for name, value in FIELD.findall(line[at + len(PREFIX):])}
        if "step" in fields:
            steps.append(fields)
        elif "held" in fields and "limit" in fields:
            end = {"held": fields["held"], "limit": fields["limit"]}
    return steps, end


def summary(steps, end):
    """What a case's `steps` and `end` say: its end, with its last held step's memory per VM and
    probes' p99, that of the step whose VMs are those it held and that was past no budget."""
    if end is None:
        return None
    found = dict(end)
    held = [step for step in steps if step.get("vms") == end["held"] and "past" not in step]
    if held:
        last = held[-1]
        found.update({name: last[name] for name in COLUMNS[3:-1] + BREAKDOWN if name in last})
    return found


def markdown(cases):
    """The summary of `cases`, each by name its (summary, steps), as Markdown."""
    out = ["| case | VMs held | limit | MiB per VM | boot p99 ms | tick p99 ms | echo p99 ms "
           "| cat p99 ms | fsync p99 ms |", "|" + " --- |" * 9]
    for case, (found, _) in cases.items():
        if found is None:
            out.append(f"| {case} | — | no end said | | | | | | |")
            continue
        cells = [found.get(name, "") for name in ("per_vm_mib", "boot_p99_ms", "tick_p99_ms",
                                                  "echo_p99_ms", "cat_p99_ms", "fsync_p99_ms")]
        out.append(f"| {case} | {found['held']} | {found['limit']} | "
                   + " | ".join(str(cell) for cell in cells) + " |")
    for case, (_, steps) in cases.items():
        out += ["", f"## {case}", "", "| " + " | ".join(COLUMNS) + " |", "|" + " --- |" * len(COLUMNS)]
        for step in steps:
            out.append("| " + " | ".join(str(step.get(name, "")) for name in COLUMNS) + " |")
        if any(name in step for step in steps for name in BREAKDOWN):
            columns = ("step", "vms") + BREAKDOWN
            out += ["", f"### {case}: MiB per VM", "", "| " + " | ".join(columns) + " |",
                    "|" + " --- |" * len(columns)]
            for step in steps:
                out.append("| " + " | ".join(str(step.get(name, "")) for name in columns) + " |")
    return "\n".join(out) + "\n"


def judge(cases, floors):
    """The failures of `cases` against `floors`: each case that said no end, or held fewer VMs
    than its floor."""
    failed = []
    for case, (found, _) in cases.items():
        if found is None:
            failed.append(f"{case}: said no end: it failed or never ran")
        elif found["held"] < floors.get(case, 0):
            failed.append(f"{case}: held {found['held']} VMs, fewer than its floor of {floors[case]}")
    return failed


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--json", type=Path)
    parser.add_argument("--floor", action="append", default=[], metavar="CASE=N")
    parser.add_argument("results", type=Path)
    args = parser.parse_args(argv)
    floors = dict(FLOORS)
    for given in args.floor:
        case, _, count = given.partition("=")
        if case not in CASES or not count.isdigit():
            parser.error(f"--floor is CASE=N, CASE one of {', '.join(CASES)}: not {given}")
        floors[case] = int(count)
    cases = {}
    for case in CASES:
        logs = case_logs(args.results, case)
        steps, end = parse(case_lines(logs[-1])) if logs else ([], None)
        cases[case] = (summary(steps, end), steps)
    sys.stdout.write(markdown(cases))
    failed = judge(cases, floors)
    if args.json:
        args.json.write_text(json.dumps(
            {"cases": {case: {"summary": found, "steps": steps}
                       for case, (found, steps) in cases.items()},
             "floors": floors, "failed": failed}, indent=1) + "\n")
    for failure in failed:
        print(f"cells-per-host: {failure}", file=sys.stderr)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
