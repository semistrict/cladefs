# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/test-weakening.py, which flags commits weakening a check unless their message
says why, and of scripts/hooks/commit-msg, which runs it on what is staged (TASK-444.9): in a
repository of their own, a commit before and one after."""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

from script import SCRIPTS

CHECKER = SCRIPTS / "test-weakening.py"
HOOK = SCRIPTS / "hooks" / "commit-msg"

TESTS = """\
#[test]
fn appends() {
    let log = Log::new();
    log.append(b"a");
    assert_eq!(log.len(), 1);
    assert!(log.bytes() < 4096);
}

#[test]
fn truncates() {
    let log = Log::new();
    log.truncate(0);
    assert!(log.is_empty());
}
"""

CONFIG = """\
SPECIFICATION Spec
CONSTANTS
    Hosts = {h1, h2}
    MaxTakes = 2
INVARIANTS
    NoAckedLost
    KeysWrittenOnce
CHECK_DEADLOCK FALSE
"""


class TestWeakeningTest(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.repo = Path(scratch.name)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Test")
        self.git("config", "user.email", "test@example.com")
        self.write("crates/log/src/tests.rs", TESTS)
        self.write("specs/bucket/Bucket.cfg", CONFIG)
        self.write("specs/bucket/mutations/NoFences.cfg", CONFIG)
        self.write("guest/sim/known-failures", "# task, then failure\n")
        self.write("scripts/zloop.sh", 'seeds=3000\n')
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
        self.git("commit", "-q", "--allow-empty", "-m", message)
        return self.git("rev-parse", "HEAD")

    def check(self):
        """The checker over what came after the base: its status and what it said."""
        ran = subprocess.run([CHECKER, f"{self.base}..HEAD"], cwd=self.repo, capture_output=True,
                             text=True)
        return ran.returncode, ran.stdout

    def assert_flags(self, rule, line):
        status, said = self.check()
        self.assertEqual(status, 1, said)
        self.assertIn(f": {rule}", said)
        self.assertIn(line, said)
        self.assertIn("its message doesn't say why", said)

    def test_a_removed_assertion_is_flagged(self):
        self.edit("crates/log/src/tests.rs", "    assert!(log.is_empty());\n", "")
        self.commit("truncates: no check")
        self.assert_flags("assertion-removed (1 removed, 0 added)",
                          "crates/log/src/tests.rs:13: assertion-removed")

    def test_an_assertion_restated_is_not(self):
        self.edit("crates/log/src/tests.rs", "    assert!(log.is_empty());\n",
                  "    assert_eq!(log.len(), 0);\n")
        self.commit("truncates: the length")
        self.assertEqual(self.check(), (0, ""))

    def test_an_assertion_moved_to_another_file_is_not(self):
        self.edit("crates/log/src/tests.rs", "    assert!(log.is_empty());\n", "")
        self.write("crates/log/tests/truncate.rs", "fn check(log: &Log) {\n"
                   "    assert!(log.is_empty());\n}\n")
        self.commit("truncate's check in a file of its own")
        self.assertEqual(self.check(), (0, ""))

    def test_a_bound_loosened_is_flagged(self):
        self.edit("crates/log/src/tests.rs", "log.bytes() < 4096", "log.bytes() < 8192")
        self.commit("bigger logs")
        self.assert_flags("bound-loosened", "crates/log/src/tests.rs:6")

    def test_a_bound_tightened_or_an_expected_value_changed_is_not(self):
        self.edit("crates/log/src/tests.rs", "log.bytes() < 4096", "log.bytes() < 1024")
        self.edit("crates/log/src/tests.rs", "assert_eq!(log.len(), 1)", "assert_eq!(log.len(), 2)")
        self.commit("smaller logs, two entries")
        self.assertEqual(self.check(), (0, ""))

    def test_an_ignored_test_is_flagged(self):
        self.edit("crates/log/src/tests.rs", "#[test]\nfn truncates",
                  '#[test]\n#[ignore = "flaky"]\nfn truncates')
        self.commit("truncates ignored")
        self.assert_flags("test-ignored", '#[ignore = "flaky"]')

    def test_a_removed_test_is_flagged(self):
        text = (self.repo / "crates/log/src/tests.rs").read_text()
        self.write("crates/log/src/tests.rs", text[:text.index("\n#[test]\nfn truncates") + 1])
        self.commit("truncates gone")
        self.assert_flags("test-removed (1 #[test] more removed than added)",
                          "crates/log/src/tests.rs:9: test-removed")

    def test_a_lowered_count_is_flagged(self):
        self.edit("scripts/zloop.sh", "seeds=3000", "seeds=300")
        self.commit("fewer seeds")
        self.assert_flags("count-lowered", "scripts/zloop.sh:1: count-lowered: seeds=300")

    def test_a_raised_count_is_not(self):
        self.edit("scripts/zloop.sh", "seeds=3000", "seeds=30000")
        self.commit("more seeds")
        self.assertEqual(self.check(), (0, ""))

    def test_a_narrowed_generator_is_flagged(self):
        self.write("crates/log/src/model_tests.rs", "proptest! {\n"
                   "    fn appends(n in 0..1000usize) {\n        run(n);\n    }\n}\n")
        self.commit("model test")
        self.base = self.git("rev-parse", "HEAD")
        self.edit("crates/log/src/model_tests.rs", "0..1000usize", "0..10usize")
        self.edit("crates/log/src/model_tests.rs", "        run(n);\n",
                  "        prop_assume!(n != 7);\n        run(n);\n")
        self.commit("smaller")
        status, said = self.check()
        self.assertEqual(status, 1, said)
        self.assertIn("model_tests.rs:2: generator-narrowed: fn appends(n in 0..10usize) {", said)
        self.assertIn("model_tests.rs:3: generator-narrowed: prop_assume!(n != 7);", said)

    def test_a_known_failure_added_is_flagged(self):
        self.edit("guest/sim/known-failures", "\n", "\nTASK-9 interleave seed 12\n")
        self.commit("known failure")
        self.assert_flags("known-failure-added", "TASK-9 interleave seed 12")

    def test_an_invariant_removed_is_flagged(self):
        self.edit("specs/bucket/Bucket.cfg", "    KeysWrittenOnce\n", "")
        self.commit("Bucket: one invariant less")
        self.assert_flags("invariant-removed", "specs/bucket/Bucket.cfg:7: invariant-removed")

    def test_a_constant_lowered_is_flagged_and_one_reordered_is_not(self):
        self.edit("specs/bucket/Bucket.cfg", "MaxTakes = 2", "MaxTakes = 1")
        self.edit("specs/bucket/Bucket.cfg", "    Hosts = {h1, h2}\n", "")
        self.edit("specs/bucket/Bucket.cfg", "CONSTANTS\n", "CONSTANTS\n    Hosts = {h1, h2}\n")
        self.commit("Bucket: one take")
        status, said = self.check()
        self.assertEqual(status, 1, said)
        self.assertIn("spec-config-narrowed: MaxTakes = 1", said)
        self.assertNotIn("Hosts", said)

    def test_a_mutant_removed_is_flagged(self):
        (self.repo / "specs/bucket/mutations/NoFences.cfg").unlink()
        self.commit("NoFences gone")
        self.assert_flags("mutant-removed", "specs/bucket/mutations/NoFences.cfg:0")

    def test_a_message_saying_why_lets_it_be(self):
        self.edit("crates/log/src/tests.rs", "    assert!(log.is_empty());\n", "")
        self.commit("truncates: no check\n\nWeakens-tests: TASK-12, truncate keeps its head")
        status, said = self.check()
        self.assertEqual(status, 0, said)
        self.assertIn("weakens checks, as its message says why:", said)

    def test_every_commit_of_the_range_is_checked_for_itself(self):
        self.edit("crates/log/src/tests.rs", "    assert!(log.is_empty());\n", "")
        first = self.commit("truncates: no check")
        self.edit("crates/log/src/tests.rs", "fn truncates() {\n",
                  "fn truncates() {\n    assert!(Log::new().is_empty());\n")
        self.commit("a check back, elsewhere")
        status, said = self.check()
        self.assertEqual(status, 1, said)
        self.assertIn(f"{first[:12]}: weakens checks", said)

    def test_the_hook_refuses_a_staged_weakening_and_takes_a_reason(self):
        self.git("config", "core.hooksPath", str(HOOK.parent))
        self.edit("crates/log/src/tests.rs", "    assert!(log.is_empty());\n", "")
        self.git("add", "-A")
        refused = subprocess.run(["git", "-C", self.repo, "commit", "-q", "-m", "no check"],
                                 capture_output=True, text=True,
                                 env={**os.environ, "GIT_DIR": str(self.repo / ".git")})
        self.assertEqual(refused.returncode, 1, refused.stdout + refused.stderr)
        self.assertIn("staged: weakens checks, and its message doesn't say why", refused.stderr)
        self.git("commit", "-q", "-m", "no check\n\nWeakens-tests: TASK-12")
        self.assertEqual(self.git("log", "-1", "--format=%s"), "no check")


if __name__ == "__main__":
    unittest.main()
