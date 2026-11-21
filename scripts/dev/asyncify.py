#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Drives a sync-to-async conversion from the compiler's diagnostics (TASK-685.17): marks a
function `async` where an `.await` sits in a sync one (E0728), and applies rustc's "consider
`await`ing" suggestions, round after round, until a round fixes nothing. What is left is printed,
for a person. Usage: asyncify.py [cargo check arguments...]."""

import json
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

import rebind_await

FN = re.compile(r"^(\s*)((?:pub(?:\([\w:]+\))? )?(?:const )?(?:unsafe )?)(?:async )?fn ")


def round_of(args):
    run = subprocess.run(["cargo", "check", "--message-format=json", *args],
                         capture_output=True, text=True)
    asyncs, awaits, rebinds, bindings, left = set(), set(), set(), set(), []
    removes, unwraps = set(), set()
    for line in run.stdout.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("reason") != "compiler-message":
            continue
        diagnostic = message["message"]
        # A future made and dropped, never run, which compiles: a call of a function made async
        # whose result went unused. Awaited.
        if diagnostic["level"] == "warning":
            if re.match(r"unused implementer of `[\w:]*Future`", diagnostic["message"]):
                span = next(span for span in diagnostic["spans"] if span["is_primary"])
                if not span.get("expansion"):
                    awaits.add((span["file_name"], span["line_end"], span["column_end"],
                                ".await"))
            continue
        if diagnostic["level"] != "error":
            continue
        code = (diagnostic.get("code") or {}).get("code")
        primary = next((span for span in diagnostic["spans"] if span["is_primary"]), None)
        fixed = False
        if code == "E0728" and primary:
            enclosing = enclosing_fn(primary["file_name"], primary["line_start"],
                                     primary["column_start"])
            if enclosing is not None:
                asyncs.add((primary["file_name"], enclosing))
                fixed = True
        # A future where its output was meant: awaited at its end. The expression rustc says is
        # a future, primary (`expected u64, found future`) or labelled (`this expression has type
        # impl Future`), not a macro's.
        for span in diagnostic["spans"]:
            label = span.get("label") or ""
            future = (span["is_primary"] and code == "E0308"
                      and re.search(r"found (future|`impl [\w:]*Future)", diagnostic["message"]
                                    + " " + label)
                      and "expected future" not in label + diagnostic["message"])
            future = future or re.search(r"^this expression has type `impl [\w:]*Future", label)
            # Not a binding's name, which a later use's error points at.
            text = span["text"][0]["text"] if span.get("text") else ""
            binding = re.search(r"\blet\s+(mut\s+)?$", text[: span["column_start"] - 1])
            if future and not binding and not span.get("expansion"):
                awaits.add((span["file_name"], span["line_end"], span["column_end"], ".await"))
                fixed = True
        # An operand a future, as its label says: awaited at its end, or, a binding's name,
        # where it is bound. In a macro's expansion (`assert_eq!`'s `*left_val`), the argument of
        # the macro's call it stands for.
        for span in diagnostic["spans"]:
            label = span.get("label") or ""
            if span["is_primary"] or not FUTURE.search(label):
                continue
            operand = operand_of(span)
            if operand is None:
                continue
            file, (line, column), (end_line, end_column), text = operand
            if re.fullmatch(r"&?[a-z_]\w*", text):
                bindings.add((file, text.lstrip("&"), line))
            else:
                awaits.add((file, end_line, end_column, ".await"))
            fixed = True
        # An `.await` of what is no future: a sync closure's call, of a helper's name, awaited
        # in bulk. The `.await` goes; or, the value awaited given to a blocking call
        # (`block_on(f().await)`), that call does.
        if code == "E0277" and primary and "is not a future" in diagnostic["message"]:
            blocking = next((span for span in diagnostic["spans"] if not span["is_primary"]
                             and source(span["file_name"], span) in ("block_on", "wait_on")), None)
            if blocking:
                unwraps.add((blocking["file_name"], blocking["line_start"], blocking["column_start"]))
                fixed = True
            elif source(primary["file_name"], primary) == "await":
                removes.add((primary["file_name"], primary["line_start"], primary["column_start"] - 1))
                fixed = True
        # A future bound, then awaited at more than one use (E0382), or bound borrowing a
        # temporary (E0716): awaited where it is bound instead.
        if code == "E0382" and primary:
            moved = re.match(r"use of moved value: `([a-z_]\w*)`", diagnostic["message"])
            line_text = line_of(primary["file_name"], primary["line_start"])
            if moved and f"{moved.group(1)}.await" in line_text:
                rebinds.add((primary["file_name"], moved.group(1)))
                fixed = True
        if code == "E0716" and primary:
            line_text = line_of(primary["file_name"], primary["line_start"])
            bound = re.match(r"\s*let (mut )?([a-z_]\w*) = ", line_text)
            if bound:
                bindings.add((primary["file_name"], bound.group(2), primary["line_start"] + 1))
                rebinds.add((primary["file_name"], bound.group(2)))
                fixed = True
        # A field of a future's output, or a future where its output is meant, named by its
        # binding: awaited where it is bound.
        if primary and FUTURE_TYPE.search(diagnostic["message"] + " " + (primary.get("label") or "")):
            line_text = line_of(primary["file_name"], primary["line_start"])
            before = line_text[: primary["column_start"] - 1]
            here = line_text[primary["column_start"] - 1: primary["column_end"] - 1]
            named = (re.search(r"(?<![.\w])([a-z_]\w*)\.$", before) if code in ("E0609", "E0599")
                     else re.fullmatch(r"&?\(?([a-z_]\w*)\)?", here))
            if named and named.group(1) != "self" and not fixed:
                bindings.add((primary["file_name"], named.group(1), primary["line_start"]))
                fixed = True
        for child in diagnostic.get("children", []):
            if "await" not in child["message"]:
                continue
            for span in child.get("spans", []):
                # `.await` after the future, or `await.` before the method called on it.
                inserted = span.get("suggested_replacement")
                if inserted in (".await", "await.") and span["column_start"] == span["column_end"]:
                    before = line_of(span["file_name"], span["line_start"])[: span["column_start"] - 1]
                    bare = re.search(r"(?<![.\w])([a-z_]\w*)\.$", before)
                    if inserted == "await." and bare and bare.group(1) != "self":
                        # A binding of a future, used: awaited where it is bound instead.
                        rebinds.add((span["file_name"], bare.group(1)))
                    else:
                        awaits.add((span["file_name"], span["line_start"], span["column_start"],
                                    inserted))
                    fixed = True
        if not fixed:
            left.append(diagnostic["rendered"])
    edits = {("remove",) + spot for spot in removes} | {("unwrap",) + spot for spot in unwraps}
    return asyncs, awaits | edits, rebinds, bindings, left


FUTURE_TYPE = re.compile(r"`?&?impl [\w:]*Future<")
FUTURE = re.compile(r"^(impl [\w:]*Future<|\{async closure body|\(\{async closure body)")


def operand_of(span):
    """The source of the operand `span` labels: (file, start, end, text), its positions from 1;
    for an operand of `assert_eq!`'s, the macro call's argument it stands for. None if neither."""
    if not span.get("expansion"):
        if not ours(span["file_name"]):
            return None
        text = source(span["file_name"], span)
        return (span["file_name"], (span["line_start"], span["column_start"]),
                (span["line_end"], span["column_end"]), text)
    which = {"*left_val": 0, "left_val": 0, "*right_val": 1, "right_val": 1}.get(
        source(span["file_name"], span))
    call = span["expansion"]["span"]
    while call.get("expansion") and not ours(call["file_name"]):
        call = call["expansion"]["span"]
    if which is None or not ours(call["file_name"]):
        return None
    lines = Path(call["file_name"]).read_text().split("\n")
    start = sum(len(line) + 1 for line in lines[: call["line_start"] - 1]) + call["column_start"] - 1
    text = "\n".join(lines)
    at = text.index("(", start) + 1
    arguments, depth, begin = [], 0, at
    while depth >= 0:
        char = text[at]
        if char in "([{":
            depth += 1
        elif char in ")]}":
            depth -= 1
        if (char == "," and depth == 0) or depth < 0:
            arguments.append((begin, at))
            begin = at + 1
        at += 1
    if which >= len(arguments):
        return None
    begin, end = arguments[which]
    while text[begin].isspace():
        begin += 1
    while text[end - 1].isspace():
        end -= 1
    position = lambda offset: (text.count("\n", 0, offset) + 1,
                               offset - (text.rfind("\n", 0, offset) + 1) + 1)
    return (call["file_name"], position(begin), position(end), text[begin:end])


def source(file, span):
    """The text `span` covers, on one line."""
    if not span.get("text"):
        return ""
    return span["text"][0]["text"][span["column_start"] - 1: span["column_end"] - 1]


def enclosing_fn(file, line, column):
    """The line, from 1, of the `fn` whose body holds the text at `line`, `column`, through blocks
    alone (if, match, loop, unsafe, plain): None if a closure or an async block holds it first,
    which a person reworks. Braces in strings and comments are not told apart: good enough for
    code formatted by rustfmt, checked by the compiler after."""
    lines = Path(file).read_text().split("\n")
    text = "\n".join(lines[: line - 1]) + "\n" + lines[line - 1][: column - 1]
    # A named closure of one expression, on the line itself: `let name = |..| expr.await`.
    before = lines[line - 1][: column - 1]
    bare = re.search(r"\blet\s+(mut\s+)?\w+\s*=\s*((move\s+)?\|[^|]*\|)[^{;|]*$", before)
    if bare:
        return ("closure", line, bare.start(2))
    passed = re.search(r"[(,]\s*(&mut\s+)?((move\s+)?\|[^|]*\|)[^{;|]*$", before)
    if passed and not re.search(r"(\.(map|filter|filter_map|flat_map|any|all|for_each|find|"
                                r"find_map|position|fold)|thread::spawn|scope\.spawn)\(\s*"
                                r"(move\s+)?\|[^|]*\|[^{;|]*$", before):
        return ("closure", line, passed.start(2))
    depth = 0
    at = len(text)
    while at > 0:
        at -= 1
        char = text[at]
        if char == "}":
            depth += 1
        elif char == "{":
            if depth:
                depth -= 1
                continue
            # The block's opener: what comes before the brace, back to the statement's start.
            start = max(text.rfind(";", 0, at), text.rfind("{", 0, at), text.rfind("}", 0, at))
            head = text[start + 1: at]
            # A named closure, `let name = |..| {`: made an async closure, whose calls rustc then
            # says to await. Any other closure, or an async block, a person reworks.
            named = re.search(r"\blet\s+(mut\s+)?\w+(\s*:[^=]+)?\s*=\s*((move\s+)?\|[^|]*\|\s*"
                              r"(->[^{]*)?)$", head)
            # Or a closure a helper takes, `phase("x", || {`: made async, the helper then
            # reworked by a person to await it. Not a thread's body, nor an iterator's.
            passed = re.search(r"[(,]\s*(&mut\s+)?((move\s+)?\|[^|]*\|\s*(->[^{]*)?)$", head)
            iterator = re.search(r"\.(map|filter|filter_map|flat_map|any|all|for_each|find|"
                                 r"find_map|position|fold|take_while|skip_while|inspect)\(\s*"
                                 r"(move\s+)?\|[^|]*\|\s*$", head)
            thread = re.search(r"(thread::spawn|scope\.spawn|spawn_blocking)\(\s*(move\s+)?\|"
                               r"[^|]*\|\s*$", head)
            if passed and not iterator and not thread:
                named = passed
                offset = start + 1 + passed.start(2)
                number = text.count("\n", 0, offset) + 1
                return ("closure", number, offset - (text.rfind("\n", 0, offset) + 1))
            if named:
                offset = start + 1 + named.start(3)
                number = text.count("\n", 0, offset) + 1
                return ("closure", number, offset - (text.rfind("\n", 0, offset) + 1))
            if re.search(r"\|[^|]*\|\s*(->[^{]*)?$|\basync\s+(move\s+)?$|\bmove\s+$", head):
                return None
            if re.search(r"\bfn\s+\w+", head):
                number = text.count("\n", 0, start + 1 + head.find("fn ")) + 1
                # The signature's own line: where `fn` is.
                for candidate in range(number - 1, -1, -1):
                    if FN.match(lines[candidate]):
                        return candidate + 1
                return None
    return None


def line_of(file, line):
    """The text of `line`, from 1, of `file`, the workspace's own: empty for another's, as a
    toolchain's macro a span points into."""
    if not ours(file):
        return ""
    return Path(file).read_text().split("\n")[line - 1]


