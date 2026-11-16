#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""The guest tests' results as data (TASK-160.31): what scripts/test-e2e.sh's runs write, and what
the scripts that drive them read in place of grepping logs.

A run's results directory (test-e2e.sh's CLADEFS_E2E_RESULTS) holds:

- run.json: the run's commit (scripts/build-commit.sh), its knobs, every CLADEFS_*, CLADEFSD_* and PJDFSTEST_*
  variable it was given but those that say where it writes (WHERE), its repeats and its cases;
- CASE/repeat-K.json: each repeat's outcome, as the runner writes it (`runner test --outcome`):
  pass, fail, timeout, skip or report, why, the checks failed, what it took of its limit, the commit, the
  knobs, its own among them (CLADEFS_SEED), and the command that replays it;
- summary.json: each case's outcome, its last repeat's (`unrun` before it starts, `running` between
  repeats), the cases that failed, and the run's exit status once it ended (null before), rewritten
  as each repeat ends; and what each extent daemon downloaded against what it read (`daemons`, a
  case's; `daemon`, the run's), from its stats file (CLADEFSD_STATS: the run's
  extentd-stats.txt, a case's daemon's NAME-stats.txt in its directory).

    guest_results.py start RESULTS REPEATS CASE...  (test-e2e.sh, before its first case)
    guest_results.py ended RESULTS CASE             (after each repeat of a case)
    guest_results.py finish RESULTS STATUS          (as it exits)
    guest_results.py report RESULTS                 how each case went; exits 1 unless the run passed
"""

import json
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# The variables that say where a run writes, or where its kernel is, not how it runs: not knobs.
# CLADEFS_GUEST_KERNEL is one only where CLADEFS_GUEST_KERNEL_BUILD says how to build it, as
# scripts/lib/test-linux-host.sh does.
WHERE = {"CLADEFS_E2E_RESULTS", "CLADEFS_BENCH_RESULTS", "CLADEFS_COVERAGE"}
FAILED = {"fail", "timeout"}


def knobs(environment):
    """The knobs of a run given `environment`."""
    where = WHERE | ({"CLADEFS_GUEST_KERNEL"} if environment.get("CLADEFS_GUEST_KERNEL_BUILD") else set())
    return {name: value for name, value in sorted(environment.items())
            if name.startswith(("CLADEFS_", "CLADEFSD_", "PJDFSTEST_")) and name not in where}


def write(path, value):
    """Writes `value` to `path` as JSON, whole or not at all, for readers as it changes."""
    staged = path.with_name(path.name + ".new")
    staged.write_text(json.dumps(value, indent=1) + "\n")
    staged.replace(path)


def repeats_of(results, case):
    """The outcomes of `case`'s repeats, in their order."""
    files = (results / case).glob("repeat-*.json")
    numbered = sorted((int(re.fullmatch(r"repeat-(\d+)\.json", f.name).group(1)), f) for f in files)
    return [json.loads(f.read_text()) for _, f in numbered]


def daemon_bytes(path):
    """What the extent daemon whose stats file is `path` downloaded from its bucket against what it
    read, as its last counts say (`object-store:`'s `down`, `daemon:`'s `fetched-bytes`, page-ins of
    the bucket's data through its part cache, and `page-in-bytes`, of its store's): None if it
    says none (TASK-165.12)."""
    last = {}
    for line in path.read_text(errors="replace").splitlines():
        kind, _, counts = line.partition(": ")
        if kind in ("object-store", "daemon"):
            last[kind] = dict(word.split("=", 1) for word in counts.split() if "=" in word)
    try:
        return {"daemon": path.name.removesuffix("-stats.txt"),
                "downloaded": int(last["object-store"]["down"]),
                "bucket_read": int(last["daemon"]["fetched-bytes"]),
                "store_read": int(last["daemon"]["page-in-bytes"])}
    except (KeyError, ValueError):
        return None


def daemons_of(directory):
    """What each extent daemon whose stats file is in `directory`, or below it, downloaded against
    what it read ([`daemon_bytes`])."""
    found = (daemon_bytes(path) for path in sorted(directory.rglob("*-stats.txt")))
    return [each for each in found if each]


