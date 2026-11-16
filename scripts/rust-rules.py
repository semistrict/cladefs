#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rules from the host crates' past bugs (TASK-444.6), as scripts/cocci-guest.py has the guest's:
each a shape a bug had, checked over the crates' Rust, so that the next instance is found as it is
written. Syntactic, in seconds, no build: what needs types is clippy's (clippy.toml's
disallowed-methods, TASK-98).

    scripts/rust-rules.py [PATH...]
    scripts/rust-rules.py --mutants

checks PATH, files or directories, or crates/. Any match fails, printed as FILE:LINE: RULE: WHY.

--mutants checks the rules instead. A rule names the mutants whose bugs it finds (MUTANTS below):
each, put back in a copy of the files it changes, it must match. And every mutant of the crates
(crates/*/mutants, crates/*/loom-mutants) is named by a rule or in scripts/rust-rules/unruled, with
why no rule finds its kind.
"""

import re
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
UNRULED = ROOT / "scripts/rust-rules/unruled"


def mutant_patches():
    """Every mutant of the crates, by name."""
    patterns = ("crates/*/mutants/*.patch", "crates/*/loom-mutants/*.patch")
    return {patch.stem: patch for pattern in patterns for patch in sorted(ROOT.glob(pattern))}


@dataclass
class Match:
    path: Path
    line: int
    rule: str
    why: str

    def __str__(self):
        return f"{self.path}:{self.line}: {self.rule}: {self.why}"


# An atomic's operation: what it is called on (a field or static), the operation, and where its
# arguments begin.
ATOMIC_CALL = re.compile(r"(\w+)\s*\.\s*(load|store|swap|fetch_\w+|compare_exchange(?:_weak)?|"
                         r"fetch_update|try_update)\s*\(")
ORDERING = re.compile(r"Ordering::(Relaxed|Acquire|Release|AcqRel|SeqCst)")
# How far above a Relaxed operation a comment may say why: its function's doc comment, or the
# comment over its statement.
REASON_LINES = 6


def arguments(text, start):
    """The text of a call's arguments, from `start`, just past its open parenthesis."""
    depth, at = 1, start
    while at < len(text) and depth:
        depth += {"(": 1, ")": -1}.get(text[at], 0)
        at += 1
    return text[start:at - 1]


def mixed_orderings(path, text):
    """An atomic with operations Relaxed and operations ordered, in one file: as loom's mutants
    weaken one operation of a synchronizing atomic to Relaxed (TASK-444.2), which no test but a loom
    model finds. An atomic is a counter, all Relaxed, or a synchronizer, none: a Relaxed operation
    of a synchronizer says why in a comment naming Relaxed, on its line or the few above. Atomics
    are known by name, a file's operations on fields or statics of one name taken for one atomic's.
    """
    lines = text.splitlines()
    atomics = {}
    for call in ATOMIC_CALL.finditer(text):
        orders = set(ORDERING.findall(arguments(text, call.end())))
        if orders:
            line = text.count("\n", 0, call.start()) + 1
            atomics.setdefault(call[1], []).append((line, orders))
    for name, operations in atomics.items():
        if not any(orders - {"Relaxed"} for _, orders in operations):
            continue
        for line, orders in operations:
            if "Relaxed" not in orders:
                continue
            above = lines[max(0, line - 1 - REASON_LINES):line]
            if any("//" in said and "Relaxed" in said.split("//", 1)[1] for said in above):
                continue
            yield Match(path, line, "mixed-orderings",
                        f"`{name}` is Relaxed here and ordered elsewhere in this file, and no "
                        "comment says why it may be Relaxed here")


def pattern_rule(name, pattern, why):
    """A rule matching a line's code, comments aside."""
    compiled = re.compile(pattern)

    def rule(path, text):
        for number, line in enumerate(text.splitlines(), 1):
            if compiled.search(line.split("//", 1)[0]):
                yield Match(path, number, name, why)

    rule.__name__ = name
    return rule


