#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""The full-stack simulation over its seeds (scripts/test.sh's madsim part, board 2912): the
simulated crates' tests/madsim.rs, each seed a process of its own (TASK-685.15), the regression
seeds first (crates/cladefs/tests/madsim-seeds, TASK-685.8), then COUNT fresh ones from FIRST.
The first fresh seed runs twice, its runs compared (`MADSIM_TEST_CHECK_DETERMINISM`): a seed that
does not replay is a failure as any other, as a simulation whose seed doesn't decide it finds
nothing it can show again (board 7583).

Every seed runs, whatever an earlier one found, all its tests (nextest's profile madsim, whose
JUnit report gives each failure's output). A failure crates/cladefs/tests/madsim-known lists, an
open bug's, by its test and the start of its panic's message, is said as known, with its board
task, and passes the run: the simulation stays in CI while the bugs it found are worked on. Any
other fails the run: another test's, another panic's, a hang, a crash. Each is said with the
command that replays it.

Usage: scripts/madsim-seeds.py --first FIRST --count COUNT [NEXTEST-ARG...]

Run as scripts/test.sh runs it, with RUSTFLAGS="--cfg madsim" and the madsim build's own
CARGO_TARGET_DIR.
"""

import argparse
import os
import re
import subprocess
import sys
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REGRESSIONS = ROOT / "crates" / "cladefs" / "tests" / "madsim-seeds"
KNOWN = ROOT / "crates" / "cladefs" / "tests" / "madsim-known"
# The crates the simulation runs, each its tests/madsim.rs; a rule search, `*_needs_*`, runs its
# own seeds, once, apart (scripts/test.sh).
SIMULATED = ["-p", "node_local", "-p", "host_fs", "-p", "bucket_cell", "-p", "local_cell",
             "-p", "cladefs", "--test", "madsim", "-E", "not test(/_needs_/)"]
# The test binary madsim-known's tests are of.
BINARY = "cladefs::madsim"
PROFILE = "madsim"
# Where nextest writes the profile's JUnit report: its store, the workspace's target/nextest,
# whatever CARGO_TARGET_DIR says.
JUNIT = ROOT / "target" / "nextest" / PROFILE / "junit.xml"
# nextest's status when tests failed; any other but 0 is an error of the run itself.
TESTS_FAILED = 100


def regression_seeds(text):
    """The seeds crates/cladefs/tests/madsim-seeds lists: the first word of each line, what
    follows `#` a comment."""
    seeds = []
    for line in text.splitlines():
        words = line.split("#", 1)[0].split()
        if words:
            seeds.append(int(words[0]))
    return seeds


@dataclass(frozen=True)
class Known:
    """A failure of an open bug: of `test`, its panic's message starting as `panic` matches."""

    board: int
    test: str
    panic: re.Pattern

    def matches(self, test, message):
        return test == self.test and self.panic.match(message) is not None


def known(text):
    """The failures crates/cladefs/tests/madsim-known lists: `BOARD TEST PANIC` a line, PANIC a
    regular expression (Python's) the rest of the line, what follows a `#` starting a line a
    comment."""
    listed = []
    for number, line in enumerate(text.splitlines(), 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        parts = line.split(maxsplit=2)
        if len(parts) != 3 or not parts[0].isdigit():
            raise ValueError(f"madsim-known:{number}: not BOARD TEST PANIC: {line!r}")
        board, test, panic = parts
        listed.append(Known(int(board), test, re.compile(panic.strip())))
    return listed


ANSI = re.compile(r"\x1b\[[0-9;]*m")
PANIC = re.compile(r"panicked at [^\n]*:\n(.*?)(?:\nnote: |\Z)", re.DOTALL)


def panic_of(output):
    """The message of the first panic `output` says, if any."""
    found = PANIC.search(ANSI.sub("", output))
    return found.group(1).strip() if found else None


@dataclass(frozen=True)
class Failure:
    """A test's failure at a seed: its panic's message, if it panicked, else what nextest says of
    it (a hang ended, a crash); and the bug it is, if madsim-known lists it."""

    seed: int
    binary: str
    test: str
    message: str
    known: Known | None
    checked: bool = False

    def replay(self):
        check = " MADSIM_TEST_CHECK_DETERMINISM=1" if self.checked else ""
        return f"MADSIM_TEST_SEED={self.seed}{check} replays it"

    def __str__(self):
        said = f"seed {self.seed}: {self.binary} {self.test}: {self.message.splitlines()[0]}"
        if self.known:
            return f"{said}: known, board {self.known.board}"
        return f"{said}: {self.replay()}"


def failures(report, seed, listed, checked=False):
    """The failures of the JUnit report `report`, of the run at `seed`, its determinism checked
    if `checked`, each known as `listed` says, or not."""
    found = []
    for case in ET.fromstring(report).iter("testcase"):
        failed = case.find("failure")
        if failed is None:
            failed = case.find("error")
        if failed is None:
            continue
        binary, test = case.get("classname"), case.get("name")
        output = "\n".join(filter(None, (failed.text, case.findtext("system-out"),
                                         case.findtext("system-err"))))
        panic = panic_of(output)
        message = panic if panic is not None else (failed.get("message") or failed.get("type")
                                                   or "failed")
        bug = None
        if panic is not None and binary == BINARY:
            bug = next((entry for entry in listed if entry.matches(test, panic)), None)
        found.append(Failure(seed, binary, test, message, bug, checked))
    return found


def run(seed, args, checked=False):
    """The simulation's tests at `seed`, nextest given `args` too, each run twice and its runs
    compared if `checked`: their failures' JUnit report, or None if all passed. Exits as nextest
    did if it failed but for tests failing."""
    JUNIT.unlink(missing_ok=True)
    command = ["cargo", "nextest", "run", "--locked", "--manifest-path", str(ROOT / "Cargo.toml"),
               "--profile", PROFILE, *SIMULATED, *args]
    env = {**os.environ, "MADSIM_TEST_SEED": str(seed)}
    if checked:
        env["MADSIM_TEST_CHECK_DETERMINISM"] = "1"
    status = subprocess.run(command, env=env).returncode
    if status == 0:
        return None
    if status != TESTS_FAILED or not JUNIT.exists():
        sys.exit(f"madsim: seed {seed}: cargo nextest failed ({status}), not a test")
    return JUNIT.read_text()


def annotate(level, title, message):
    """`message` as a GitHub Actions annotation, in a workflow's run."""
    if os.environ.get("GITHUB_ACTIONS") == "true":
        message = message.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
        print(f"::{level} title={title}::{message}")


def summary(seeds, found):
    """What the run found, a line each, and whether it passes: no failure but known ones."""
    new = [failure for failure in found if not failure.known]
    by_board = {}
    for failure in found:
        if failure.known:
            by_board.setdefault(failure.known.board, set()).add(failure.seed)
    lines = [f"madsim: {len(seeds)} seeds, {len(new)} new failures, "
             f"{len(found) - len(new)} known"]
    for board, at in sorted(by_board.items()):
        lines.append(f"madsim: known, board {board}: seeds {', '.join(map(str, sorted(at)))}")
    lines += [f"madsim: new: {failure}" for failure in new]
    return lines, not new


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--first", type=int, required=True)
    parser.add_argument("--count", type=int, required=True)
    options, args = parser.parse_known_args(argv)
    listed = known(KNOWN.read_text())
    seeds = regression_seeds(REGRESSIONS.read_text())
    checked = len(seeds) if options.count > 0 else None
    seeds += range(options.first, options.first + options.count)
    found = []
    for at, seed in enumerate(seeds):
        report = run(seed, args, at == checked)
        if report is None:
            continue
        failed = failures(report, seed, listed, at == checked)
        if not failed:
            sys.exit(f"madsim: seed {seed}: tests failed, but the JUnit report names none")
        for failure in failed:
            print(f"madsim: {failure}", file=sys.stderr)
            if failure.known:
                annotate("warning", f"madsim: known, board {failure.known.board}", str(failure))
            else:
                annotate("error", "madsim: a new failure", str(failure))
        found += failed
    lines, passed = summary(seeds, found)
    print("\n".join(lines), file=sys.stderr)
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
