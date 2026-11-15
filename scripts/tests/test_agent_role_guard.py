# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/agent-role-guard.py, the PreToolUse hook that keeps an implementing agent off
what checks the code and a test-writing agent off the code (TASK-444.9), as Claude Code runs it:
the hook's JSON on stdin, the project in CLAUDE_PROJECT_DIR."""

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

from script import SCRIPTS

GUARD = SCRIPTS / "agent-role-guard.py"
ROOT = SCRIPTS.parent


class AgentRoleGuardTest(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.project = Path(scratch.name).resolve()
        (self.project / "crates/log/src").mkdir(parents=True)
        (self.project / "crates/log/src/lib.rs").write_text(
            "pub fn len() -> usize {\n    debug_assert!(true);\n    0\n}\n")

    def role(self, role):
        (self.project / ".agent-role").write_text(f"{role}\n")

    def run_guard(self, tool, **tool_input):
        """The guard's status and what it said, for a call of `tool`."""
        ran = subprocess.run(
            [GUARD], input=json.dumps({"tool_name": tool, "tool_input": tool_input}),
            capture_output=True, text=True,
            env={**os.environ, "CLAUDE_PROJECT_DIR": str(self.project)})
        return ran.returncode, ran.stderr

    def path(self, name):
        return str(self.project / name)

    def test_without_a_role_everything_goes(self):
        self.assertEqual(self.run_guard("Write", file_path=self.path("crates/log/src/tests.rs"),
                                        content=""), (0, ""))

    def test_an_implementer_writes_code(self):
        self.role("implement")
        self.assertEqual(self.run_guard("Edit", file_path=self.path("crates/log/src/lib.rs"),
                                        old_string="    0\n", new_string="    1\n"), (0, ""))

    def test_an_implementer_leaves_tests_specs_and_known_failures(self):
        self.role("implement")
        for name in ["crates/log/src/tests.rs", "crates/log/src/model_tests.rs",
                     "crates/log/tests/append.rs", "scripts/tests/test_bench.py",
                     "specs/bucket/Bucket.tla", "guest/mutants/fix.patch",
                     "guest/sim/known-failures"]:
            with self.subTest(name):
                status, said = self.run_guard("Write", file_path=self.path(name), content="x")
                self.assertEqual(status, 2)
                self.assertIn(f"{name} checks the code", said)

    def test_an_implementer_takes_no_assertion_out_of_code(self):
        self.role("implement")
        status, said = self.run_guard("Edit", file_path=self.path("crates/log/src/lib.rs"),
                                      old_string="    debug_assert!(true);\n", new_string="")
        self.assertEqual(status, 2)
        self.assertIn("takes away a test or an assertion", said)

    def test_an_implementer_rewrites_no_test_file_from_the_shell(self):
        self.role("implement")
        for command in ["sed -i '' s/1/2/ crates/log/src/tests.rs",
                        "echo x > specs/bucket/Bucket.cfg",
                        "git checkout HEAD~1 -- crates/log/tests/append.rs"]:
            with self.subTest(command):
                self.assertEqual(self.run_guard("Bash", command=command)[0], 2)

    def test_an_implementer_reads_and_runs_tests_from_the_shell(self):
        self.role("implement")
        for command in ["rg assert crates/log/src/tests.rs",
                        "cargo nextest run -p log > /tmp/run.log 2>&1",
                        "cat crates/log/src/tests.rs | wc -l"]:
            with self.subTest(command):
                self.assertEqual(self.run_guard("Bash", command=command), (0, ""))

    def test_a_test_writer_leaves_code_but_declares_test_modules(self):
        self.role("test")
        status, said = self.run_guard("Edit", file_path=self.path("crates/log/src/lib.rs"),
                                      old_string="    0\n", new_string="    1\n")
        self.assertEqual(status, 2)
        self.assertIn("crates/log/src/lib.rs is code", said)
        self.assertEqual(self.run_guard(
            "Edit", file_path=self.path("crates/log/src/lib.rs"), old_string="}\n",
            new_string="}\n\n#[cfg(test)]\nmod tests;\n"), (0, ""))
        self.assertEqual(self.run_guard("Write", file_path=self.path("crates/log/src/tests.rs"),
                                        content="#[test]\nfn len() {}\n"), (0, ""))

    def test_no_role_unsets_itself(self):
        self.role("implement")
        self.assertEqual(self.run_guard("Bash", command="rm .agent-role")[0], 2)
        self.assertEqual(self.run_guard("Write", file_path=self.path(".agent-role"),
                                        content="test")[0], 2)

    def test_an_unknown_role_refuses(self):
        self.role("review")
        status, said = self.run_guard("Bash", command="ls")
        self.assertEqual(status, 2)
        self.assertIn("a role is 'implement' or 'test'", said)

    def test_the_project_runs_it_for_edits_and_commands(self):
        settings = json.loads((ROOT / ".claude" / "settings.json").read_text())
        (hook,) = settings["hooks"]["PreToolUse"]
        self.assertEqual(hook["matcher"], "Edit|MultiEdit|Write|NotebookEdit|Bash")
        self.assertIn("scripts/agent-role-guard.py", hook["hooks"][0]["command"])
        self.assertIn(".agent-role", (ROOT / ".gitignore").read_text().splitlines())


if __name__ == "__main__":
    unittest.main()
