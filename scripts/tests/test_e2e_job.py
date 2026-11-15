# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/test-e2e-job.sh, which runs scripts/test-e2e.sh with what it writes, the
cases' stores among it, in the job's temporary directory, on ext4, and copies the results back
(TASK-397): here in a tree of its own, test-e2e-job.sh as it is, and in place of test-e2e.sh one
that says where it ran and leaves what a case leaves, and of findmnt one that says the filesystem
given; and of scripts/bench-rounds.sh's runs, which go through it."""

import subprocess
import tempfile
import unittest
from pathlib import Path

from script import SCRIPTS

# test-e2e.sh, which says in its results what it was given, leaves a case's log, its store, its
# guest's root filesystem and image and the daemon's part cache, and ends with $FAKE_STATUS.
CASES = """#!/bin/bash
cd "$CLADEFS_E2E_RESULTS"
printf '%s\\n' "$CLADEFS_E2E_RESULTS" "${CLADEFS_EXTENTD_REMOTE-no bucket named}" "$@" > given.txt
mkdir -p case/extents case/rootfs/bin .extentd-cache.x1 case/store-1 case/vm.bucket
echo log > case/log.txt
for left in case/extents/segment case/rootfs/bin/sh case/rootfs.ext4 case/disk.img \\
    .extentd-cache.x1/part case/store-1/head case/vm.bucket/object; do
    echo left > "$left"
done
exit "$FAKE_STATUS"
"""
# findmnt, which says every directory is on $FAKE_FSTYPE, or fails if that is empty.
FINDMNT = """#!/bin/bash
echo "$*" > "$FAKE_ASKED"
[[ -n $FAKE_FSTYPE ]] || exit 1
echo "$FAKE_FSTYPE"
"""


def files(directory):
    """The files under `directory`, by their paths in it."""
    return sorted(str(path.relative_to(directory)) for path in directory.rglob("*")
                  if path.is_file())


