#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Checks that what the tests leave out says why, and that a why naming a task is still open.

What a test or suite leaves out outlives its reason unless something looks (TASK-149.20: a guest
check covering a data loss bug had been switched off on DAX). So each of these says why:

- An ignored Rust test of cladefs's: `#[ignore = "why"]`, not a bare `#[ignore]`, nor a cfg_attr
  ignoring it with none. A file of Fuchsia's (its copyright Fuchsia's) keeps upstream's ways.
- A line of the open bugs' lists of failures the runs let be (scripts/pjdfstest-fuse-known,
  guest/sim/known-failures, crates/fxfs-posix/known-panics): its task first, which is open.
- xfstests' known failures (guest/tests/test_cases/src/test_xfstests.rs, KNOWN_FAILURES): each
  under a comment saying why, a task it names open.
- pjdfstest's skipped tests on the guest's Fxfs (test_pjdfstest.rs, FXFS_SKIPPED): a task their why
  names open.
- A hand mutant no test can kill (guest/mutants, `Killer: none`): its header says why, equivalent
  or out of every oracle's reach, as guest/mutants/README.md has it.
- A test .config/nextest.toml names (`test(=path::name)`, its slow tests', the unit profile leaves
  out): one there is, a `fn name` of cladefs's. A test deleted or renamed left its filter behind,
  matching nothing, and its successor ran where it was meant not to (board 10525).

A task is cited as `board 812`, or, for a task from Backlog.md, as its TASK-N (its label `task` on
the board). It is open while the board has it waiting, ready, claimed or blocked: completed,
canceled and failed tasks aren't. The states are Hotbed's board's, read with `hotbed board` where
`hotbed` is installed and configured on the user's machines; CI's runners can't reach the board, so
there only that each names a task is checked (the user's call, 2026-11-21). A configured board that
can't be read fails it.
scripts/test.sh's scripts part runs this, as CI does. Exits 1, saying each, if anything breaks these.

Usage: scripts/check-exclusions.py
"""

import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# The lists of open bugs' failures, a line each: the task, then the failure.
KNOWN_LISTS = [
    "scripts/pjdfstest-fuse-known",
    "guest/sim/known-failures",
    "crates/fxfs-posix/known-panics",
]
XFSTESTS = "guest/tests/test_cases/src/test_xfstests.rs"
PJDFSTEST = "guest/tests/test_cases/src/test_pjdfstest.rs"
MUTANTS = "guest/mutants"
NEXTEST = ".config/nextest.toml"
# Where cladefs's Rust tests are: not vendor/, Firecracker's and the guest kernel's sources.
RUST = ["crates", "guest/tests", "guest/rust"]

TASK = re.compile(r"\b(?:TASK-\d+(?:\.\d+)*|board \d+)\b")
# The board's project and the states of a task that is no longer open.
PROJECT = "cladefs"
CLOSED = ("completed", "canceled", "failed")
PAGE = 500
BARE_IGNORE = re.compile(r"#\[ignore\]|#\[cfg_attr\([^]]*,\s*ignore\s*\)\]")
FUCHSIA = re.compile(r"Copyright \d{4} The Fuchsia Authors")
NEXTEST_TEST = re.compile(r"test\(=([\w:]+)\)")
TEST_FN = re.compile(r"\bfn (\w+)")


class BoardNotInitialized(Exception):
    """Raised when `hotbed` is installed on a host with no controller configured."""


def hotbed_board(*args, environ=os.environ):
    """What `hotbed board` answers ARGS, as JSON `data`; a failure ends the check."""
    done = subprocess.run(["hotbed", "board", *args, "--json"], capture_output=True, text=True,
                          env=environ)
    if done.returncode:
        if not environ.get("HOTBED_CONTROLLER"):
            try:
                err = json.loads(done.stdout).get("error", {})
            except ValueError:
                err = {}
            if err.get("code") == "NOT_INITIALIZED":
                raise BoardNotInitialized()
        detail = (done.stderr or done.stdout).strip()
        sys.exit(f"hotbed board {' '.join(args)} failed: {detail}")
    return json.loads(done.stdout)["data"]


def _task_state(raw):
    return raw.removeprefix("BOARD_TASK_STATE_").lower()


def task_states(environ=os.environ, ask=None):
    """Each task's state on the board, by how it is cited (`board 812`, and a task from Backlog.md
    by its TASK-N too); None on CI or where hotbed isn't configured, so that states go unchecked."""
    if environ.get("CI"):
        return None
    if not environ.get("HOTBED_CONTROLLER") and not shutil.which("hotbed",
                                                               path=environ.get("PATH", "")):
        return None
    if ask is None:
        ask = lambda *args: hotbed_board(*args, environ=environ)
    states, after = {}, []
    while True:
        try:
            page = ask("task", "ls", "-p", PROJECT, "--all", "--limit", str(PAGE), *after)
        except BoardNotInitialized:
            return None
        tasks = page.get("tasks") or []
        for each in tasks:
            message = each["message"]
            state = _task_state(message["task"]["state"])
            states[f"board {message['id']}"] = state
            labels = message.get("labels") or {}
            if "task" in labels:
                states[labels["task"]] = state
        if len(tasks) < PAGE:
            return states
        after = ["--after", page["cursor"]]


