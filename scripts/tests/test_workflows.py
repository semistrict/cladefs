# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""What CI runs changes by a commit alone (board 8603): every action a workflow or a composite
action of .github uses is pinned by its full commit, its release named beside it, as a tag or
branch can move under it and run with the workflows' caches and artifacts; every workflow's token
reads the repository and nothing more unless a job says why; and Rust is the version
rust-toolchain.toml pins, CI's and the test hosts' alike, the one nightly guest/rust's dated one:
`dtolnay/rust-toolchain@stable` had new rustc and clippy releases change CI under it (board 2292,
TASK-274)."""

import re
import subprocess
import unittest
from pathlib import Path

from script import SCRIPTS

ROOT = SCRIPTS.parent
GITHUB = ROOT / ".github"
# `uses: OWNER/REPO[/PATH]@COMMIT # vX.Y.Z`, or a local action or workflow.
PINNED = re.compile(r"^[\w.-]+/[\w.-]+(/[\w./-]+)?@[0-9a-f]{40} # v\d+(\.\d+){1,2}$")


def uses():
    """Each `uses:` of .github's YAML: (file, value)."""
    for path in sorted(GITHUB.rglob("*.yml")):
        for value in re.findall(r"^\s*(?:- )?uses: (.+)$", path.read_text(), re.MULTILINE):
            yield path.relative_to(ROOT), value.strip()


def channel(path):
    return re.search(r'^channel = "(.+)"$', (ROOT / path).read_text(), re.MULTILINE).group(1)


class Workflows(unittest.TestCase):
    def test_every_action_is_pinned_by_commit(self):
        unpinned = [(str(path), value) for path, value in uses()
                    if not value.startswith("./") and not PINNED.match(value)]
        self.assertEqual(unpinned, [])

    def test_one_commit_per_action(self):
        """An action's steps all run one release of it: actions/cache's restore and save too."""
        commits = {}
        for _, value in uses():
            if not value.startswith("./"):
                repo = "/".join(value.split("@")[0].split("/")[:2])
                commits.setdefault(repo, set()).add(value.split("@")[1])
        self.assertEqual({repo: len(seen) for repo, seen in commits.items() if len(seen) > 1}, {})

    def test_every_workflow_reads_the_repository_alone(self):
        for path in sorted((GITHUB / "workflows").glob("*.yml")):
            with self.subTest(path.name):
                self.assertRegex(path.read_text(), r"(?m)^permissions:\n  contents: read\n")

    def test_rust_is_rust_toolchain_tomls(self):
        floating = []
        for path in sorted(GITHUB.rglob("*.yml")):
            for line in path.read_text().splitlines():
                if re.search(r"rust-toolchain@|toolchain install (stable|nightly|beta)\b", line):
                    floating.append((str(path.relative_to(ROOT)), line.strip()))
        self.assertEqual(floating, [])
        self.assertRegex(channel("rust-toolchain.toml"), r"^\d+\.\d+\.\d+$")
        self.assertRegex(channel("guest/rust/rust-toolchain.toml"), r"^nightly-\d{4}-\d{2}-\d{2}$")

    def test_the_test_hosts_build_with_it(self):
        hosts = subprocess.run(
            ["bash", "-c", 'source "$0"; pinned_version rust',
             SCRIPTS / "lib" / "linux-host-setup.sh"],
            check=True, capture_output=True, text=True).stdout.strip()
        self.assertEqual(hosts, channel("rust-toolchain.toml"))


if __name__ == "__main__":
    unittest.main()
