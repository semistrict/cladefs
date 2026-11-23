# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/review-change.sh, a second vendor's review of a change (TASK-444.9): Codex,
read-only, given the verifying-reviewer agent's instructions and the change, with a fake codex
and fake hotbed that record what they were asked."""

import json
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
    if [[ $1 == --output-last-message ]]; then
        if [[ -n ${FAKE_FINDINGS:-} ]]; then
            cat > "$2" <<'REPORT'
One finding survives review.

```json hotbed-findings
{"findings": [{"file": "src/lib.rs", "line": 42, "short_summary": "Null check missing", "summary": "Missing check", "failure_scenario": "Pass empty input", "category": "correctness", "severity": "important", "verdict": "CONFIRMED"}]}
```
REPORT
        elif [[ -n ${FAKE_NONE:-} ]]; then
            printf 'no findings\n\n```json hotbed-findings\n{"findings": []}\n```\n' > "$2"
        else
            echo "no findings" > "$2"
        fi
    fi
    shift
done
echo "codex ran"
"""

FAKE_HOTBED = """#!/bin/bash
printf '%s\\n' "$*" >> "$FAKE/hotbed.log"
# Its answer: JSON for --json, else plain text, as hotbed prints.
answer() {
    if [[ " $args " == *" --json "* ]]; then echo "$1"; else echo "plain text: ask with --json"; fi
}
args=$*
case "$1 $2" in
    "session current")
        if [[ -n ${FAKE_NO_SESSION:-} ]]; then exit 1; fi
        answer '{"version":"1","ok":true,"command":"session current","data":{"id":"test-sess"}}'
        ;;
    "session start")
        answer '{"version":"1","ok":true,"command":"session start","data":{"id":"new-sess"}}'
        ;;
    "session end")
        answer '{"version":"1","ok":true,"command":"session end","data":{}}'
        ;;
    "review request")
        answer '{"version":"1","ok":true,"command":"review request","data":{"id":77}}'
        ;;
    "review start"|"review finish"|"review fail")
        answer '{"version":"1","ok":true,"command":"review","data":{"id":77}}'
        ;;
    "review findings")
        while (($#)); do
            if [[ $1 == --file ]]; then cp "$2" "$FAKE/findings.json"; fi
            shift
        done
        answer '{"version":"1","ok":true,"command":"review findings","data":{"findings":[{"id":1}]}}'
        ;;
    "review link")
        answer '{"version":"1","ok":true,"command":"review link","data":{"url":"http://hotbed.local/reviews/77"}}'
        ;;
    *)
        echo "unexpected hotbed args: $*" >&2
        exit 1
        ;;
esac
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
        hotbed = self.fake / "bin" / "hotbed"
        hotbed.write_text(FAKE_HOTBED)
        hotbed.chmod(0o755)

    def review(self, *args, extra_env=None):
        env = {
            **os.environ,
            "PATH": f"{self.fake / 'bin'}:{os.environ['PATH']}",
            "FAKE": str(self.fake),
            "CODEX_MODEL": "test-model",
        }
        env.pop("HOTBED_SESSION", None)
        if extra_env:
            env.update(extra_env)
        return subprocess.run(
            [REVIEW, *args, "--out", str(self.fake / "review.md")],
            capture_output=True, text=True, env=env,
        )

    def test_codex_reviews_a_commit_read_only_with_the_agents_instructions(self):
        done = self.review("--commit", "abc123", "--task", "TASK-7", "--no-hotbed")
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
        self.assertEqual(self.review("--base", "origin/main", "--no-hotbed").returncode, 0)
        self.assertIn("what HEAD brings beyond origin/main (`git diff origin/main...HEAD`",
                      (self.fake / "stdin").read_text())

    def test_records_review_and_findings_in_hotbed_by_default(self):
        done = self.review("--base", "origin/main", "--task", "board 6902",
                           extra_env={"FAKE_FINDINGS": "1"})
        self.assertEqual(done.returncode, 0, done.stderr)
        findings = f"{self.fake / 'review.md.findings.json'}"
        self.assertEqual((self.fake / "hotbed.log").read_text().splitlines(), [
            "session current --json",
            "review request -s test-sess --title Codex review: origin/main (board 6902) "
            "--instructions verifying-reviewer instructions; what HEAD brings beyond origin/main "
            "(`git diff origin/main...HEAD`, `git log origin/main..HEAD`), serving board 6902 "
            "--base origin/main --head HEAD --json",
            "review start -s test-sess 77 --model test-model",
            f"review findings -s test-sess 77 --file {findings}",
            "review finish -s test-sess 77 --summary Codex review, 1 finding(s): "
            "One finding survives review.",
            "review link 77 --json",
        ])
        self.assertIn("This is hotbed review 77.", (self.fake / "stdin").read_text())
        # Codex runs on the model the review records.
        args = (self.fake / "args").read_text().splitlines()
        self.assertEqual(args[:5], ["exec", "-m", "test-model", "--sandbox", "read-only"])
        self.assertIn("hotbed review 77, http://hotbed.local/reviews/77", done.stderr)
        recorded = json.loads((self.fake / "findings.json").read_text())
        self.assertEqual([f["file"] for f in recorded["findings"]], ["src/lib.rs"])

    def test_a_report_without_findings_for_hotbed_fails_the_review(self):
        done = self.review("--commit", "abc123", "--review", "77")
        self.assertEqual(done.returncode, 1)
        self.assertEqual(done.stdout, "no findings\n")
        self.assertEqual((self.fake / "hotbed.log").read_text().splitlines(), [
            "session current --json",
            "review start -s test-sess 77 --model test-model",
            "review fail -s test-sess 77 --reason Codex's report carries no findings for hotbed "
            f"(see {self.fake / 'review.md'})",
        ])

    def test_no_findings_finishes_the_review_without_reporting_any(self):
        done = self.review("--commit", "abc123", "--review", "77", extra_env={"FAKE_NONE": "1"})
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual((self.fake / "hotbed.log").read_text().splitlines(), [
            "session current --json",
            "review start -s test-sess 77 --model test-model",
            "review finish -s test-sess 77 --summary Codex review, 0 finding(s): no findings",
            "review link 77 --json",
        ])

    def test_without_a_session_one_is_started_and_ended(self):
        done = self.review("--uncommitted", "--review", "77",
                           extra_env={"FAKE_FINDINGS": "1", "FAKE_NO_SESSION": "1"})
        self.assertEqual(done.returncode, 0, done.stderr)
        log = (self.fake / "hotbed.log").read_text().splitlines()
        self.assertEqual(log[:3], ["session current --json", "session start --json",
                                   "review start -s new-sess 77 --model test-model"])
        self.assertEqual(log[-1], "session end -s new-sess")

    def test_no_change_named_fails(self):
        done = self.review()
        self.assertEqual(done.returncode, 2)
        self.assertIn("give --commit SHA, --base BRANCH or --uncommitted", done.stderr)


if __name__ == "__main__":
    unittest.main()