# Each rule, and the mutants of the bugs it is from.
RULES = [
    (mixed_orderings, [
        "latest-listens-relaxed", "latest-rises-relaxed", "occurrence-happens-relaxed",
        "occurrence-listens-relaxed", "answers-release-no-capacity", "capacities-read-relaxed",
    ]),
    (pattern_rule(
        "unbounded-semaphore", r"Semaphore::MAX_PERMITS",
        "a semaphore of every permit bounds nothing: as page-ins of every fault at once, the "
        "daemon's memory past its budget (TASK-149.17)"),
     ["page-ins-of-every-fault-at-once"]),
    (pattern_rule(
        "bytes-copied", r"\bBytes::copy_from_slice\b",
        "copies a buffer Bytes::clone would share: as each upload's retry copied its segment "
        "(TASK-330); copy only what must outlive a borrow, and say so with to_vec"),
     ["uploads-copied-each-try"]),
]


def rust_files(path):
    """The Rust files of `path`, a file or a directory, targets' and vendored code aside."""
    path = Path(path)
    if path.is_file():
        return [path]
    return sorted(file for file in path.rglob("*.rs")
                  if not {"target", "vendor", "node_modules"} & set(file.relative_to(path).parts))


def check(paths):
    """Every rule's matches in `paths`."""
    return [match for path in paths for file in rust_files(path)
            for rule, _ in RULES for match in rule(file, file.read_text(errors="replace"))]


def unruled():
    """scripts/rust-rules/unruled: each mutant no rule finds, and why."""
    reasons = {}
    for line in UNRULED.read_text().splitlines():
        if line and not line.startswith("#"):
            name, _, why = line.partition(": ")
            reasons[name] = why
    return reasons


def with_mutant(rule, patch):
    """What `rule` matches in a copy of the files `patch` changes, the patch in, or why it can't."""
    touched = re.findall(r"^\+\+\+ b/(\S+)", patch.read_text(), re.MULTILINE)
    with tempfile.TemporaryDirectory() as copy:
        for path in touched:
            (Path(copy) / path).parent.mkdir(parents=True, exist_ok=True)
            (Path(copy) / path).write_bytes((ROOT / path).read_bytes())
        applied = subprocess.run(["patch", "-s", "-F0", "-p1", "-d", copy], input=patch.read_text(),
                                 capture_output=True, text=True)
        if applied.returncode != 0:
            return None, f"doesn't apply: {applied.stdout}{applied.stderr}"
        files = [Path(copy) / path for path in touched if path.endswith(".rs")]
        return [match for file in files for match in rule(file, file.read_text())], None


def check_mutants():
    """Each rule's mutants it matches, and each mutant a rule's or unruled: the failures."""
    failures = []
    patches = mutant_patches()
    reasons = unruled()
    ruled = {}
    for rule, mutants in RULES:
        for mutant in mutants:
            ruled[mutant] = rule.__name__
            if mutant not in patches:
                failures.append(f"{rule.__name__}: no mutant {mutant}")
                continue
            found, why_not = with_mutant(rule, patches[mutant])
            if why_not:
                failures.append(f"{rule.__name__}: {mutant} {why_not}")
            elif not found:
                failures.append(f"{rule.__name__}: matches nothing with {mutant} in")
    for mutant in patches:
        if mutant not in ruled and mutant not in reasons:
            failures.append(f"{mutant}: no rule names it, nor scripts/rust-rules/unruled")
    for mutant, why in reasons.items():
        if mutant in ruled:
            failures.append(f"{mutant}: named by {ruled[mutant]}, and unruled too")
        elif mutant not in patches:
            failures.append(f"scripts/rust-rules/unruled: no mutant {mutant}")
        elif not why:
            failures.append(f"scripts/rust-rules/unruled: {mutant} says no why")
    return failures


def main():
    if sys.argv[1:] == ["--mutants"]:
        failures = check_mutants()
    else:
        failures = check([Path(path) for path in sys.argv[1:]] or [ROOT / "crates"])
    print("".join(f"{failure}\n" for failure in failures), end="")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
