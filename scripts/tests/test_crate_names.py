# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""The crates' names keep AGENTS.md's convention (Crate names; hotbed question 621, board 14599):
a crate of cladefs's own is named with underscores, one copied from Fuchsia as it came, and each
is in the directory of its name."""

import re
import unittest
from pathlib import Path

CRATES = Path(__file__).resolve().parent.parent.parent / "crates"
# A crate's name as Rust code says it: what cladefs's own are named.
UNDERSCORED = re.compile(r"[a-z][a-z0-9_]*")
# cladefs's own crates named before the convention, until board 14600 renames them. None is added:
# one renamed leaves the list.
HYPHENATED = {
    "cladefs-examples", "fxfs-posix", "fxfs-sim-diff", "libmachine-agent", "libmachine-firecracker",
}


def crates():
    """Each crate's directory's name, its package's name, and whether it is copied from Fuchsia, as
    its manifest's head says (`Fuchsia's `src/...``)."""
    for path in sorted(CRATES.iterdir()):
        manifest = path / "Cargo.toml"
        if not manifest.is_file():
            continue
        text = manifest.read_text()
        package = re.search(r'^\[package\]\nname = "([^"]+)"$', text, re.MULTILINE).group(1)
        head = "\n".join(text.splitlines()[:8])
        yield path.name, package, bool(re.search(r"Fuchsia's `(?:src|zircon)/", head))


class CrateNames(unittest.TestCase):
    def test_a_crate_is_in_the_directory_of_its_name(self):
        self.assertEqual([(directory, package) for directory, package, _ in crates()
                          if directory != package], [])

    def test_cladefs_own_crates_are_named_with_underscores(self):
        own = {package for _, package, fuchsias in crates() if not fuchsias}
        self.assertEqual(sorted(name for name in own if not UNDERSCORED.fullmatch(name)),
                         sorted(HYPHENATED))


if __name__ == "__main__":
    unittest.main()
