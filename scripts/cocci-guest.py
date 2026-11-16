#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Runs the semantic patches in scripts/cocci, the shapes the guest's bugs had (TASK-44.17,
TASK-444.6), over the guest's C, with Coccinelle's spatch, and prints what each matches: any match
fails, so that the next instance of a shape is found as it is written, not by a VM stopping.

    scripts/cocci-guest.py [SOURCE...]
    scripts/cocci-guest.py --mutants

checks SOURCE, files or directories, or the guest's C (guest/linux/fs/fxfs and
guest/linux/drivers/virtio_extent). A directory's .c and .h files go to spatch together, so that a
rule binding a function's declaration in a header finds its calls in the .c files. Each rule's
matches print as spatch's context diffs, whose lines starting with '-' are the code matched. A
match scripts/cocci/safe lists, with why it is no bug (`RULE FILE FUNCTION: CODE -- WHY`, a match
of several lines by its first), passes; one it lists that matches nowhere in the guest's C fails,
so that the list stays true.

--mutants checks the rules instead. A rule names the mutants of guest/mutants whose bugs it finds
(`// Mutants: NAME...`): each, put back in a copy of the guest's C, it must match. And every mutant
is named by a rule or in scripts/cocci/unruled, with why no rule finds its kind: each past bug of the
guest's has a rule, or a reason it has none.
"""

import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RULES = ROOT / "scripts/cocci"
MUTANTS = ROOT / "guest/mutants"
UNRULED = RULES / "unruled"
SAFE = RULES / "safe"
GUEST_C = ["guest/linux/fs/fxfs", "guest/linux/drivers/virtio_extent"]
SOURCES = [ROOT / source for source in GUEST_C]


def files(source):
    """The files spatch takes for `source`: a directory's C files and headers, or the file."""
    source = Path(source)
    if source.is_dir():
        return sorted(str(path) for path in source.iterdir() if path.suffix in (".c", ".h"))
    return [str(source)]


def matches(rule, source):
    """What `rule` matches in `source`, a file or a directory: spatch's context diff, or ''."""
    try:
        ran = subprocess.run(
            ["spatch", "--very-quiet", "--sp-file", str(rule), *files(source)],
            capture_output=True,
            text=True,
        )
    except FileNotFoundError:
        raise SystemExit("no spatch: install Coccinelle (apt install coccinelle, brew install "
                         "coccinelle)") from None
    if ran.returncode != 0:
        raise SystemExit(f"spatch {rule.name} on {source} failed:\n{ran.stdout}{ran.stderr}")
    return ran.stdout


def matched_lines(found):
    """The code a context diff of spatch's matched: (file name, function, line), a match of several
    lines by its first, stripped. The function is the one the line is in, as the source has it:
    the function a hunk's header names is diff's guess, which differs between spatch's builds."""
    lines, path, number, previous = [], None, 0, ""
    for line in found.splitlines():
        if line.startswith("--- "):
            path = Path(line[4:].split("\t")[0].strip())
        elif line.startswith("+++ "):
            pass
        elif line.startswith("@@"):
            number = int(re.match(r"@@ -(\d+)", line)[1])
        elif line.startswith((" ", "-")):
            if line.startswith("-") and not previous.startswith("-"):
                lines.append((path.name, function_at(path, number), line[1:].strip()))
            number += 1
        previous = line
    return lines


def function_at(path, number):
    """The function line `number` of C file `path` is in: the name of the nearest definition at or
    above it, a line at the margin naming a function, not a declaration's, which ends in `;`."""
    source = path.read_text().splitlines()
    for line in reversed(source[:number]):
        defined = re.match(r"[A-Za-z_][^;=]*?\b(\w+)\(", line)
        if defined and not line.rstrip().endswith(";"):
            return defined[1]
    return None


