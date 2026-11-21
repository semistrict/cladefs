# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/agent-name.py: each worktree's board profile, adjective-noun, picked once,
kept in its gitignored `.agent-name`."""

import subprocess
import tempfile
import unittest
from pathlib import Path

from script import SCRIPTS, load

agent_name = load("agent-name")
ROOT = SCRIPTS.parent


def git(cwd, *args):
    subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True)


class AgentNameTest(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.scratch = Path(scratch.name).resolve()
        self.main = self.scratch / "main"
        self.main.mkdir()
        git(self.main, "init", "-q")
        git(self.main, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q",
            "--allow-empty", "-m", "root")
        (self.main / "sub").mkdir()

    def worktree(self, name):
        path = self.scratch / name
        git(self.main, "worktree", "add", "-q", "--detach", str(path))
        return path

    def test_first_run_writes_an_adjective_noun_name(self):
        name = agent_name.name(self.main / "sub")
        adjective, noun = name.split("-")
        self.assertIn(adjective, agent_name.ADJECTIVES)
        self.assertIn(noun, agent_name.NOUNS)
        self.assertEqual((self.main / ".agent-name").read_text(), f"{name}\n")

    def test_later_runs_keep_the_written_name(self):
        (self.main / ".agent-name").write_text("quiet-otter\n")
        self.assertEqual(agent_name.name(self.main), "quiet-otter")

    def test_each_worktree_has_its_own_file(self):
        other = self.worktree("other")
        (self.main / ".agent-name").write_text("quiet-otter\n")
        name = agent_name.name(other)
        self.assertEqual((other / ".agent-name").read_text(), f"{name}\n")
        self.assertEqual((self.main / ".agent-name").read_text(), "quiet-otter\n")

    def test_word_lists_are_two_hundred_distinct_lowercase_words_or_more(self):
        for words in (agent_name.ADJECTIVES, agent_name.NOUNS):
            self.assertGreaterEqual(len(words), 200)
            self.assertEqual(len(set(words)), len(words))
            self.assertEqual([w for w in words if not (w.isascii() and w.isalpha() and w.islower())],
                             [])

    def test_git_ignores_the_file(self):
        self.assertIn(".agent-name", (ROOT / ".gitignore").read_text().splitlines())


if __name__ == "__main__":
    unittest.main()
