# Copyright 2026 The fxdax Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/review-change.sh, a second vendor's review of a change (TASK-444.9): Codex,
read-only, given the verifying-reviewer agent's instructions and the change, with a fake codex
that keeps what it was asked."""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

from script import SCRIPTS

REVIEW = SCRIPTS / "review-change.sh"
AGENT = SCRIPTS.parent / ".claude" / "agents" / "verifying-reviewer.md"

FAKE_CODEX = """#!/bin/bash
printf '%s\\n' "$@" > "$FAKE/args"
cat > "$FAKE/stdin"
while (($#)); do
    if [[ $1 == --output-last-message ]]; then echo "no findings" > "$2"; fi
    shift
done
echo "codex ran"
"""


class ReviewChangeTest(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.fake = Path(scratch.name)
        (self.fake / "bin").mkdir()
        codex = self.fake / "bin" / "codex"
        codex.write_text(FAKE_CODEX)
        codex.chmod(0o755)

    def review(self, *args):
        return subprocess.run(
            [REVIEW, *args, "--out", str(self.fake / "review.md")], capture_output=True, text=True,
            env={**os.environ, "PATH": f"{self.fake / 'bin'}:{os.environ['PATH']}",
                 "FAKE": str(self.fake)})

    def test_codex_reviews_a_commit_read_only_with_the_agents_instructions(self):
        done = self.review("--commit", "abc123", "--task", "TASK-7")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout, "no findings\n")
        args = (self.fake / "args").read_text().splitlines()
        self.assertEqual(args[:3], ["exec", "--sandbox", "read-only"])
        self.assertIn("-", args)
        asked = (self.fake / "stdin").read_text()
        body = AGENT.read_text().split("---\n", 2)[2]
        self.assertTrue(asked.startswith(body.rstrip("\n")), asked[:200])
        self.assertNotIn("name: verifying-reviewer", asked)
        self.assertIn("The change to review: the commit abc123 (`git show abc123`). "
                      "It serves TASK-7.", asked)
        self.assertEqual((self.fake / "review.md.log").read_text(), "codex ran\n")

    def test_a_base_names_what_head_brings(self):
        self.assertEqual(self.review("--base", "origin/main").returncode, 0)
        self.assertIn("what HEAD brings beyond origin/main (`git diff origin/main...HEAD`",
                      (self.fake / "stdin").read_text())

    def test_no_change_named_fails(self):
        done = self.review()
        self.assertEqual(done.returncode, 2)
        self.assertIn("give --commit SHA, --base BRANCH or --uncommitted", done.stderr)


if __name__ == "__main__":
    unittest.main()
