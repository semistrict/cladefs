#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""cladefs: the branches a simulation's seeds never take (board 2842, TASK-685.10).

Reads the JSON exports of coverage runs (`cargo llvm-cov report --branch --json`, which
scripts/sim-coverage.sh writes), of this checkout's sources, wherever they ran, and prints, in
Markdown, for each source file whose path holds one of the given patterns: its branch sides and
how many no run took, then every side no run took, by the function it is in, with its line; those
of the file's own unit tests (a top-level `#[cfg(test)]` module) aside. Given several exports, a
side is taken if any run took it: what no simulation reaches. A side is a condition's outcome:
`true` never taken is a condition always false, `false` never taken one always true; `neither`, a
condition never reached.

    scripts/coverage-branches.py COVERAGE.json... PATTERN...
"""

import json
import re
import sys
from collections import defaultdict
from pathlib import Path

FN = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+(\w+)")


def enclosing_function(lines: list[str], line: int) -> tuple[int, str]:
    """The line and name of the function whose definition is the nearest above `line` (1-based)."""
    for at in range(line - 1, -1, -1):
        found = FN.match(lines[at])
        if found:
            return at + 1, found.group(1)
    return 0, "(top level)"


def tests_start(lines: list[str]) -> int:
    """The line (1-based) where the file's own unit tests begin, a top-level `#[cfg(test)]` module,
    which a simulation never runs: past the end if it has none."""
    for at, line in enumerate(lines):
        if line.startswith("#[cfg(test)]"):
            following = next((rest for rest in lines[at + 1 :] if not rest.startswith("#[")), "")
            if re.match(r"(pub(\([^)]*\))? )?mod \w+ \{", following):
                return at + 1
    return len(lines) + 1


ROOT = Path(__file__).resolve().parent.parent


def source(filename: str) -> tuple[str, Path]:
    """A source file's name from its crate on, and its path in this checkout."""
    name = re.sub(r"^.*/crates/", "", filename)
    return name, ROOT / "crates" / name


def merged(exports: list[dict], patterns: list[str]) -> dict[str, dict[tuple[int, int], list[int]]]:
    """Each matching file's branch sides, by (line, column): how often each was taken and not, over
    every export, once however many records name it (a branch in a macro's expansion is recorded
    once per expansion)."""
    files: dict[str, dict[tuple[int, int], list[int]]] = defaultdict(lambda: defaultdict(lambda: [0, 0]))
    for export in exports:
        for each in export["data"][0]["files"]:
            if not any(pattern in each["filename"] for pattern in patterns):
                continue
            name, _ = source(each["filename"])
            for line, column, _end_line, _end_column, taken, not_taken, *_ in each["branches"]:
                counts = files[name][(line, column)]
                counts[0] += taken
                counts[1] += not_taken
    return files


def untaken(sides: dict[tuple[int, int], list[int]]) -> list[tuple[int, int, str]]:
    """Each branch side never taken, as (line, column, side)."""
    found = []
    for (line, column), (taken, not_taken) in sorted(sides.items()):
        if taken == 0 and not_taken == 0:
            found.append((line, column, "neither"))
        elif taken == 0:
            found.append((line, column, "true"))
        elif not_taken == 0:
            found.append((line, column, "false"))
    return found


def main() -> int:
    exports = [arg for arg in sys.argv[1:] if arg.endswith(".json")]
    patterns = [arg for arg in sys.argv[1:] if not arg.endswith(".json")]
    if not exports or not patterns:
        print(__doc__, file=sys.stderr)
        return 2
    files = merged([json.loads(Path(export).read_text()) for export in exports], patterns)
    report = []
    print("| File | Branch sides | Not taken | Taken |")
    print("|---|---:|---:|---:|")
    for name in sorted(files):
        lines = (ROOT / "crates" / name).read_text().splitlines()
        end = tests_start(lines)
        sides = {at: counts for at, counts in files[name].items() if at[0] < end}
        missed = untaken(sides)
        total = 2 * len(sides)
        not_taken = sum(2 if side == "neither" else 1 for _, _, side in missed)
        share = 100 * (total - not_taken) / total if total else 100.0
        print(f"| {name} | {total} | {not_taken} | {share:.1f}% |")
        report.append((name, lines, missed))
    for name, lines, missed in report:
        if not missed:
            continue
        print(f"\n## {name}\n")
        by_function = defaultdict(list)
        for line, column, side in missed:
            by_function[enclosing_function(lines, line)].append((line, column, side))
        for (at, function), found in sorted(by_function.items()):
            print(f"- `{function}` ({at}):")
            for line, column, side in found:
                text = lines[line - 1].strip()
                print(f"  - {line}:{column} never {side}: `{text[:100]}`")
    return 0


if __name__ == "__main__":
    sys.exit(main())
