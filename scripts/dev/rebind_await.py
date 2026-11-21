#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Moves an `.await` from a future's uses to its binding (board 2944): `let x = f();` then
`x.await.y` becomes `let x = f().await;` then `x.y`, where rustc's suggestion awaits each use.
Usage: rebind_await.py FILE IDENT..."""

import re
import sys
from pathlib import Path


def rebind(file, idents):
    """Rebinds each of `idents` in `file`: returns how many bindings it awaited."""
    lines = file.read_text().split("\n")
    changed = sum(rebind_one(file, lines, ident) for ident in idents)
    file.write_text("\n".join(lines))
    return changed


def rebind_one(file, lines, ident):
    """Awaits each binding of `ident` some use awaits, single statements alone, and strips the
    uses' awaits through to its next binding."""
    use = re.compile(rf"\b{ident}\.await\b")
    bind = re.compile(rf"^\s*let (mut )?{ident}\b[^=]*=")
    changed, at = 0, 0
    while at < len(lines):
        if not use.search(lines[at]) or bind.match(lines[at]):
            at += 1
            continue
        bound = at - 1
        while bound >= 0 and not bind.match(lines[bound]):
            bound -= 1
        if bound < 0:
            print(f"{file}:{at + 1}: no binding of {ident}")
            at += 1
            continue
        end = bound
        while not lines[end].rstrip().endswith(";"):
            end += 1
        if not lines[end].rstrip().endswith(".await;"):
            lines[end] = re.sub(r";\s*$", ".await;", lines[end])
            print(f"{file}:{bound + 1}: {ident} bound awaited")
            changed += 1
        at = end + 1
        while at < len(lines) and not bind.match(lines[at]):
            lines[at] = use.sub(ident, lines[at])
            at += 1
    return changed


def await_binding(file, ident, line):
    """Awaits the binding of `ident` nearest above `line`, from 1, a future its use there takes for
    its output: returns whether it changed it."""
    lines = file.read_text().split("\n")
    bind = re.compile(rf"^\s*let (mut )?\(?{ident}\b[^=]*=")
    bound = line - 1
    while bound >= 0 and not bind.match(lines[bound]):
        bound -= 1
    if bound < 0:
        print(f"{file}:{line}: no binding of {ident}")
        return False
    end = bound
    while not lines[end].rstrip().endswith(";"):
        end += 1
    if lines[end].rstrip().endswith(".await;") or "|" in lines[bound].split("=", 1)[1][:12]:
        return False
    lines[end] = re.sub(r";\s*$", ".await;", lines[end])
    print(f"{file}:{bound + 1}: {ident} bound awaited")
    file.write_text("\n".join(lines))
    return True


if __name__ == "__main__":
    rebind(Path(sys.argv[1]), sys.argv[2:])
