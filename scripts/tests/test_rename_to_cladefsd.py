# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/rename-to-cladefsd.sh (TASK-475.1): what it renames and what it keeps, by its
--filter, which renames text as it renames the checkout's files; and the backlog helper's --skip,
which leaves the rename's own tasks as they are."""

import importlib.util
import subprocess
import unittest

from script import SCRIPTS

SCRIPT = SCRIPTS / "rename-to-cladefsd.sh"


def filtered(text):
    return subprocess.run([SCRIPT, "--filter"], input=text, capture_output=True, text=True,
                          check=True).stdout


class Filter(unittest.TestCase):
    def test_the_binary_variables_and_metrics_are_renamed(self):
        self.assertEqual(
            filtered("CLADEFS_EXTENTD_REMOTE=s3://b target/debug/cladefs-extentd SOCKET\n"
                     "CARGO_BIN_EXE_cladefs-extentd cladefs_extentd_requests_total\n"
                     'std::env::var_os("CLADEFS_EXTENTD")\n'),
            "CLADEFSD_BUCKET=s3://b target/debug/cladefsd SOCKET\n"
            "CARGO_BIN_EXE_cladefsd cladefsd_requests_total\n"
            'std::env::var_os("CLADEFSD")\n',
        )

    def test_the_extent_service_keeps_its_names(self):
        kept = ("cladefs::extentd::serve, cladefs.extentd.v1.Extentd, go/extentd/v1, "
                "extentd_socket, CLADEFS_LOG_LEVEL\n")
        self.assertEqual(filtered(kept), kept)


def rename_backlog():
    spec = importlib.util.spec_from_file_location("rename_backlog",
                                                  SCRIPTS / "lib" / "rename_backlog.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class Skip(unittest.TestCase):
    def test_a_task_skipped_is_skipped_with_its_subtasks_alone(self):
        skipped = rename_backlog().skipped
        self.assertEqual(
            [task for task in ["TASK-475", "TASK-475.1", "TASK-475.2.1", "TASK-4750", "TASK-47"]
             if skipped(task, ["TASK-475"])],
            ["TASK-475", "TASK-475.1", "TASK-475.2.1"],
        )


if __name__ == "__main__":
    unittest.main()