class Job(unittest.TestCase):
    def job(self, *arguments, fstype="ext4", status=0, tmpdir=True, env=None):
        """Runs test-e2e-job.sh with `arguments` after its results directory, in a job whose
        temporary directory is on `fstype` (none, if `tmpdir` is false), its cases ending with
        `status`: its status and errors."""
        self.tree = tree = Path(tempfile.mkdtemp()).resolve()
        (tree / "scripts").mkdir()
        (tree / "scripts" / "test-e2e-job.sh").symlink_to(SCRIPTS / "test-e2e-job.sh")
        (tree / "bin").mkdir()
        for path, text in [(tree / "scripts" / "test-e2e.sh", CASES),
                           (tree / "bin" / "findmnt", FINDMNT)]:
            path.write_text(text)
            path.chmod(0o755)
        (tree / "tmp").mkdir()
        done = subprocess.run(
            ["/bin/bash", tree / "scripts" / "test-e2e-job.sh", tree / "results", *arguments],
            env={
                "PATH": f"{tree / 'bin'}:/usr/bin:/bin",
                "FAKE_STATUS": str(status),
                "FAKE_FSTYPE": fstype,
                "FAKE_ASKED": str(tree / "asked"),
                **({"TMPDIR": str(tree / "tmp")} if tmpdir else {}),
                **(env or {}),
            },
            capture_output=True,
            text=True,
        )
        return done.returncode, done.stderr

    def given(self):
        """What test-e2e.sh was given: its results directory, its bucket and its arguments."""
        return (self.tree / "results" / "given.txt").read_text().splitlines()

    def left(self):
        """What the tree holds once a job was refused: no results, nothing in the temporary
        directory, and `asked` if findmnt was."""
        self.assertEqual(list((self.tree / "tmp").iterdir()), [])
        return sorted(path.name for path in self.tree.iterdir())

    def test_the_run_writes_in_the_jobs_temporary_directory_and_its_results_come_back(self):
        self.assertEqual(self.job("bench-fs-fxfs-guest-dax"), (0, ""))
        where, bucket, *arguments = self.given()
        # In a directory of its own there, which is gone, as nothing else of the run's is left.
        self.assertEqual(Path(where).parent, self.tree / "tmp")
        self.assertEqual(files(self.tree / "tmp"), [])
        self.assertEqual(list((self.tree / "tmp").iterdir()), [])
        # The bucket is test-e2e.sh's own, on tmpfs: none is named.
        self.assertEqual((bucket, arguments), ("no bucket named", ["bench-fs-fxfs-guest-dax"]))
        self.assertEqual((self.tree / "asked").read_text(), f"-no FSTYPE -T {self.tree / 'tmp'}\n")

    def test_the_stores_caches_and_root_filesystems_are_no_results(self):
        self.assertEqual(self.job("--tag", "bench"), (0, ""))
        self.assertEqual(files(self.tree / "results"), ["case/log.txt", "given.txt"])
        self.assertEqual(self.given()[2:], ["--tag", "bench"])

    def test_a_bucket_named_is_the_runs(self):
        self.assertEqual(self.job("case", env={"CLADEFS_EXTENTD_REMOTE": "s3://bucket/p"}), (0, ""))
        self.assertEqual(self.given()[1], "s3://bucket/p")

    def test_a_failed_runs_results_come_back_with_its_status(self):
        self.assertEqual(self.job("case", status=1), (1, ""))
        self.assertEqual(files(self.tree / "results"), ["case/log.txt", "given.txt"])
        self.assertEqual(list((self.tree / "tmp").iterdir()), [])

    def test_a_temporary_directory_on_another_filesystem_is_refused(self):
        # ubu1's home, where a durable sync measured ZFS's syncs, and /tmp, memory.
        for fstype in ["zfs", "tmpfs"]:
            with self.subTest(fstype):
                status, errors = self.job("case", fstype=fstype)
                self.assertEqual(
                    (status, errors),
                    (2, f"{self.tree / 'tmp'} is on {fstype}: the run's stores go on ext4, as a "
                        "host's (TMPDIR, fxjob's on its scratch disk)\n"))
                self.assertEqual(self.left(), ["asked", "bin", "scripts", "tmp"])

    def test_a_filesystem_findmnt_does_not_name_is_refused(self):
        status, errors = self.job("case", fstype="")
        self.assertEqual(
            (status, errors),
            (2, f"findmnt doesn't say what filesystem {self.tree / 'tmp'} is on\n"))
        self.assertEqual(self.left(), ["asked", "bin", "scripts", "tmp"])

    def test_no_temporary_directory_is_refused(self):
        status, errors = self.job("case", tmpdir=False)
        self.assertEqual(
            (status, errors),
            (2, "TMPDIR is unset: the run's stores go in the job's temporary directory, fxjob's "
                "(run this under fxjob)\n"))
        self.assertEqual(self.left(), ["bin", "scripts", "tmp"])

    def test_it_takes_a_results_directory(self):
        done = subprocess.run(["/bin/bash", SCRIPTS / "test-e2e-job.sh"], capture_output=True,
                              text=True, env={"PATH": "/usr/bin:/bin"})
        self.assertEqual((done.returncode, done.stderr),
                         (2, "#   scripts/test-e2e-job.sh RESULTS [ARG...]\n"))


class Rounds(unittest.TestCase):
    """scripts/bench-rounds.sh's runs, by its text: it runs on a host with fxjob alone, as root."""

    def test_each_run_goes_through_the_job_with_no_bucket_or_store_of_the_results(self):
        text = (SCRIPTS / "bench-rounds.sh").read_text()
        self.assertIn('"CLADEFS_E2E_PREBUILT=$artifacts" "CLADEFS_BENCH_RESULTS=$out" \\\n'
                      '            "$repo/scripts/test-e2e-job.sh" "$out/e2e" "$case" '
                      '> "$out/run.log" 2>&1 || ran=$?', text)
        # Neither a bucket nor test-e2e.sh's results, the stores with them, in the results'
        # directory, on whatever filesystem that is (TASK-397).
        self.assertNotIn("CLADEFS_EXTENTD_REMOTE=", text)
        self.assertNotIn("CLADEFS_E2E_RESULTS=", text)
        self.assertNotIn("/scripts/test-e2e.sh\" \"$case\"", text)


if __name__ == "__main__":
    unittest.main()