def case_summary(results, case, repeats):
    """How `case` went, from its repeats' outcomes, and what its extent daemons downloaded and
    read."""
    ran = repeats_of(results, case)
    if not ran:
        return {"case": case, "outcome": "unrun", "repeat": 0, "repeats_passed": 0}
    last = ran[-1]
    outcome = last["outcome"]
    if outcome in ("pass", "report") and last["repeat"] < repeats:
        outcome = "running"
    return {
        "case": case,
        "outcome": outcome,
        "repeat": last["repeat"],
        "repeats_passed": sum(each["outcome"] in ("pass", "report") for each in ran),
        "failed_checks": last["failed_checks"],
        "reason": last["reason"],
        "replay": last["replay"],
        "daemons": daemons_of(results / case),
    }


def summarize(results, status=None):
    """Rewrites `results`' summary.json, of its repeats so far, and the exit status `status`."""
    run = json.loads((results / "run.json").read_text())
    cases = [case_summary(results, case, run["repeats"]) for case in run["cases"]]
    stats = results / "extentd-stats.txt"
    summary = {
        "commit": run["commit"],
        "repeats": run["repeats"],
        "status": status,
        "failed": [each["case"] for each in cases if each["outcome"] in FAILED],
        "cases": cases,
        "daemon": daemon_bytes(stats) if stats.exists() else None,
        "knobs": run["knobs"],
    }
    write(results / "summary.json", summary)
    return summary


def start(results, repeats, cases):
    commit = subprocess.run([ROOT / "scripts" / "build-commit.sh", ROOT], check=True,
                            capture_output=True, text=True).stdout.strip()
    results.mkdir(parents=True, exist_ok=True)
    write(results / "run.json", {"commit": commit, "knobs": knobs(os.environ), "repeats": repeats,
                                 "cases": cases})
    summarize(results)


def ended(results, case):
    summarize(results)


def finish(results, status):
    summarize(results, status)


def load(results):
    """`results`' summary, or None if it has none: its run never started."""
    path = Path(results) / "summary.json"
    return json.loads(path.read_text()) if path.exists() else None


def size(count):
    """`count` bytes, in SI units."""
    for unit, scale in (("GB", 10**9), ("MB", 10**6), ("kB", 10**3)):
        if count >= scale:
            return f"{count / scale:.1f} {unit}"
    return f"{count} B"


def said(daemon):
    """What `daemon` downloaded against what it read ([`daemon_bytes`]), as a line says it."""
    return (f"{daemon['daemon']}: downloaded {size(daemon['downloaded'])} for "
            f"{size(daemon['bucket_read'])} read of its bucket's, "
            f"{size(daemon['store_read'])} of its store's")


def report(summary):
    """How each case of `summary` went, a line each, a replay's under a failure's, and what each of
    its extent daemons downloaded against what it read; then the run's daemon's."""
    repeats = summary["repeats"]
    lines = []
    for each in summary["cases"]:
        case, outcome, repeat = each["case"], each["outcome"], each["repeat"]
        if outcome in ("pass", "report"):
            lines.append(f"{case}: passed" + (f" {repeats} repeats" if repeats > 1 else ""))
        elif outcome in FAILED:
            failed = "timed out" if outcome == "timeout" else "failed"
            checks = each["failed_checks"]
            lines.append(f"{case}: {failed} in repeat {repeat} of {repeats}"
                         + (f", checks {', '.join(checks)}" if checks else ""))
            lines.append(f"  replay: {each['replay']}")
        elif outcome == "skip":
            lines.append(f"{case}: skipped: {each['reason']}")
        elif outcome == "running":
            lines.append(f"{case}: stopped after repeat {repeat} of {repeats}, which passed")
        else:
            lines.append(f"{case}: not run")
        lines.extend(f"  {said(daemon)}" for daemon in each.get("daemons", []))
    if summary.get("daemon"):
        lines.append(f"the run's {said(summary['daemon'])}")
    status = summary["status"]
    lines.append("the run did not finish" if status is None else f"the run ended with {status}")
    return lines


def main(argv):
    command, args = argv[0] if argv else "", argv[1:]
    if command == "start" and len(args) >= 3:
        start(Path(args[0]), int(args[1]), args[2:])
    elif command == "ended" and len(args) == 2:
        ended(Path(args[0]), args[1])
    elif command == "finish" and len(args) == 2:
        finish(Path(args[0]), int(args[1]))
    elif command == "report" and len(args) == 1:
        summary = load(args[0])
        if summary is None:
            print(f"{args[0]}: no summary: the guest tests never started")
            return 1
        print("\n".join(report(summary)))
        return 0 if summary["status"] == 0 else 1
    else:
        sys.exit(__doc__)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
