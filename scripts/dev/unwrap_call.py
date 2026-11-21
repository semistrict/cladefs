#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Turns calls of a blocking wrapper into awaits (board 2948): `wait_on(f(x))` becomes
`f(x).await`, and an argument that is no postfix expression, `(a + b).await`. Usage:
unwrap_call.py CALLEE FILE..., CALLEE as written at the call, `local_cell::upload::wait_on` say;
each of its spellings is given on its own."""

import re
import sys
from pathlib import Path

OPENERS, CLOSERS = "([{", ")]}"


def argument_end(text, start):
    """The index of the `)` closing the call whose `(` is just before `start`."""
    depth, at, quote = 0, start, None
    while at < len(text):
        char = text[at]
        if quote:
            if char == "\\":
                at += 2
                continue
            if char == quote:
                quote = None
        elif char == '"':
            quote = char
        elif char in OPENERS:
            depth += 1
        elif char in CLOSERS:
            if depth == 0:
                return at
            depth -= 1
        at += 1
    raise ValueError("unclosed call")


def postfix(argument):
    """Whether `argument` is a postfix expression, as `.await` may follow without parentheses:
    nothing at its top level but paths, calls, fields and indexes."""
    depth, quote = 0, None
    for char in argument:
        if quote:
            if char == quote:
                quote = None
            continue
        if char == '"':
            quote = char
        elif char in OPENERS:
            depth += 1
        elif char in CLOSERS:
            depth -= 1
        elif depth == 0 and not (char.isalnum() or char in "_.:!?"):
            return False
    return True


def unwrap(text, callee):
    call = re.compile(rf"(?<![\w:]){re.escape(callee)}\(")
    out, at, count = [], 0, 0
    while True:
        found = call.search(text, at)
        if not found:
            out.append(text[at:])
            return "".join(out), count
        start = found.end()
        end = argument_end(text, start)
        argument = text[start:end].strip().rstrip(",")
        awaited = f"{argument}.await" if postfix(argument) else f"({argument}).await"
        out.append(text[at: found.start()])
        out.append(awaited)
        at = end + 1
        count += 1


if __name__ == "__main__":
    callee = sys.argv[1]
    for name in sys.argv[2:]:
        path = Path(name)
        text, count = unwrap(path.read_text(), callee)
        if count:
            path.write_text(text)
            print(f"{name}: {count}")
