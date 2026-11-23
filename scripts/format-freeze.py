#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Refuses commits that break a frozen format, unless the user agreed to the break.

A family of formats is frozen once its golden corpus is under formats/<slug>/ (doc-28, board
10033): every later build reads every version from its oldest on. A commit may add cases (a new
version's), and raise a family's current version. It may not:

- corpus-changed: change, delete or move away a file under formats/ (a merge: one none of its
  parents held so);
- oldest-raised: raise a frozen family's oldest version (`formats::Format::new(NAME, OLDEST,
  CURRENT)`), which stops reading what it wrote;
- current-lowered: lower a frozen family's current version;
- declaration-removed: drop a frozen family's declaration;
- not-literal: declare any family with other than literals, which this can't read;
- declared-twice: declare a family's name in two places, which this can't tell apart.

Only a `Breaks-format: hotbed question N` line in the commit's message lets one through: the
question in which the user agreed to that break.

scripts/hooks/commit-msg runs it on what is staged; CI on what each push or pull request brings.
Exits 1, saying each, if a commit breaks a frozen format and its message names no agreement.

Usage: scripts/format-freeze.py --staged [--message FILE]
       scripts/format-freeze.py RANGE
"""

import argparse
import re
import subprocess
import sys

TRAILER = re.compile(r"^Breaks-format:\s*hotbed question \d+", re.MULTILINE)
CORPUS = "formats/"
CALL = re.compile(r"\bFormat::new\(")
DECLARATION = re.compile(r'\bFormat::new\(\s*"([^"]+)"\s*,\s*(\d+)\s*,\s*(\d+)\s*\)')


INDEX = None  # The index, as a tree: what is staged.


def git(*args, ok=(0,)):
    done = subprocess.run(["git", *args], capture_output=True, text=True)
    if done.returncode not in ok:
        sys.exit(f"git {' '.join(args)}: {done.stderr.strip()}")
    return done.stdout


def slug(name):
    """A family's corpus directory, as formats::Format::slug makes it."""
    return "".join(c.lower() if c.isascii() and c.isalnum() else "-" for c in name)


def corpus(tree):
    """Each file under formats/ at `tree` (a revision, or INDEX), by its path: its blob."""
    if tree is INDEX:
        listed = git("ls-files", "-s", "--", CORPUS)
        return {path: meta.split()[1] for meta, path in
                (line.split("\t", 1) for line in listed.splitlines())}
    listed = git("ls-tree", "-r", tree, "--", CORPUS)
    return {path: meta.split()[2] for meta, path in
            (line.split("\t", 1) for line in listed.splitlines())}


def declarations(tree):
    """Each family declared in the Rust of `tree` (a revision, or INDEX): its name's (oldest,
    current); and each declaration this can't read, as its place's flag."""
    where = ["--cached"] if tree is INDEX else []
    revision = [] if tree is INDEX else [tree]
    found = git("grep", "-n", *where, "-E", r"Format::new\(", *revision, "--", "*.rs", ok=(0, 1))
    families, unreadable = {}, set()
    for line in found.splitlines():
        if tree is not INDEX:
            line = line[len(tree) + 1:]
        place, _, text = line.partition(":")
        number, _, text = text.partition(":")
        code = text.split("//", 1)[0]
        if not CALL.search(code):
            continue
        match = DECLARATION.search(code)
        if not match:
            unreadable.add(f"{place}: not-literal: declare a format with literals alone")
        elif match[1] in families:
            unreadable.add(f"{place}: declared-twice: one declaration a family")
        else:
            families[match[1]] = (int(match[2]), int(match[3]))
    return families, unreadable


def broken(parent, after):
    """What `after` breaks of what was frozen at `parent`, each a tree."""
    found = set()
    held, now = corpus(parent), corpus(after)
    for path, blob in held.items():
        if now.get(path) != blob:
            found.add(f"{path}: corpus-changed ({'M' if path in now else 'D'})")
    frozen = {path[len(CORPUS):].split("/", 1)[0] for path in held}
    (old, unread_before), (new, unread_now) = declarations(parent), declarations(after)
    found |= unread_now - unread_before
    for name, (oldest, current) in old.items():
        if slug(name) not in frozen:
            continue
        if name not in new:
            found.add(f"{name}: declaration-removed")
            continue
        new_oldest, new_current = new[name]
        if new_oldest > oldest:
            found.add(f"{name}: oldest-raised ({oldest} to {new_oldest})")
        if new_current < current:
            found.add(f"{name}: current-lowered ({current} to {new_current})")
    return found


def flags(parents, after):
    """What `after` breaks: what it breaks of every one of its parents. A merge breaks only what
    none of its parents brought: what main landed, its own agreement with it, is no break of a
    branch merging it; an evil merge's own change is."""
    found = set.intersection(*(broken(parent, after) for parent in parents))
    return sorted(found)


def commits(revisions):
    """Each commit of the range, oldest first, merges among them: its name, what it breaks, and
    its message."""
    for line in git("rev-list", "--reverse", "--parents", revisions).splitlines():
        sha, *parents = line.split()
        if not parents:
            continue
        yield sha[:12], flags(parents, sha), git("log", "-1", "--format=%B", sha)


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
        merging = git("rev-parse", "-q", "--verify", "MERGE_HEAD", ok=(0, 1)).split()
        checked = [("staged", flags(["HEAD", *merging], INDEX), message)]
    else:
        checked = commits(args.range)

    status = 0
    for name, found, message in checked:
        if not found:
            continue
        agreed = TRAILER.search(message)
        print(f"{name}: breaks a frozen format, " +
              ("as the user agreed:" if agreed else "and names no agreement of the user's:"))
        for flag in found:
            print(f"  {flag}")
        if not agreed:
            print("  A frozen format is read at every version from its oldest on (doc-28): add "
                  "a new version and keep reading the old. Only the user agrees to a break: a "
                  "'Breaks-format: hotbed question N' line naming where.")
            status = 1
    return status


if __name__ == "__main__":
    sys.exit(main())
