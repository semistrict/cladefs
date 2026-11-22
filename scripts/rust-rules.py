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


# cladefs: what of the daemon's code starts a thread, builds a runtime or blocks a thread on a
# future: the daemon's seam's alone (doc-24, TASK-685.3).
UNSEAMED = re.compile(r"\bthread::spawn\b|\bthread::Builder::new\(\)|\bruntime::Builder::new_\w+_thread\b|"
                      r"\bblock_in_place\b|\bspawn_blocking\b|\.block_on\(|\bexecutor::block_on\b")
# How far above such a line a comment may say why it is outside the simulation.
UNSEAMED_REASON_LINES = 3


# A module declared, with any visibility: `mod`, `pub mod`, `pub(crate) mod`.
MODULE = re.compile(r"(pub(\([\w:]+\))? )?mod ")


# A `#[cfg(...)]` line, its predicate; and the predicate's tokens.
CFG = re.compile(r"#\[cfg\((.*)\)\]$")
CFG_TOKEN = re.compile(r'\w+|"[^"]*"|[(),=]')


def cfg_tree(predicate):
    """A cfg `predicate` as (name, children): children None for an option, `name = "value"`
    named `=`."""
    tokens = CFG_TOKEN.findall(predicate)
    position = 0

    def node():
        nonlocal position
        name = tokens[position]
        position += 1
        if position < len(tokens) and tokens[position] == "=":
            position += 2
            return ("=", None)
        if position == len(tokens) or tokens[position] != "(":
            return (name, None)
        position += 1
        children = []
        while tokens[position] != ")":
            children.append(node())
            if tokens[position] == ",":
                position += 1
        position += 1
        return (name, children)

    return node()


def only_in_tests(tree):
    """Whether a cfg predicate, as cfg_tree says it, holds only where `test` does: `test`, `all`
    of one such among others, `any` of such alone (board 6490: `all(test, target_os = "linux")`)."""
    name, children = tree
    if children is None:
        return name == "test"
    if name == "all":
        return any(only_in_tests(child) for child in children)
    if name == "any":
        return bool(children) and all(only_in_tests(child) for child in children)
    return False


def starts_tests(lines, number):
    """Whether line `number` of `lines`, from 1, begins a test module: a `#[cfg(...)]` holding
    only in tests on a module's declaration, at any depth."""
    cfg = CFG.match(lines[number - 1].strip())
    return cfg is not None and only_in_tests(cfg_tree(cfg.group(1))) and number < len(lines) and \
        MODULE.match(lines[number].lstrip()) is not None


def outside_tests(lines):
    """Each of `lines`, numbered from 1, but those the rules leave to tests: a file's test module
    and what follows it; a test module within another module (board 6105: migration.rs's), to its
    closing brace, at its own indent."""
    closing = None
    for number, line in enumerate(lines, 1):
        if closing is not None:
            if line == closing:
                closing = None
            continue
        if starts_tests(lines, number):
            indent = line[:len(line) - len(line.lstrip())]
            if not indent:
                return
            closing = indent + "}"
            continue
        yield number, line


def outside_the_seam(path, text):
    """A thread started, a runtime built or a thread blocked on a future in cladefsd's code, other
    than through its seam (cladefsd::run), which stops the simulation's one thread under madsim
    (doc-24, TASK-685.3): each goes through the seam, or a comment on it or the few lines above says
    why it is outside the simulation (the VM socket, FUSE, metrics, signals, clients). Its tests
    aside, a file's test module and what follows it among them."""
    posix = path.as_posix()
    if "crates/cladefs/src/cladefsd/" not in posix or posix.endswith(("/run.rs", "/tests.rs")) \
            or "/tests/" in posix:
        return
    lines = text.splitlines()
    for number, line in outside_tests(lines):
        if not UNSEAMED.search(line.split("//", 1)[0]):
            continue
        above = lines[max(0, number - 1 - UNSEAMED_REASON_LINES):number]
        if any("outside the simulation" in near or "the daemon's seam" in near for near in above):
            continue
        yield Match(path, number, "outside-the-seam",
                    "a thread, a runtime or a blocking wait of the daemon's outside its seam "
                    "(cladefsd::run), which under madsim stops the simulation: go through the seam, "
                    "or say why it is outside the simulation (doc-24, TASK-685.3)")


# The host's file system, other than through host_fs: std::fs, its extension traits, and what
# `Path` asks the file system itself.
OFF_HOST_FS = re.compile(r"\bstd::fs\b|\bstd::os::unix::fs::(FileExt|MetadataExt|OpenOptionsExt)\b|"
                         r"\.(exists|canonicalize|try_exists)\(\)")
OFF_HOST_FS_CRATES = ("crates/cladefs/src/", "crates/local_cell/src/", "crates/bucket_cell/src/")
# The files whose reading or writing of the host's file system is outside the simulation as a
# whole (doc-24): FUSE, the VM sockets and a VM process's client, procfs, the image an import reads,
# a file bucket, the tools; and those that are tests' alone.
OFF_HOST_FS_FILES = (
    "/fuse/", "/cladefsd/grants.rs", "/cladefsd/mod.rs", "/cladefsd/mount.rs",
    "/cladefsd/client.rs", "/cladefsd/governor.rs", "/cladefsd/process.rs", "/dax/uffd.rs",
    "/import.rs", "/import/", "/storage/", "/bucket_cell/src/local.rs", "/bucket_cell/src/faults.rs",
    "/src/bin/", "/testing.rs", "_faults.rs", "/bucket/lifecycle.rs", "/bucket/write_budgets.rs",
    "/upload/encrypted.rs", "/memory_cell.rs",
)


def off_host_fs(path, text):
    """The host's disk read or written by cladefsd's code other than through host_fs, which under
    madsim is each simulated host's disk in memory, crashed as the host is (doc-24, TASK-685.4):
    std::fs, its extension traits, or `Path::exists` and `Path::canonicalize`, which go to the real
    file system there; unless a comment on it or the few lines above says why it is outside the
    simulation, or its file is (OFF_HOST_FS_FILES). Its tests aside, a file's test module and what
    follows it among them."""
    posix = path.as_posix()
    if not any(crate in posix for crate in OFF_HOST_FS_CRATES) \
            or any(file in posix for file in OFF_HOST_FS_FILES) \
            or posix.endswith("/tests.rs") or "/tests/" in posix:
        return
    lines = text.splitlines()
    for number, line in outside_tests(lines):
        if not OFF_HOST_FS.search(line.split("//", 1)[0]):
            continue
        above = lines[max(0, number - 1 - UNSEAMED_REASON_LINES):number]
        if any("outside the simulation" in near for near in above):
            continue
        yield Match(path, number, "off-host-fs",
                    "the host's disk outside host_fs, which under madsim is each simulated host's "
                    "disk in memory: go through host_fs, or say why it is outside the simulation "
                    "(doc-24, TASK-685.4)")


# Each rule, and the mutants of the bugs it is from.
RULES = [
    (outside_the_seam, []),
    (off_host_fs, []),
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
