#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""TASK-508.3: folds extent_frame's `Branch { cell: Cell { tenant, id }, id }` and `Cell { tenant, id }`
into one type, `CellId { tenant, id }`, in the Rust files given: a cell's ID is its branch's (doc-20).

    merge-cell-branch.py FILE...

Rewrites `Branch { cell: C, id: I }` literals to `CellId { tenant: <C's tenant>, id: I }`, `.cell.tenant`
to `.tenant`, `.cell.id` to `.id` and `.cell.prefix()` to `.prefix()`, and the type names `Branch` and
`Cell` to `CellId`. What it can't rewrite, the compiler finds: a `.cell` left on a `CellId`.
"""

import re
import sys


def split_top(text, sep=","):
    """`text` split at `sep` outside brackets."""
    parts, depth, start = [], 0, 0
    for i, ch in enumerate(text):
        if ch in "({[":
            depth += 1
        elif ch in ")}]":
            depth -= 1
        elif ch == sep and depth == 0:
            parts.append(text[start:i])
            start = i + 1
    parts.append(text[start:])
    return [part for part in parts if part.strip()]


def closing(text, open_at):
    """The index of the brace closing the one at `open_at`."""
    depth = 0
    for i in range(open_at, len(text)):
        if text[i] in "({[":
            depth += 1
        elif text[i] in ")}]":
            depth -= 1
            if depth == 0:
                return i
    raise ValueError("unbalanced")


def tenant_of(cell):
    """The tenant of the cell expression `cell`, as an expression."""
    cell = cell.strip()
    found = re.fullmatch(r"(?:[\w:]+::)?Cell\s*\{(.*)\}", cell, re.S)
    if found:
        for field in split_top(found.group(1)):
            name, _, value = field.partition(":")
            if name.strip() == "tenant":
                return value.strip() if value else "tenant"
    return f"{cell}.tenant" if re.fullmatch(r"[\w.]+(\(\))?", cell) else f"({cell}).tenant"


def literals(text):
    """`text` with each `Branch { cell, id }` literal a `CellId { tenant, id }` one."""
    out, at = [], 0
    pattern = re.compile(r"(?<![A-Za-z0-9_])((?:[A-Za-z_][\w]*::)*)Branch\s*\{")
    while True:
        found = pattern.search(text, at)
        if not found:
            out.append(text[at:])
            return "".join(out)
        brace = found.end() - 1
        end = closing(text, brace)
        fields = {}
        rest = []
        for field in split_top(text[brace + 1 : end]):
            name, colon, value = field.partition(":")
            name = name.strip()
            if name in ("cell", "id"):
                fields[name] = value.strip() if colon else name
            else:
                rest.append(field.strip())
        if set(fields) != {"cell", "id"} or rest:
            out.append(text[at : end + 1])
            at = end + 1
            continue
        prefix = found.group(1)
        out.append(text[at : found.start()])
        out.append(f"{prefix}CellId {{ tenant: {tenant_of(fields['cell'])}, id: {fields['id']} }}")
        at = end + 1


def merged(text):
    text = literals(text)
    text = text.replace(".cell.tenant", ".tenant").replace(".cell.id", ".id")
    text = text.replace(".cell.prefix()", ".prefix()")
    text = re.sub(r"(?<![A-Za-z0-9_])Branch(?![A-Za-z0-9_])", "CellId", text)
    if not re.search(r"use std::cell::(Cell\b|\{[^}]*\bCell\b)", text):
        keep = r"(?<!std::cell::)(?<!BranchOf::)(?<!Scope::)(?<!Memory::)(?<!MemBackendType::)"
        lines = []
        for line in text.split("\n"):
            # An enum's variant named Cell, declared: kept, what it holds renamed.
            variant = re.match(r"^(\s*)Cell(\(|,|\s*=|\s*\{)", line)
            head, tail = (line[: len(variant.group(1)) + 4], line[len(variant.group(1)) + 4 :]) if variant else ("", line)
            tail = re.sub(keep + r"(?<![A-Za-z0-9_])Cell(?![A-Za-z0-9_])", "CellId", tail)
            lines.append(head + tail)
        text = "\n".join(lines)
    # Imports naming both, now one.
    text = re.sub(r"\bCellId,\s*CellId\b", "CellId", text)
    text = re.sub(r"\{CellId\}", "CellId", text)
    return text


for path in sys.argv[1:]:
    with open(path) as f:
        text = f.read()
    new = merged(text)
    if new != text:
        with open(path, "w") as f:
            f.write(new)
        print(path)