def safe():
    """scripts/cocci/safe: each match argued no bug, (rule, file, function, code), and why."""
    entries = {}
    for line in SAFE.read_text().splitlines():
        if line and not line.startswith("#"):
            where, _, why = line.partition(" -- ")
            place, _, code = where.partition(": ")
            rule, file, function = place.split()
            entries[(rule, file, function, code.strip())] = why
    return entries


def named_mutants(rule):
    """The mutants `rule` says it finds, its `// Mutants:` lines."""
    return [name for line in rule.read_text().splitlines() if line.startswith("// Mutants:")
            for name in line.removeprefix("// Mutants:").split()]


def unruled():
    """scripts/cocci/unruled: each mutant no rule finds, and why."""
    reasons = {}
    for line in UNRULED.read_text().splitlines():
        if line and not line.startswith("#"):
            name, _, why = line.partition(": ")
            reasons[name] = why
    return reasons


def with_mutant(rule, mutant):
    """What `rule` matches in a copy of the guest's C with `mutant` in, or why it can't run."""
    patch = MUTANTS / f"{mutant}.patch"
    touched = re.findall(r"^\+\+\+ b/(\S+)", patch.read_text(), re.MULTILINE)
    outside = [path for path in touched if not any(path.startswith(f"{c}/") for c in GUEST_C)]
    if outside:
        return None, f"changes {', '.join(outside)}, outside the guest's C"
    with tempfile.TemporaryDirectory() as copy:
        for source in GUEST_C:
            shutil.copytree(ROOT / source, Path(copy) / source)
        applied = subprocess.run(["patch", "-s", "-F0", "-p1", "-d", copy], input=patch.read_text(),
                                 capture_output=True, text=True)
        if applied.returncode != 0:
            return None, f"doesn't apply: {applied.stdout}{applied.stderr}"
        return "".join(matches(rule, Path(copy) / source) for source in GUEST_C), None


def check_mutants():
    """Each rule's mutants it matches, and each mutant a rule's or unruled: the failures."""
    failures = []
    reasons = unruled()
    ruled = {}
    for rule in sorted(RULES.glob("*.cocci")):
        for mutant in named_mutants(rule):
            ruled[mutant] = rule.name
            if not (MUTANTS / f"{mutant}.patch").exists():
                failures.append(f"{rule.name}: no mutant {mutant}")
                continue
            found, why_not = with_mutant(rule, mutant)
            if why_not:
                failures.append(f"{rule.name}: {mutant} {why_not}")
            elif not found:
                failures.append(f"{rule.name}: matches nothing with {mutant} in")
    for patch in sorted(MUTANTS.glob("*.patch")):
        if patch.stem not in ruled and patch.stem not in reasons:
            failures.append(f"{patch.stem}: no rule names it, nor scripts/cocci/unruled")
    for mutant, why in reasons.items():
        if mutant in ruled:
            failures.append(f"{mutant}: named by {ruled[mutant]}, and unruled too")
        elif not (MUTANTS / f"{mutant}.patch").exists():
            failures.append(f"scripts/cocci/unruled: no mutant {mutant}")
        elif not why:
            failures.append(f"scripts/cocci/unruled: {mutant} says no why")
    return failures


def main():
    if sys.argv[1:] == ["--mutants"]:
        failures = check_mutants()
        print("".join(f"{failure}\n" for failure in failures), end="")
        sys.exit(1 if failures else 0)
    sources = [Path(source) for source in sys.argv[1:]] or SOURCES
    argued = safe()
    used = set()
    failed = False
    for rule in sorted(RULES.glob("*.cocci")):
        for source in sources:
            found = matches(rule, source)
            keys = {(rule.name, *match) for match in matched_lines(found)}
            used |= keys
            unargued = sorted(keys - argued.keys())
            if unargued:
                print(f"{rule.name} matches in {source}:\n{found}", flush=True)
                failed = True
    if sources == SOURCES:
        for rule, file, function, code in sorted(argued.keys() - used):
            print(f"scripts/cocci/safe: {rule} matches no `{code}` in {file}'s {function}")
            failed = True
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
