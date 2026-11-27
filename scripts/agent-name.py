#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Prints this worktree's agent name, the board profile its sessions write as (`hotbed board ... --as NAME`).

The name is two words, adjective-noun (`amber-heron`), kept in `.agent-name` at the worktree's
root (git ignores it). The first run picks one at random, from about 200,000, and writes it; every
later run prints the same one. So each worktree's claims and replies on the board are its own,
where a shared `claude` would let one session settle another's. The words are petname's
(scripts/agent-names/README.md).
"""

import random
import subprocess
from pathlib import Path

FILE = ".agent-name"

WORDS = Path(__file__).resolve().parent / "agent-names"
ADJECTIVES = (WORDS / "adjectives.txt").read_text().split()
NOUNS = (WORDS / "nouns.txt").read_text().split()


def name(cwd="."):
    """The name of the worktree holding `cwd`, picked and written on the first call."""
    root = subprocess.run(["git", "rev-parse", "--show-toplevel"], cwd=cwd, check=True,
                          capture_output=True, text=True).stdout.strip()
    path = Path(root) / FILE
    if path.is_file() and (kept := path.read_text().strip()):
        return kept
    chosen = f"{random.choice(ADJECTIVES)}-{random.choice(NOUNS)}"
    path.write_text(f"{chosen}\n")
    return chosen


if __name__ == "__main__":
    print(name())
