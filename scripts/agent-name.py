#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Prints this worktree's agent name, the board profile its sessions write as (`vb ... --as NAME`).

The name is two words, adjective-noun (`amber-heron`), kept in `.agent-name` at the worktree's
root (git ignores it). The first run picks one at random, from thousands, and writes it; every
later run prints the same one. So each worktree's claims and replies on the board are its own,
where a shared `claude` would let one session settle another's.
"""

import random
import subprocess
from pathlib import Path

FILE = ".agent-name"

ADJECTIVES = """
    amber ample ashen azure bold brave brisk calm candid clever coral crisp curious deft eager
    fair fleet frank gentle gilded glad golden grand hardy honest humble jade keen kind lively
    lucid mellow merry misty nimble noble patient placid plucky proud quick quiet rapid ready
    rosy rugged russet sage scarlet serene sharp silver sleek sly snowy sober solid spry stark
    steady stout sunny swift tawny tidy tranquil valiant vivid wary warm wild wily wise witty
    """.split()

NOUNS = """
    acorn alder anvil aspen badger beacon birch bison bramble brook cedar comet condor cove
    crane creek dune eagle ember falcon fern finch fjord fox gannet glacier grove harbor hawk
    hazel heron hollow ibis juniper kestrel lantern larch lark lynx maple marten meadow mesa
    moss newt oak orca osprey otter owl pebble pine plover quail raven reef ridge robin rowan
    sparrow spruce summit swan thistle tern thrush tide willow wren
    """.split()


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
