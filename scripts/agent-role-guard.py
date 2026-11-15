#!/usr/bin/env python3
# Copyright 2026 The fxdax Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Keeps an agent to one side of the line between the code and what checks it.

An agent that writes both a change and the tests judging it can make them agree by weakening the
tests; models do, stronger ones more (ImpossibleBench, doc-17). When the work is split, one agent
writing the tests and another the code, this holds each to its side. A Claude Code PreToolUse hook
(.claude/settings.json) runs it for every edit and shell command, once the worktree's
`.agent-role` (git ignores it) says the session's role:

- implement: the code only. Tests, specs, mutants and the lists of known failures are the other
  side's (CHECKS below); an edit elsewhere that takes away a #[test] or an assertion is too.
- test: what checks only. Code it may touch only to declare a test module (`#[cfg(test)]`,
  `mod tests;`).

Without the file, it allows everything, as for any session not split. A role holds for the session:
nothing touching `.agent-role` is allowed. It reads the hook's JSON on stdin and exits 2 to refuse,
saying why on stderr, which Claude Code shows the agent; 0 to allow. Shell commands are judged by
what they name (paths the role may not write, with a command or redirection that writes): a guard
against slips and shortcuts, not against an agent set on getting round it. TASK-444.9.
"""

import json
import os
import re
import shlex
import sys
from pathlib import Path

CHECKS = re.compile(
    r"(^|/)tests?/|_tests?\.rs$|(^|/)tests\.rs$|(^|/)test_[^/]*\.py$|_test\.go$"
    r"|^specs/|^guest/mutants/|^guest/sim/(mutants|tests)/"
    r"|^guest/sim/known-failures$|^scripts/pjdfstest-fuse-known$|^crates/fxfs-posix/known-panics$")
CODE = re.compile(r"\.(rs|c|h|go|py|sh|tla|cfg)$")
TEST_MARKS = re.compile(
    r"#\[\s*([\w:]+::)?test\b|assert(_eq|_ne|_matches)?!|debug_assert\w*!|prop_assert\w*!"
    r"|\b(BUG_ON|WARN_ON\w*|lockdep_assert\w*)\s*\(|self\.assert\w+\s*\(")
DECLARATION = re.compile(r"^\s*(#\[cfg\(test\)\]|(pub(\(\w+\))?\s+)?mod\s+\w+;)?\s*$")
WRITES = re.compile(
    r"\b(sed\s+(-\w+\s+)*-i|perl\s+-\w*i|tee|mv|cp|rm|truncate|patch|install|ln"
    r"|git\s+(checkout|restore|apply|rm|mv|reset|am|cherry-pick))\b|\bopen\([^)]*['\"][wa]")
REDIRECT = re.compile(r"(?<![0-9&=-])>{1,2}\s*([^\s&|;<>]+)")
ROLE_FILE = ".agent-role"


def refuse(why):
    print(f"agent-role-guard: {why} (scripts/agent-role-guard.py, TASK-444.9)", file=sys.stderr)
    return 2


def relative(path, root):
    try:
        return Path(path).resolve().relative_to(root).as_posix()
    except ValueError:
        return None


def checks(path):
    return bool(CHECKS.search(path))


def removes_marks(old, new):
    """Whether new has fewer tests or assertions than old."""
    return len(TEST_MARKS.findall(old)) > len(TEST_MARKS.findall(new))


def only_declarations(old, new):
    """Whether new is old with only test-module declarations added."""
    old_lines, new_lines = old.splitlines(), new.splitlines()
    rest = list(new_lines)
    for line in old_lines:
        if line not in rest:
            return False
        rest.remove(line)
    return all(DECLARATION.match(line) for line in rest)


def edits(tool, tool_input, root):
    """What an edit tool changes: (path, old text, new text) per change."""
    path = tool_input.get("file_path") or tool_input.get("notebook_path") or ""
    if tool == "Edit":
        return path, [(tool_input.get("old_string", ""), tool_input.get("new_string", ""))]
    if tool == "MultiEdit":
        return path, [(e.get("old_string", ""), e.get("new_string", ""))
                      for e in tool_input.get("edits", [])]
    if tool == "Write":
        file = Path(path)
        old = file.read_text(errors="replace") if file.is_file() else ""
        return path, [(old, tool_input.get("content", ""))]
    return path, [("", tool_input.get("new_source", ""))]


def judge_edit(role, tool, tool_input, root):
    path, changes = edits(tool, tool_input, root)
    name = relative(path, root)
    if name is None:
        return 0
    if name == ROLE_FILE:
        return refuse(f"the session's role ({role}) is fixed; {ROLE_FILE} isn't to be edited")
    if role == "implement":
        if checks(name):
            return refuse(f"{name} checks the code; an implementing agent leaves it to the "
                          "agent writing the tests")
        if any(removes_marks(old, new) for old, new in changes):
            return refuse(f"this edit of {name} takes away a test or an assertion; an "
                          "implementing agent leaves those to the agent writing the tests")
    elif role == "test":
        if CODE.search(name) and not checks(name) and \
                not all(only_declarations(old, new) for old, new in changes):
            return refuse(f"{name} is code; a test-writing agent changes only what checks it, "
                          "and declares test modules")
    return 0


def judge_command(role, command, root):
    if ROLE_FILE in command:
        return refuse(f"the session's role ({role}) is fixed; {ROLE_FILE} isn't to be touched")
    try:
        words = shlex.split(command, comments=True)
    except ValueError:
        words = command.split()
    forbidden = (lambda name: checks(name)) if role == "implement" else \
        (lambda name: bool(CODE.search(name)) and not checks(name))
    named = [name for word in words
             if (name := relative(root / word if not os.path.isabs(word) else word, root))
             and forbidden(name)]
    targets = [name for target in REDIRECT.findall(command)
               if (name := relative(root / target if not os.path.isabs(target) else target, root))
               and forbidden(name)]
    side = "what checks the code" if role == "implement" else "code"
    if targets:
        return refuse(f"this command writes {targets[0]}, {side}, which a {role} agent leaves "
                      "to the other side")
    if named and WRITES.search(command):
        return refuse(f"this command may write {named[0]}, {side}, which a {role} agent leaves "
                      "to the other side")
    return 0


def main():
    root = Path(os.environ.get("CLAUDE_PROJECT_DIR") or os.getcwd()).resolve()
    role_file = root / ROLE_FILE
    if not role_file.is_file():
        return 0
    role = role_file.read_text().strip()
    if role not in ("implement", "test"):
        return refuse(f"{ROLE_FILE} says {role!r}; a role is 'implement' or 'test'")
    hook = json.load(sys.stdin)
    tool, tool_input = hook.get("tool_name", ""), hook.get("tool_input", {})
    if tool == "Bash":
        return judge_command(role, tool_input.get("command", ""), root)
    if tool in ("Edit", "MultiEdit", "Write", "NotebookEdit"):
        return judge_edit(role, tool, tool_input, root)
    return 0


if __name__ == "__main__":
    sys.exit(main())
