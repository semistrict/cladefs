# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/format-freeze.py, which refuses commits breaking a frozen format unless the
user agreed (doc-28, board 10036), and of scripts/hooks/commit-msg, which runs it on what is
staged: in a repository of their own, a commit before and one after."""

import subprocess
import tempfile
import unittest
from pathlib import Path

from script import SCRIPTS

CHECKER = SCRIPTS / "format-freeze.py"
HOOK = SCRIPTS / "hooks" / "commit-msg"

LAYOUT = """\
/// The bucket's objects.
pub const FORMAT: formats::Format = formats::Format::new("bucket layout", 28, 28);
"""

HOST = """\
pub const FORMAT: Format = Format::new("ephemeral host", 2, 3); // not frozen yet
"""


class FormatFreezeTest(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.repo = Path(scratch.name)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Test")
        self.git("config", "user.email", "test@example.com")
        self.write("crates/bucket_cell/src/layout.rs", LAYOUT)
        self.write("crates/cladefs/src/ephemeral_host.rs", HOST)
        self.write("formats/bucket-layout/28/head.bin", "\x1c\x00\x00\x00")
        self.write("formats/bucket-layout/28/head.txt", "HeadV28 { .. }")
        self.base = self.commit("base")

    def git(self, *args):
        return subprocess.run(["git", "-C", self.repo, *args], check=True, capture_output=True,
                              text=True).stdout.strip()

    def write(self, path, text):
        file = self.repo / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(text)

    def edit(self, path, old, new):
        file = self.repo / path
        text = file.read_text()
        self.assertIn(old, text)
        file.write_text(text.replace(old, new, 1))

    def commit(self, message):
        self.git("add", "-A")
        self.git("commit", "-q", "--allow-empty", "--no-verify", "-m", message)
        return self.git("rev-parse", "HEAD")

    def check(self):
        ran = subprocess.run([CHECKER, f"{self.base}..HEAD"], cwd=self.repo, capture_output=True,
                             text=True)
        return ran.returncode, ran.stdout

    def assert_refused(self, *flags):
        status, said = self.check()
        self.assertEqual(status, 1, said)
        for flag in flags:
            self.assertIn(f"  {flag}\n", said)
        self.assertIn("names no agreement of the user's", said)

    def test_a_new_version_with_its_cases_is_not_refused(self):
        self.edit("crates/bucket_cell/src/layout.rs", '"bucket layout", 28, 28', '"bucket layout", 28, 29')
        self.write("formats/bucket-layout/29/head.bin", "\x1d\x00\x00\x00")
        self.write("formats/bucket-layout/29/head.txt", "HeadV29 { .. }")
        self.commit("bucket layout 29")
        self.assertEqual(self.check(), (0, ""))

    def test_a_changed_case_is_refused(self):
        self.write("formats/bucket-layout/28/head.txt", "HeadV28 { slots: 3 }")
        self.commit("head reads otherwise")
        self.assert_refused("formats/bucket-layout/28/head.txt: corpus-changed (M)")

    def test_a_deleted_or_moved_case_is_refused(self):
        self.git("rm", "-q", "formats/bucket-layout/28/head.txt")
        self.git("mv", "formats/bucket-layout/28/head.bin", "formats/bucket-layout/28/old-head.bin")
        self.commit("old heads gone")
        status, said = self.check()
        self.assertEqual(status, 1, said)
        self.assertIn("formats/bucket-layout/28/head.txt: corpus-changed (D)", said)
        self.assertIn("formats/bucket-layout/28/head.bin: corpus-changed (D)", said)

    def test_raising_oldest_or_lowering_current_is_refused(self):
        self.edit("crates/bucket_cell/src/layout.rs", '"bucket layout", 28, 28', '"bucket layout", 29, 27')
        self.commit("bucket layout narrowed")
        self.assert_refused("bucket layout: oldest-raised (28 to 29)",
                            "bucket layout: current-lowered (28 to 27)")

    def test_dropping_a_frozen_declaration_is_refused(self):
        self.write("crates/bucket_cell/src/layout.rs", "pub const FORMAT: u32 = 28;\n")
        self.commit("back to a number")
        self.assert_refused("bucket layout: declaration-removed")

    def test_a_declaration_not_of_literals_is_refused(self):
        self.edit("crates/cladefs/src/ephemeral_host.rs", "2, 3)", "OLDEST, 3)")
        self.commit("a named oldest")
        self.assert_refused("crates/cladefs/src/ephemeral_host.rs: not-literal: declare a "
                            "format with literals alone")

    def test_a_family_declared_twice_is_refused(self):
        self.write("crates/cladefs/src/other.rs", 'const F: Format = Format::new("ephemeral host", 1, 1);\n')
        self.commit("a second ephemeral host")
        status, said = self.check()
        self.assertEqual(status, 1, said)
        self.assertIn(": declared-twice: one declaration a family", said)

    def test_a_family_not_frozen_changes_freely(self):
        self.edit("crates/cladefs/src/ephemeral_host.rs", "2, 3)", "3, 3)")
        self.commit("ephemeral host 3 alone")
        self.assertEqual(self.check(), (0, ""))

    def test_the_users_agreement_lets_a_break_through(self):
        self.write("formats/bucket-layout/28/head.txt", "HeadV28 { slots: 3 }")
        self.commit("head reads otherwise\n\nBreaks-format: hotbed question 512")
        status, said = self.check()
        self.assertEqual(status, 0, said)
        self.assertIn("as the user agreed:", said)

    def hook(self, message):
        """The commit-msg hook over what is staged: its status and what it said."""
        file = self.repo / ".git" / "MESSAGE"
        file.write_text(message)
        ran = subprocess.run([HOOK, file], cwd=self.repo, capture_output=True, text=True)
        return ran.returncode, ran.stdout + ran.stderr

    def test_the_hook_refuses_what_is_staged(self):
        self.write("formats/bucket-layout/28/head.txt", "HeadV28 { slots: 3 }")
        self.edit("crates/bucket_cell/src/layout.rs", '"bucket layout", 28, 28', '"bucket layout", 29, 29')
        self.git("add", "-A")
        status, said = self.hook("head reads otherwise\n")
        self.assertEqual(status, 1, said)
        self.assertIn("staged: breaks a frozen format, and names no agreement", said)
        self.assertIn("  bucket layout: oldest-raised (28 to 29)\n", said)
        self.assertIn("  formats/bucket-layout/28/head.txt: corpus-changed (M)\n", said)

    def test_the_hook_lets_what_breaks_nothing_through(self):
        # Hotbed review 65: the index's declarations unread, every staged commit was refused.
        self.write("README.md", "cladefs\n")
        self.edit("crates/cladefs/src/ephemeral_host.rs", "2, 3)", "2, 4)")
        self.git("add", "-A")
        self.assertEqual(self.hook("a readme\n"), (0, ""))

    def branch_breaking_head(self, message):
        """A branch off the base whose commit edits head.txt, its message `message`; main moved
        on meanwhile by a commit of its own. Returns the branch's commit."""
        self.git("checkout", "-q", "-b", "side")
        self.write("formats/bucket-layout/28/head.txt", "HeadV28 { slots: 3 }")
        side = self.commit(message)
        self.git("checkout", "-q", "main")
        self.write("README.md", "cladefs\n")
        self.commit("a readme")
        return side

    def test_a_merge_bringing_an_agreed_break_is_not_refused(self):
        side = self.branch_breaking_head("head reads otherwise\n\nBreaks-format: hotbed question 512")
        self.git("merge", "-q", "--no-ff", "--no-edit", side)
        status, said = self.check()
        self.assertEqual(status, 0, said)
        self.assertNotIn("Merge branch", said)

    def test_an_evil_merge_is_refused(self):
        # Hotbed review 65: merges went unchecked, whatever their own resolution changed.
        side = self.branch_breaking_head("head reads otherwise\n\nBreaks-format: hotbed question 512")
        self.git("merge", "-q", "--no-ff", "--no-commit", side)
        self.write("formats/bucket-layout/28/head.bin", "\x1d\x00\x00\x00")
        self.git("add", "-A")
        self.git("commit", "-q", "--no-verify", "-m", "merge side")
        status, said = self.check()
        self.assertEqual(status, 1, said)
        # The merge breaks head.bin alone: head.txt's change, the side's, the user agreed to.
        merge = said[said.index(self.git("rev-parse", "HEAD")[:12]):]
        self.assertTrue(merge.startswith(
            f"{self.git('rev-parse', 'HEAD')[:12]}: breaks a frozen format, and names no agreement "
            "of the user's:\n  formats/bucket-layout/28/head.bin: corpus-changed (M)\n  A frozen"),
            said)

    def test_the_hook_lets_a_merge_bring_an_agreed_break(self):
        side = self.branch_breaking_head("head reads otherwise\n\nBreaks-format: hotbed question 512")
        self.git("merge", "-q", "--no-ff", "--no-commit", side)
        self.assertEqual(self.hook("merge side\n"), (0, ""))


if __name__ == "__main__":
    unittest.main()