def ours(file):
    """Whether `file` is the workspace's own source, which this edits: never a registry's, a
    toolchain's or a vendored tree's, where a macro's span may point (TASK-719)."""
    path = Path(file).resolve()
    root = Path.cwd().resolve()
    if not path.is_relative_to(root):
        return False
    return path.relative_to(root).parts[0] not in ("target", "vendor", "guest")


def apply(asyncs, awaits):
    asyncs = {spot for spot in asyncs if ours(spot[0])}
    edits = {spot for spot in awaits if spot[0] in ("remove", "unwrap") and ours(spot[1])}
    awaits = {spot for spot in awaits if spot[0] not in ("remove", "unwrap") and ours(spot[0])}
    changed = 0
    for kind, file, line, column in sorted(edits, key=lambda spot: (spot[1], -spot[2], -spot[3])):
        lines = Path(file).read_text().split("\n")
        text = "\n".join(lines)
        offset = sum(len(each) + 1 for each in lines[: line - 1]) + column - 1
        if kind == "remove" and text.startswith(".await", offset):
            text = text[:offset] + text[offset + len(".await"):]
            changed += 1
        if kind == "unwrap":
            callee = re.match(r"\w+\(", text[offset:])
            if callee:
                from unwrap_call import argument_end
                end = argument_end(text, offset + callee.end())
                text = text[:offset] + text[offset + callee.end(): end] + text[end + 1:]
                changed += 1
        Path(file).write_text(text)
    by_file = defaultdict(lambda: ([], []))
    for file, line in asyncs:
        by_file[file][0].append(line)
    for file, line, column, inserted in awaits:
        by_file[file][1].append((line, column, inserted))
    for file, (async_lines, await_spots) in by_file.items():
        lines = Path(file).read_text().split("\n")
        for line, column, inserted in sorted(set(await_spots), reverse=True):
            text = lines[line - 1]
            lines[line - 1] = text[: column - 1] + inserted + text[column - 1:]
            changed += 1
        closures = sorted({spot for spot in async_lines if isinstance(spot, tuple)}, reverse=True)
        for _, line, column in closures:
            text = lines[line - 1]
            if not text[:column].rstrip().endswith("async"):
                lines[line - 1] = text[:column] + "async " + text[column:]
                changed += 1
        for line in {spot for spot in async_lines if not isinstance(spot, tuple)}:
            at = line - 1
            if FN.match(lines[at]) and "async fn" not in lines[at]:
                lines[at] = FN.sub(lambda m: f"{m.group(1)}{m.group(2)}async fn ", lines[at],
                                   count=1)
                changed += 1
                # A test made async runs on a runtime of its own: tokio's, multi-threaded, as
                # `upload::blocking` and the timers a store's waits take need one.
                for above in range(at - 1, -1, -1):
                    attribute = lines[above].strip()
                    if attribute == "#[test]":
                        lines[above] = lines[above].replace(
                            "#[test]", '#[tokio::test(flavor = "multi_thread")]')
                        break
                    if not attribute.startswith(("#[", "//")):
                        break
        Path(file).write_text("\n".join(lines))
    return changed


def main():
    args = sys.argv[1:]
    for number in range(1, 60):
        asyncs, awaits, rebinds, bindings, left = round_of(args)
        changed = apply(asyncs, awaits)
        for file, ident, line in sorted(spot for spot in bindings if ours(spot[0])):
            changed += rebind_await.await_binding(Path(file), ident, line)
        for file, ident in sorted(spot for spot in rebinds if ours(spot[0])):
            changed += rebind_await.rebind(Path(file), [ident])
        print(f"round {number}: {len(asyncs)} made async, {len(awaits)} awaited, "
              f"{len(left)} left", flush=True)
        if not changed:
            for rendered in left:
                print(rendered)
            return 1 if left else 0
    return 1


if __name__ == "__main__":
    sys.exit(main())
