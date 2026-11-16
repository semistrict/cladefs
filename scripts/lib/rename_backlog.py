# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Renames the backlog through its CLI, for the rename scripts (scripts/rename-to-*.sh): the docs'
titles and bodies, and the open tasks' titles, descriptions, acceptance criteria and plans (their
notes are records), each text as the Perl in $RENAME rewrites it, line by line.

    RENAME='s/old/new/g' python3 scripts/lib/rename_backlog.py [--skip TASK-N]...

--skip leaves a task, and its subtasks, as they are: a rename's own, which names both."""

import argparse
import json
import os
import subprocess


def renamed(text):
    return subprocess.run(["perl", "-pe", os.environ["RENAME"]], input=text, capture_output=True,
                          text=True, check=True).stdout


def backlog(*args):
    return subprocess.run(["backlog", *args], capture_output=True, text=True, check=True).stdout


def skipped(task_id, skip):
    return any(task_id == s or task_id.startswith(s + ".") for s in skip)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--skip", action="append", default=[])
    skip = parser.parse_args().skip

    # Docs: each title as the backlog reads it, its front matter's YAML value (quoted, folded),
    # not the YAML's text, and set only if the rename changes it; each body, after the front
    # matter.
    docs = json.loads(backlog("search", "--type", "document", "--json"))["results"]
    for doc in sorted((r["data"] for r in docs), key=lambda d: int(d["id"].removeprefix("doc-"))):
        body = open(doc["path"]).read().split("---\n", 2)[2]
        args = []
        for flag, text in (("--title", doc["title"]), ("--content", body)):
            if renamed(text) != text:
                args += [flag, renamed(text)]
        if args:
            backlog("doc", "update", doc["id"], *args)
            print(doc["id"], "renamed")

    listed = json.loads(backlog("task", "list", "--exclude-status", "Done", "--json"))["tasks"]
    for brief in (t for t in listed if not skipped(t["id"], skip)):
        task = json.loads(backlog("task", "view", brief["id"], "--json"))["task"]
        args = []
        for field, flag in (("title", "-t"), ("description", "-d"),
                            ("implementationPlan", "--plan")):
            if task.get(field) and renamed(task[field]) != task[field]:
                args += [flag, renamed(task[field])]
        criteria = task.get("acceptanceCriteria") or []
        if any(renamed(c["text"]) != c["text"] for c in criteria):
            for c in criteria:
                args += ["--acceptance-criteria", renamed(c["text"])]
        if not args:
            continue
        backlog("task", "edit", task["id"], *args)
        checked = [str(c["index"]) for c in criteria if c["checked"]]
        if criteria and "--acceptance-criteria" in args and checked:
            backlog("task", "edit", task["id"], *[a for i in checked for a in ("--check-ac", i)])
        print(task["id"], "renamed")


if __name__ == "__main__":
    main()
