# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/rename-to-cladefs.sh --backlog's docs (TASK-459): it renames each doc's title as
the backlog reads it, its front matter's YAML value, quoted or folded, and sets a title only if the
rename changes it; in a repository of its own, against a backlog that answers its search with the
docs' titles as the test says and records each call."""

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from script import SCRIPTS

SCRIPT = SCRIPTS / "rename-to-cladefs.sh"

# backlog: `search` answers $DOCS, `task list` no tasks; each call's words a JSON line in $CALLS.
FAKE_BACKLOG = """\
import json, os, sys
with open(os.environ["CALLS"], "a") as calls:
    calls.write(json.dumps(sys.argv[1:]) + "\\n")
if sys.argv[1] == "search":
    print(open(os.environ["DOCS"]).read())
elif sys.argv[1:3] == ["task", "list"]:
    print(json.dumps({"tasks": []}))
"""

# Each doc: its front matter's title as YAML text, as the backlog reads it, and its body.
DOCS = {
    # Quoted, its value not renamed, its body renamed: the case that came back quoted twice.
    "doc-1": ("'TASK-26.1 spike: porting Fxfs into the guest''s Linux kernel'",
              "TASK-26.1 spike: porting Fxfs into the guest's Linux kernel",
              "fxdax's guest.\n"),
    # Folded over two lines: the case that came back '>-'.
    "doc-2": (">-\n  Spending frontier tokens on quality: what the forefront does, and what fits\n  fxdax",
              "Spending frontier tokens on quality: what the forefront does, and what fits fxdax",
              "Nothing to rename.\n"),
    # Quoted, its value renamed.
    "doc-3": ("'Upgrading fxdax: the operator''s guide'",
              "Upgrading fxdax: the operator's guide",
              "Run FXDAX_KMS=1 fxdax.\n"),
    # Nothing to rename.
    "doc-4": ("'Copy-on-write DAX: the host''s part'", "Copy-on-write DAX: the host's part", "DAX.\n"),
}


class RenameBacklogDocsTest(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.dir = Path(scratch.name)
        repo = self.dir / "repo"
        (repo / "backlog/docs").mkdir(parents=True)
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        results = []
        for id_, (yaml, title, body) in DOCS.items():
            path = f"backlog/docs/{id_} - doc.md"
            (repo / path).write_text(f"---\nid: {id_}\ntitle: {yaml}\ntype: other\n---\n{body}")
            results.append({"type": "document", "data": {"id": id_, "title": title, "path": path}})
        docs = self.dir / "docs.json"
        docs.write_text(json.dumps({"schemaVersion": 1, "kind": "search", "results": results}))
        bin_ = self.dir / "bin"
        bin_.mkdir()
        (bin_ / "fake_backlog.py").write_text(FAKE_BACKLOG)
        (bin_ / "backlog").write_text(f'#!/bin/sh\nexec {sys.executable} "{bin_}/fake_backlog.py" "$@"\n')
        (bin_ / "backlog").chmod(0o755)
        self.calls = self.dir / "calls"
        env = {**os.environ, "PATH": f"{bin_}:{os.environ['PATH']}", "CALLS": str(self.calls),
               "DOCS": str(docs)}
        subprocess.run([SCRIPT, "--backlog"], cwd=repo, env=env, check=True, capture_output=True)

    def test_docs_titles_are_renamed_as_values_and_set_only_when_renamed(self):
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()]
        updates = [call for call in calls if call[:2] == ["doc", "update"]]
        self.assertEqual(updates, [
            ["doc", "update", "doc-1", "--content", "cladefs's guest.\n"],
            ["doc", "update", "doc-2", "--title",
             "Spending frontier tokens on quality: what the forefront does, and what fits cladefs"],
            ["doc", "update", "doc-3", "--title", "Upgrading cladefs: the operator's guide",
             "--content", "Run CLADEFS_KMS=1 cladefs.\n"],
        ])

    def test_the_backlog_is_asked_for_the_docs_and_the_open_tasks(self):
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()]
        self.assertEqual([call for call in calls if call[:2] != ["doc", "update"]], [
            ["config", "set", "projectName", "cladefs"],
            ["search", "--type", "document", "--json"],
            ["task", "list", "--exclude-status", "Done", "--json"],
        ])


if __name__ == "__main__":
    unittest.main()