def closed(text, states):
    """The tasks `text` names that aren't open: closed or unknown, each with why; none when STATES
    is None (unchecked)."""
    if states is None:
        return []
    found = []
    for task in TASK.findall(text):
        state = states.get(task)
        if state is None:
            found.append(f"{task}, no such task")
        elif state in CLOSED:
            found.append(f"{task}, {state}")
    return found


def bare_ignores(path, text):
    """The lines of `text`, a Rust file at `path`, that ignore a test without saying why."""
    if FUCHSIA.search("\n".join(text.splitlines()[:5])):
        return []
    return [
        f"{path}:{number}: an ignored test says no why: #[ignore = \"why\"]"
        for number, line in enumerate(text.splitlines(), 1)
        if BARE_IGNORE.search(line)
    ]


def known_list(path, text, states):
    """What is wrong with `text`, a list of open bugs' failures at `path`: lines naming no task
    first, or a task not open."""
    problems = []
    for number, line in enumerate(text.splitlines(), 1):
        if not line.strip() or line.startswith("#"):
            continue
        first = TASK.match(line)
        if not first:
            problems.append(f"{path}:{number}: names no task first: {line}")
            continue
        for task in closed(first[0], states):
            problems.append(f"{path}:{number}: its task isn't open ({task}): drop the line")
    return problems


def xfstests_known(path, text, states):
    """What is wrong with KNOWN_FAILURES in `text`, xfstests' case at `path`: a failure under no
    comment saying why, or a comment naming a task not open."""
    match = re.search(r"const KNOWN_FAILURES: \[&str; \d+\] = \[(.*?)\n\s*\];", text, re.S)
    if not match:
        return [f"{path}: no KNOWN_FAILURES"]
    start = text[: match.start(1)].count("\n") + 1
    problems, why = [], None
    for offset, line in enumerate(match.group(1).splitlines()):
        number, line = start + offset, line.strip()
        if line.startswith("//"):
            why = line if why is None else f"{why} {line}"
            for task in closed(line, states):
                problems.append(f"{path}:{number}: a known failure's task isn't open ({task})")
        elif line.startswith('"'):
            if why is None:
                problems.append(f"{path}:{number}: {line.rstrip(',')} is under no comment saying why")
        elif not line:
            why = None
    return problems


def pjdfstest_skipped(path, text, states):
    """What is wrong with FXFS_SKIPPED in `text`: a why naming a task not open."""
    match = re.search(r"FXFS_SKIPPED: &\[\(&str, &str\)\] = &\[(.*?)\];", text, re.S)
    if not match:
        return [f"{path}: no FXFS_SKIPPED"]
    return [f"{path}: a skipped test's task isn't open ({task})" for task in closed(match.group(1), states)]


def unkillable(path, text):
    """What is wrong with the hand mutant at `path`, `text`: no killer, its header saying no why."""
    header = text.split("\n---", 1)[0]
    if not re.search(r"^# Killer: none\s*$", header, re.M):
        return []
    if re.search(r"quivalent|oracle", header):
        return []
    return [f"{path}: Killer: none, and its header says no why (equivalent, or out of every oracle's reach)"]


def nextest_names(path, text, fns):
    """The tests `text`, nextest's configuration at `path`, names exactly that no `fn` of FNS, the
    names of cladefs's Rust functions, is."""
    return [
        f"{path}:{number}: names {name}, no test there is: drop it, or name what it became"
        for number, line in enumerate(text.splitlines(), 1)
        for name in NEXTEST_TEST.findall(line)
        if name.rsplit("::", 1)[-1] not in fns
    ]


def check(root, states):
    """Every problem of the tree at `root`, each a line, tasks' states as STATES has them (None:
    unchecked)."""
    problems, fns = [], set()
    for directory in RUST:
        for path in sorted((root / directory).rglob("*.rs")):
            if "target" in path.relative_to(root).parts:
                continue
            text = path.read_text(errors="replace")
            problems += bare_ignores(path.relative_to(root), text)
            fns.update(TEST_FN.findall(text))
    problems += nextest_names(NEXTEST, (root / NEXTEST).read_text(), fns)
    for name in KNOWN_LISTS:
        problems += known_list(name, (root / name).read_text(), states)
    problems += xfstests_known(XFSTESTS, (root / XFSTESTS).read_text(), states)
    problems += pjdfstest_skipped(PJDFSTEST, (root / PJDFSTEST).read_text(), states)
    for path in sorted((root / MUTANTS).glob("*.patch")):
        problems += unkillable(path.relative_to(root), path.read_text())
    return problems


def main():
    states = task_states()
    if states is None:
        print("exclusions: hotbed isn't configured here, so whether each task named is open goes unchecked")
    problems = check(ROOT, states)
    for problem in problems:
        print(problem, file=sys.stderr)
    print(f"exclusions: {len(problems)} saying no why, or why no longer")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
