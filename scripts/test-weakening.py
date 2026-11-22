#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Flags commits that weaken what checks the code, unless their message says why.

A test bent around a bug merges never (AGENTS.md), and models cheat on tests that contradict them,
stronger ones more (ImpossibleBench, doc-17). So a commit that does any of these says why in a
`Weakens-tests:` line of its message (the task of the bug it steps around, or why it is no weaker):

- assertion-removed: more assertions (Rust's assert macros, C's BUG_ON/WARN_ON/lockdep_assert,
  Python's) removed than added, those not moved elsewhere in the commit: a rewrite restating its
  checks isn't flagged, a net loss is.
- bound-loosened: an assertion's bound moved the lenient way (`x < 10` to `x < 20`), the rest of
  it the same. A changed expected value (`assert_eq!(x, 3)` to 4) isn't flagged.
- test-ignored: a test ignored, skipped, or expected to panic, unless the commit adds the test (a
  benchmark run by hand, a test of a panic): one of a name its file had none of before, nor any
  file the commit takes it from, with its test attribute, so that it checks nothing less than
  before (TASK-468).
- test-removed: more #[test]s, or Python test_ methods, removed than added.
- count-lowered: a count of cases, seeds, rounds, iterations or operations lowered.
- generator-narrowed: a property test's inputs filtered or assumed away, or a range in a test's
  file narrowed.
- known-failure-added: a line added to an open bugs' list of failures the runs let be.
- invariant-removed: an invariant or property gone from a TLA+ model's configuration
  (specs/**/*.cfg), and spec-config-narrowed: a constant of one lowered (`MaxTakes = 2` to 1).
- mutant-removed: a spec mutation or a hand mutant deleted.

Moved lines aren't flagged: a line removed and added back, anywhere in the commit, is a move.

scripts/hooks/commit-msg runs it on what is staged (`git config core.hooksPath scripts/hooks`);
CI on what each push or pull request brings (TASK-444.9). Exits 1, saying each, if a commit
weakens a check and its message doesn't say why; what a message does say why for, it lists.

Usage: scripts/test-weakening.py --staged [--message FILE]
       scripts/test-weakening.py RANGE
"""

import argparse
import re
import subprocess
import sys
from collections import Counter
from dataclasses import dataclass

TRAILER = re.compile(r"^Weakens-tests:\s*\S", re.MULTILINE)

# Files whose numbers can be counts of what runs: code, scripts and workflows, not data or notes.
RUNS = re.compile(r"\.(rs|c|h|py|go|sh|ya?ml|toml)$|^scripts/[^.]+$")


@dataclass(frozen=True)
class Language:
    """What an assertion, an ignored test, a test, a narrowed generator and a function's
    definition, by its `name`, look like in a language's code. A test pattern with a `name` group
    counts tests by name, else all alike. An ignoring line beginning an attribute or a decorator
    (`#[`, `@`) ignores the function defined after it, any other the one it is in."""
    comments: tuple
    assertion: re.Pattern
    ignored: re.Pattern = None
    test: re.Pattern = None
    narrowing: re.Pattern = None
    definition: re.Pattern = None


C = Language(("//", "/*", "*"), re.compile(
    r"\b(BUG_ON|WARN_ON\w*|lockdep_assert\w*|KUNIT_(EXPECT|ASSERT)\w*|static_assert|assert)\s*\("))
LANGUAGES = {
    ".rs": Language(
        ("//", "/*", "*"),
        re.compile(r"\b(assert(_eq|_ne|_matches)?|debug_assert\w*|prop_assert\w*)!"),
        ignored=re.compile(r"#\[\s*(ignore|should_panic)\b|cfg_attr\(.*\bignore\b"),
        test=re.compile(r"#\[\s*([\w:]+::)?test\b"),
        narrowing=re.compile(r"\bprop_assume!|\.prop_filter(_map)?\("),
        definition=re.compile(r"\bfn\s+(?P<name>\w+)")),
    ".c": C,
    ".h": C,
    ".py": Language(
        ("#",),
        re.compile(r"^\s*assert\s|\bself\.assert\w+\s*\("),
        ignored=re.compile(r"@unittest\.skip|\.skipTest\(|pytest\.mark\.(skip|xfail)"),
        test=re.compile(r"^\s*def\s+(?P<name>test_\w+)"),
        definition=re.compile(r"^\s*def\s+(?P<name>\w+)")),
    ".go": Language(
        ("//", "/*", "*"),
        re.compile(r"\bt\.(Fatal|Error)\w*\(|\b(assert|require)\.\w+\("),
        ignored=re.compile(r"\bt\.Skip\w*\("),
        test=re.compile(r"^func\s+(?P<name>Test\w+)"),
        definition=re.compile(r"^func\s+(?P<name>\w+)")),
}
COUNT = re.compile(r"cases|seeds?\b|rounds|iterations|operations|\bops\b|PROPTEST_CASES|\s-s\s")
TEST_FILE = re.compile(r"(^|/)tests?/|_tests?\.rs$|(^|/)tests\.rs$|(^|/)test_[^/]*\.py$")
KNOWN_LISTS = {"scripts/pjdfstest-fuse-known", "guest/sim/known-failures",
               "crates/fxfs-posix/known-panics", "crates/cladefs/tests/madsim-known"}
MUTANTS = re.compile(r"^specs/[^/]+/mutations/|^guest/mutants/")
SPEC_CONFIG = re.compile(r"^specs/.*\.cfg$")
CONFIG_SECTION = re.compile(r"^([A-Z_]+)\b")
CHECKED = {"INVARIANT", "INVARIANTS", "PROPERTY", "PROPERTIES"}
NUMBER = re.compile(r"\d+")
# A comparison with a number: the operator, and which side the number is on.
BOUND = re.compile(r"(?<![-=<>])(<=?|>=?)\s*(\d+)|(\d+)\s*(<=?|>=?)(?![=>])")


@dataclass(frozen=True)
class Line:
    path: str
    number: int
    text: str


@dataclass(frozen=True)
class Flag:
    rule: str
    line: Line

    def __str__(self):
        return f"{self.line.path}:{self.line.number}: {self.rule}: {self.line.text.strip()}"


def git(*args):
    return subprocess.run(["git", "-c", "core.quotePath=false", *args], check=True,
                          capture_output=True, text=True).stdout


def parse(diff):
    """A unified diff's (-U0) removed and added lines, and the files it deletes."""
    removed, added, deleted = [], [], []
    old_path = new_path = None
    old_line = new_line = 0
    for text in diff.splitlines():
        if text.startswith("diff --git "):
            old_path = new_path = None
        elif text.startswith("--- "):
            old_path = None if text == "--- /dev/null" else text[6:].rstrip("\t")
        elif text.startswith("+++ "):
            new_path = None if text == "+++ /dev/null" else text[6:].rstrip("\t")
            if new_path is None and old_path is not None:
                deleted.append(old_path)
        elif text.startswith("@@"):
            match = re.match(r"@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@", text)
            old_line, new_line = int(match.group(1)), int(match.group(2))
        elif text.startswith("-") and old_path is not None:
            removed.append(Line(old_path, old_line, text[1:]))
            old_line += 1
        elif text.startswith("+") and new_path is not None:
            added.append(Line(new_path, new_line, text[1:]))
            new_line += 1
    return removed, added, deleted


def code(line):
    """The line's language and text, if it is code of a source file, not a comment; else None."""
    language = LANGUAGES.get(re.sub(r"^.*(\.\w+)$", r"\1", line.path))
    if language is None:
        return None
    stripped = line.text.strip()
    if stripped.startswith(language.comments) and not stripped.startswith("#["):
        return None
    return language, stripped


def matches(line, kind):
    """The match of the line's language's pattern `kind` in its code, if any."""
    found = code(line)
    if found is None or getattr(found[0], kind) is None:
        return None
    return getattr(found[0], kind).search(found[1])


def test_key(match):
    return match.groupdict().get("name") or "#[test]"


def skeleton(text):
    return NUMBER.sub("#", " ".join(text.split()))


def numbers(text):
    return [int(n) for n in NUMBER.findall(text)]


def bounds(text):
    """Each comparison with a number in text: +1 where a larger number is more lenient
    (`x < 10`, `10 > x`), -1 where a smaller one is, with the number."""
    found = []
    for match in BOUND.finditer(text):
        if match.group(1):
            op, number, right = match.group(1), int(match.group(2)), True
        else:
            op, number, right = match.group(4), int(match.group(3)), False
        found.append(((1 if op.startswith("<") else -1) * (1 if right else -1), number))
    return found


def loosened(old, new):
    """Whether an assertion's bounds moved the lenient way, old to new."""
    before, after = bounds(old), bounds(new)
    return len(before) == len(after) and any(
        sense == sense_after and (number_after - number) * sense > 0
        for (sense, number), (sense_after, number_after) in zip(before, after))


def section(old_text, number):
    """The section of a TLA+ configuration (CONSTANTS, INVARIANTS...) its line `number` is in."""
    current = None
    for text in old_text.splitlines()[:number]:
        if match := CONFIG_SECTION.match(text):
            current = match.group(1)
    return current


def runs(lines):
    """Lines added one after another in a file, each run in order."""
    found = []
    for line in lines:
        if found and found[-1][-1].path == line.path and found[-1][-1].number + 1 == line.number:
            found[-1].append(line)
        else:
            found.append([line])
    return found


def defined(lines):
    """The names of the functions the lines define."""
    return {match.group("name") for line in lines if (match := matches(line, "definition"))}


def ignoring_added_tests(added, removed, old_file):
    """The lines of `added` ignoring, skipping or expecting a panic of a test the commit adds: its
    definition, of a name defined in its file before nowhere, nor in a line the commit removes,
    added with the ignoring line and its test attribute, one run of lines."""
    gone = defined(removed)
    before = {}

    def new(path, name):
        if path not in before:
            text = old_file(path) or ""
            before[path] = defined(Line(path, 0, text) for text in text.splitlines())
        return name not in before[path] and name not in gone

    found = set()
    for run in runs(added):
        definitions = [i for i, line in enumerate(run) if matches(line, "definition")]
        for i, line in enumerate(run):
            if not matches(line, "ignored"):
                continue
            if line.text.strip().startswith(("#[", "@")):
                own = next((d for d in definitions if d > i), None)
            else:
                own = next((d for d in reversed(definitions) if d < i), None)
            if own is None:
                continue
            start = max((d + 1 for d in definitions if d < own), default=0)
            if new(line.path, matches(run[own], "definition").group("name")) and \
                    any(matches(header, "test") for header in run[start:own + 1]):
                found.add(line)
    return found


def flags(diff, old_file=lambda path: None):
    """What of a commit's diff weakens a check. old_file(path) is a file's text before it."""
    removed, added, deleted = parse(diff)
    found = []
    moved = Counter(" ".join(line.text.split()) for line in added)

    def is_move(line):
        key = " ".join(line.text.split())
        if key and moved[key] > 0:
            moved[key] -= 1
            return True
        return False

    # Lines whose numbers alone changed: an assertion's bound, a count, a generator's range.
    pending = {}
    for line in added:
        pending.setdefault((line.path, skeleton(line.text)), []).append(line)
    changed = set()
    for line in removed:
        partners = pending.get((line.path, skeleton(line.text)))
        if not partners or " ".join(line.text.split()) == " ".join(partners[0].text.split()):
            continue
        new = partners.pop(0)
        changed.add(line)
        lowered = any(b < a for a, b in zip(numbers(line.text), numbers(new.text)))
        if matches(line, "assertion"):
            if loosened(line.text, new.text):
                found.append(Flag("bound-loosened", new))
        elif SPEC_CONFIG.search(line.path):
            if lowered:
                found.append(Flag("spec-config-narrowed", new))
        elif lowered and RUNS.search(line.path):
            if COUNT.search(line.text) or (TEST_FILE.search(line.path) and ".." in line.text):
                found.append(Flag("count-lowered" if COUNT.search(line.text)
                                  else "generator-narrowed", new))

    tests = Counter()
    first_removed = {}
    assertions_gone = []
    for line in removed:
        if line in changed or is_move(line):
            continue
        if matches(line, "assertion"):
            assertions_gone.append(line)
        if match := matches(line, "test"):
            tests[test_key(match)] += 1
            first_removed.setdefault(test_key(match), line)
        if SPEC_CONFIG.search(line.path) and line.text.strip() and \
                not line.text.strip().startswith("\\*"):
            old_text = old_file(line.path)
            if old_text is not None and section(old_text, line.number) in CHECKED:
                found.append(Flag("invariant-removed", line))
    removed_texts = Counter(" ".join(line.text.split()) for line in removed)
    assertions_added = 0
    added_tests = ignoring_added_tests(added, removed, old_file)
    for line in added:
        moved_here = removed_texts[" ".join(line.text.split())] > 0
        if matches(line, "assertion") and not moved_here:
            assertions_added += 1
        if matches(line, "ignored") and not moved_here and line not in added_tests:
            found.append(Flag("test-ignored", line))
        if matches(line, "narrowing"):
            found.append(Flag("generator-narrowed", line))
        if match := matches(line, "test"):
            tests[test_key(match)] -= 1
        if line.path in KNOWN_LISTS and line.text.strip() and \
                not line.text.strip().startswith("#") and \
                not any(r.path == line.path and r.text == line.text for r in removed):
            found.append(Flag("known-failure-added", line))
    if len(assertions_gone) > assertions_added:
        found.extend(Flag(f"assertion-removed ({len(assertions_gone)} removed, "
                          f"{assertions_added} added)", line) for line in assertions_gone)
    for key, count in tests.items():
        if count > 0 and key in first_removed:
            found.append(Flag(f"test-removed ({count} {key} more removed than added)"
                              if key == "#[test]" else "test-removed", first_removed[key]))
    for path in deleted:
        if MUTANTS.search(path):
            found.append(Flag("mutant-removed", Line(path, 0, "(deleted)")))
    return sorted(found, key=lambda flag: (flag.line.path, flag.line.number, flag.rule))


def commits(revisions):
    """Each commit of the range, oldest first: its sha, diff and message. Merges bring what their
    parents' commits did, which those are checked for."""
    shas = git("rev-list", "--no-merges", "--reverse", revisions).split()
    for sha in shas:
        diff = git("show", "--format=", "-U0", "-M", "--no-color", "--no-ext-diff", sha)
        yield sha[:12], diff, git("log", "-1", "--format=%B", sha), old_files(f"{sha}^")


def old_files(revision):
    """A file's text at revision, or None where it has none."""
    def read(path):
        done = subprocess.run(["git", "show", f"{revision}:{path}"], capture_output=True, text=True)
        return done.stdout if done.returncode == 0 else None
    return read


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("range", nargs="?", help="the commits to check, as git rev-list takes")
    parser.add_argument("--staged", action="store_true", help="check what is staged")
    parser.add_argument("--message", help="the commit message's file, with --staged")
    args = parser.parse_args()
    if args.staged == bool(args.range):
        parser.error("give a range or --staged")
    if args.staged:
        message = open(args.message).read() if args.message else ""
        diff = git("diff", "--cached", "-U0", "-M", "--no-color", "--no-ext-diff")
        checked = [("staged", diff, message, old_files("HEAD"))]
    else:
        checked = commits(args.range)

    status = 0
    for name, diff, message, old_file in checked:
        found = flags(diff, old_file)
        if not found:
            continue
        said = TRAILER.search(message)
        print(f"{name}: weakens checks, " +
              ("as its message says why:" if said else "and its message doesn't say why:"))
        for flag in found:
            print(f"  {flag}")
        if not said:
            print("  Say why in a 'Weakens-tests:' line of the message: the task of the bug it "
                  "steps around, or why it is no weaker (AGENTS.md: never write a test around a "
                  "bug).")
            status = 1
    return status


if __name__ == "__main__":
    sys.exit(main())
