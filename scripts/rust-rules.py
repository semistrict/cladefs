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
    named `name="value"`."""
    tokens = CFG_TOKEN.findall(predicate)
    position = 0

    def node():
        nonlocal position
        name = tokens[position]
        position += 1
        if position < len(tokens) and tokens[position] == "=":
            position += 2
            return (f"{name}={tokens[position - 1]}", None)
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


def only_in_tests(tree, tests=("test",)):
    """Whether a cfg predicate, as cfg_tree says it, holds only where one of `tests` does:
    `test`, `all` of one such among others, `any` of such alone (board 6490: `all(test, target_os
    = "linux")`)."""
    name, children = tree
    if children is None:
        return name in tests
    if name == "all":
        return any(only_in_tests(child, tests) for child in children)
    if name == "any":
        return bool(children) and all(only_in_tests(child, tests) for child in children)
    return False


# What tests alone build: `test`; the `testing` feature, which a crate's tests turn on in the
# crates they test for their harnesses (`any(test, feature = "testing")`); `madsim`, the
# full-stack simulation's build (doc-24); and the model checkers' features, Shuttle's and loom's.
TESTS_AND_HARNESSES = ("test", 'feature="testing"', "madsim", 'feature="shuttle"', 'feature="loom"')


def starts_tests(lines, number, tests=("test",)):
    """Whether line `number` of `lines`, from 1, begins a test module: a `#[cfg(...)]` holding
    only in `tests` on a module's declaration with its body, at any depth. One in a file of its own
    (`mod sim;`) is that file's (built_only_in), the lines after it the declaring file's."""
    cfg = CFG.match(lines[number - 1].strip())
    return cfg is not None and only_in_tests(cfg_tree(cfg.group(1)), tests) and \
        number < len(lines) and MODULE.match(lines[number].lstrip()) is not None and \
        not lines[number].split("//", 1)[0].rstrip().endswith(";")


def built_only_in_tests(lines, number, tests=("test",)):
    """Whether line `number` of `lines`, from 1, is a `#[cfg(...)]` holding only in `tests`: on an
    item or a statement, of any kind (board 6908: a function of tests', a statement of theirs)."""
    cfg = CFG.match(lines[number - 1].strip())
    return cfg is not None and only_in_tests(cfg_tree(cfg.group(1)), tests)


def brackets(code):
    """How far `code`, a line's, opens brackets of any kind, less how far it closes them."""
    return sum(code.count(opening) for opening in "([{") - sum(code.count(c) for c in ")]}")


def outside_tests(lines, tests=("test",)):
    """Each of `lines`, numbered from 1, but those the rules leave to tests: a file's test module
    and what follows it; a test module within another module (board 6105: migration.rs's), to its
    closing brace, at its own indent; and any other item or statement built only in `tests`, to
    where its brackets close (board 6908: a function of tests', a statement of theirs, a
    `compile_error!(..)`). A test module is one built only in `tests`."""
    closing = None
    attributed = False
    depth = 0
    for number, line in enumerate(lines, 1):
        if closing is not None:
            if line.startswith(closing + "}"):
                closing = None
            continue
        if attributed:
            if line.lstrip().startswith("#["):
                continue
            depth += brackets(line.split("//", 1)[0])
            if depth <= 0:
                attributed, depth = False, 0
            continue
        if starts_tests(lines, number, tests):
            indent = line[:len(line) - len(line.lstrip())]
            # A file's tests, at its top: they and what follows them. A harness of tests among
            # its items (`any(test, feature = "testing")`): to its closing brace.
            if not indent and starts_tests(lines, number):
                return
            closing = indent
            continue
        if built_only_in_tests(lines, number, tests):
            attributed = True
            continue
        yield number, line


def outside_the_seam(path, text):
    """A thread started, a runtime built or a thread blocked on a future in cladefsd's code, other
    than through its seam (cladefsd::run), which stops the simulation's one thread under madsim
    (doc-24, TASK-685.3): each goes through the seam, or a comment on it or the few lines above says
    why it is outside the simulation (the VM socket, FUSE, metrics, signals, clients). Its tests
    aside, a file's test module and what follows it among them."""
    posix = path.as_posix()
    if "crates/cladefsd/src/" not in posix or posix.endswith(("/run.rs", "/tests.rs", "/fuse_control.rs", "/testing_control.rs")) \
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
OFF_HOST_FS_CRATES = ("crates/cladefs/src/", "crates/cladefs_core/src/", "crates/cladefs_fs/src/", "crates/cladefs_fuse/src/", "crates/cladefs_wire/src/", "crates/cladefsd/src/", "crates/local_cell/src/", "crates/bucket_cell/src/")
# The files whose reading or writing of the host's file system is outside the simulation as a
# whole (doc-24): FUSE, the VM sockets and a VM process's client, procfs, the image an import reads,
# a file bucket, the tools; and those that are tests' alone.
OFF_HOST_FS_FILES = (
    "/cladefs_fuse/src/", "/cladefsd/src/grants.rs", "/cladefsd/src/lib.rs", "/cladefsd/src/mount/fuse.rs",
    "/cladefs_wire/src/client.rs", "/cladefsd/src/governor.rs", "/cladefsd/src/process.rs", "/cladefsd/src/fuse_control.rs", "/dax/uffd.rs",
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


# cladefs: the crates whose code falls back silently at its peril: the daemon's, and what it is
# built of, cladefs's own. Fuchsia's crates are upstream's; the simulations and examples are tests.
FALLBACK_CRATES = (
    "block_device", "bucket_cell", "cladefs", "extent_frame", "extent_keys",
    "fxfs-posix", "host_fs", "local_cell", "node_local", "sqlite_cladefs", "virtio_extent",
)
# An error dropped: a pattern of any `Err`, its error unnamed; a `Result` made an `Option`; a
# closure given the error that ignores it; a `Result` awaited and made a value at once.
DROPPED = re.compile(r"\bErr\((?:_|\.\.)\)|\.ok\(\)|\.unwrap_or_else\(\|_\||"
                     r"\.await\s*\.unwrap_or(?:_default|_else)?\(")
# A `let _ =` on anything but bindings, which drops what it is: a call's `Result`, a task's answer
# awaited (a panic within it among them). Not `write!` into a String, which can't fail, nor
# arguments a default method leaves unused, a binding or a tuple of them, borrowed or not.
BINDING = r"&?(?:(?!\.await\b)[\w.:])+"
BINDINGS = rf"{BINDING}|\((?:\s*{BINDING}\s*,?)+\)"
LET_DROPPED = re.compile(rf"\blet _ =(?!\s*(?:write!|writeln!|(?:{BINDINGS})\s*;))")
# What makes a dropped error no fallback, right after it on its line: an `Err` arm giving an
# error on, or panicking; a closure given the error making another of it.
PROPAGATED = re.compile(r"\s*(?:\|[\w, ]*\|\s*)?(?:=>\s*)?(?:return\s+)?(?:Err\(|Some\(Err\(|panic!|unreachable!|"
                        r"anyhow::bail!|bail!)")
# A line continuing the statement above it: a method chained, a closing bracket, a `?`, a
# condition's next term.
CONTINUES = re.compile(r"^\s*(?:\.|\?|\)|\]|&&|\|\|)")


def parent_module(path):
    """The file declaring `path`'s module, and the name it declares it by: None for a crate's root,
    a binary's or a file outside a crate's src/."""
    parts = path.parts
    if "src" not in parts:
        return None
    src = len(parts) - 1 - parts[::-1].index("src")
    inner = parts[src + 1:]
    if inner in (("lib.rs",), ("main.rs",)) or inner[:1] == ("bin",):
        return None
    name = path.parent.name if path.name == "mod.rs" else path.stem
    directory = path.parent.parent if path.name == "mod.rs" else path.parent
    if directory == Path(*parts[:src + 1]):
        candidates = [directory / "lib.rs", directory / "main.rs"]
    else:
        candidates = [directory.with_suffix(".rs"), directory / "mod.rs"]
    for candidate in candidates:
        if candidate.is_file():
            return candidate, name
    return None


def built_only_in(path, tests):
    """Whether `path`'s module is built only in `tests`: its declaration, or an ancestor's, under a
    cfg that holds only there, or within a test module of the file declaring it."""
    declared = parent_module(path)
    if declared is None:
        return False
    parent, name = declared
    lines = parent.read_text(errors="replace").splitlines()
    declaration = re.compile(rf"^\s*(pub(\([\w:]+\))? )?mod {re.escape(name)};")
    built = {number for number, _ in outside_tests(lines, tests)}
    for number, line in enumerate(lines, 1):
        if not declaration.match(line):
            continue
        if number not in built:
            return True
        above = number - 1
        while above >= 1 and lines[above - 1].strip().startswith("#["):
            cfg = CFG.match(lines[above - 1].strip())
            if cfg and only_in_tests(cfg_tree(cfg.group(1)), tests):
                return True
            above -= 1
        return built_only_in(parent, tests)
    return False


def explained(lines, number):
    """Whether line `number` of `lines`, from 1, has a comment on it, or the statement it belongs
    to a comment just above it: the lines of comment right over its first line."""
    if "//" in lines[number - 1]:
        return True
    start = number
    while start > 1 and (CONTINUES.match(lines[start - 1])
                         or lines[start - 2].split("//", 1)[0].rstrip().endswith(("=", "("))):
        start -= 1
        if "//" in lines[start - 1]:
            return True
    return start > 1 and lines[start - 2].lstrip().startswith("//")


def silent_fallback(path, text):
    """An error dropped without a word: an `Err(_)` arm carrying on, a `.ok()`, an
    `.unwrap_or_else(|_| ..)`, a `Result` awaited and defaulted, a `let _ =` on a call, in
    cladefs's own crates' code. Each is a failure the code goes on past as though it had been an
    answer, and some such answers stick: a bucket's default slot count kept for good (board 5534),
    a member's failed LIST taken for an empty one (board 5526), a spool's failure ignored until a
    flush asserted (board 5424). Each says why on its line, or in the comment over its statement,
    or fails (board 6908). Tests aside: their files, modules and harnesses."""
    posix = path.as_posix()
    crate = re.search(r"crates/([^/]+)/src/", posix)
    if crate is None or crate.group(1) not in FALLBACK_CRATES or "/tests/" in posix \
            or posix.endswith(("/tests.rs", "_tests.rs", "/testing.rs")) \
            or built_only_in(path, TESTS_AND_HARNESSES):
        return
    lines = text.splitlines()
    for number, line in outside_tests(lines, TESTS_AND_HARNESSES):
        code = line.split("//", 1)[0]
        # What follows a dropped error ending its line, `{` or `=> {`, goes on on the next.
        following = lines[number].split("//", 1)[0] if number < len(lines) else ""
        dropped = any(not (PROPAGATED.match(code, found.end()) or
                           re.fullmatch(r"\s*(?:\|[\w, ]*\|\s*)?(?:=>\s*)?\{\s*",
                                        code[found.end():]) and
                           PROPAGATED.match(following) or
                           found.group().startswith("Err") and "matches!(" in code)
                      for found in DROPPED.finditer(code))
        follows_await = re.match(r"\s*\.unwrap_or(?:_default|_else)?\(", code) and number > 1 \
            and lines[number - 2].split("//", 1)[0].rstrip().endswith(".await")
        # A `let _ =` its value on the line after.
        let = LET_DROPPED.search(code) and LET_DROPPED.search(f"{code} {following}")
        if not (dropped or follows_await or let) or explained(lines, number):
            continue
        yield Match(path, number, "silent-fallback",
                    "an error dropped, carried on past as an answer: say why on its line or over "
                    "its statement, or let it fail (board 6908)")


# cladefs: the crates the full-stack simulation runs, and what they link into the daemon (doc-24).
SIMULATED_CRATES = ("bucket_cell", "cladefs", "extent_frame", "extent_keys", "host_fs", "local_cell")
# A static's declaration: its name and type.
STATIC = re.compile(r"^\s*(?:pub(?:\([\w:]+\))? )?static (?:mut )?([A-Z_0-9]+)\s*:\s*(.*)")
# A static's type that holds nothing that changes: a string, a slice or array of plain values, a
# number, a flag.
PLAIN_TYPE = re.compile(r"(?:&'static |&)(?:str|\[)|\[&|\[?(?:u|i)(?:8|16|32|64|128|size)\b|bool\b|"
                        r"f(?:32|64)\b|char\b")
# A block of statics: task_local's, one per task, each a node's; thread_local's, one per thread,
# which the simulation's one thread gives every node alike.
STATICS_BLOCK = re.compile(r"\b(task_local|thread_local)!\s*\{")
# What says a static is the process's, or per node: its comment's words, or a cfg that leaves it
# out of the simulation.
PROCESS_WIDE = ("process", "node", "per thread", "outside the simulation", "not(madsim)")


def said_above(lines, number, words):
    """Whether line `number` of `lines`, from 1, or the comments and attributes right over it,
    say one of `words`."""
    said = lines[number - 1].split("//", 1)[1] if "//" in lines[number - 1] else ""
    above = number - 1
    while above >= 1 and lines[above - 1].lstrip().startswith(("//", "#[")):
        said += lines[above - 1]
        above -= 1
    return any(word in said.lower() for word in words)


def process_wide_static(path, text):
    """A static of a daemon's that every simulated host shares: madsim runs them all in one
    process, on one thread, so a mutable static, a thread_local's among them, is one for every
    host, and a host's state kept in one leaks to the others and outlives its kill (board 5450:
    a part cache all hosts shared; board 7451: a lost host's replicator, on the process's
    runtime, went on). Keep per-host state on a Host/Daemon context instead (board 10962), or say
    on its line or in the comments over it why it is the process's or that it is per node (board
    7583). Tests aside: their files, modules and harnesses."""
    posix = path.as_posix()
    crate = re.search(r"crates/([^/]+)/src/", posix)
    if crate is None or crate.group(1) not in SIMULATED_CRATES or "/tests/" in posix \
            or "/src/bin/" in posix \
            or posix.endswith(("/tests.rs", "_tests.rs", "/testing.rs")) \
            or built_only_in(path, TESTS_AND_HARNESSES):
        return
    lines = text.splitlines()
    depth, block = 0, None
    for number, line in outside_tests(lines, TESTS_AND_HARNESSES):
        code = line.split("//", 1)[0]
        opened = STATICS_BLOCK.search(code)
        if block is None and opened:
            block, depth = opened.group(1), 0
        in_block = block
        if block is not None:
            depth += brackets(code)
            if depth <= 0:
                block = None
        declared = STATIC.match(code)
        if declared is None or in_block == "task_local" \
                or PLAIN_TYPE.match(declared.group(2)) or said_above(lines, number, PROCESS_WIDE):
            continue
        yield Match(path, number, "process-wide-static",
                    f"`{declared.group(1)}` is one for every host the simulation runs: keep "
                    "per-host state on a Host/Daemon instead, or say why it is the process's "
                    "(board 7583, board 10962)")


# Crates exercised by the deterministic `local_cell::sim` replay harness (`sim::seeds_replay`).
SIM_REPLAY_CRATES = ("bucket_cell", "local_cell")
UNSEEDED_RAND = re.compile(r"\brand::(?:random|thread_rng|rng)\b")
UNSEEDED_RAND_OK = ("outside the simulation", "shuttle", "not(madsim)")


def unseeded_rand(path, text):
    """Unseeded process RNG (`rand::random` / `rand::thread_rng` / `rand::rng`) in crates run by
    `local_cell::sim` breaks seed replay (`sim::seeds_replay`, board 12982). Draw deterministic
    values from `Bucket::random_u64()` or a seeded RNG instead, or explain on the line / comment
    above why the call is outside the simulation."""
    posix = path.as_posix()
    crate = re.search(r"crates/([^/]+)/src/", posix)
    if crate is None or crate.group(1) not in SIM_REPLAY_CRATES or "/tests/" in posix \
            or "/src/bin/" in posix or "/sim/" in posix \
            or posix.endswith(("/tests.rs", "_tests.rs", "/testing.rs")) \
            or built_only_in(path, TESTS_AND_HARNESSES):
        return
    lines = text.splitlines()
    for number, line in outside_tests(lines, TESTS_AND_HARNESSES):
        code = line.split("//", 1)[0]
        hit = UNSEEDED_RAND.search(code)
        if hit is None or said_above(lines, number, UNSEEDED_RAND_OK):
            continue
        yield Match(path, number, "unseeded-rand",
                    f"`{hit.group(0)}` uses unseeded process RNG inside a simulation crate: use "
                    "`Bucket::random_u64()` or say why it is outside the simulation (board 12982)")


# Each rule, and the mutants of the bugs it is from.
RULES = [
    (outside_the_seam, []),
    (off_host_fs, []),
    (silent_fallback, []),
    (process_wide_static, []),
    (unseeded_rand, []),
    (mixed_orderings, [
        "occurrence-happens-relaxed", "occurrence-listens-relaxed", "answers-release-no-capacity",
        "capacities-read-relaxed",
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
