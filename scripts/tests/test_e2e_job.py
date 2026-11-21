# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/test-e2e-job.sh, which runs scripts/test-e2e.sh with what it writes, the
cases' cells among it, in the job's temporary directory, on ext4, and copies the results back
(TASK-397): here in a tree of its own, test-e2e-job.sh as it is, and in place of test-e2e.sh one
that says where it ran and leaves what a case leaves, of findmnt one that says the filesystem
given, and of scripts/contamination.py, under which the cases run (TASK-375), one that says how it
was run; and of scripts/bench-rounds.sh's runs, which go through it, and its build, which they
boot."""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

from fake_ssh import FXJOB
from script import SCRIPTS

# test-e2e.sh, which says in its results what it was given, leaves a case's log, its cell, its
# guest's root filesystem and image and the daemon's part cache, and ends with $FAKE_STATUS.
CASES = """#!/bin/bash
cd "$CLADEFS_E2E_RESULTS"
printf '%s\\n' "$CLADEFS_E2E_RESULTS" "${CLADEFSD_BUCKET-no bucket named}" "$@" > given.txt
mkdir -p case/extents case/rootfs/bin .cladefsd-cache.x1 case/store-1 case/vm.bucket
echo log > case/log.txt
for left in case/extents/segment case/rootfs/bin/sh case/rootfs.ext4 case/disk.img \\
    .cladefsd-cache.x1/part case/store-1/head case/vm.bucket/object; do
    echo left > "$left"
done
exit "$FAKE_STATUS"
"""
# contamination.py, which says how it was run and runs the round it was given.
CONTAMINATION = """#!/bin/bash
printf '%s\\n' "$@" > "$FAKE_RECORDED"
[[ $1 == run && $4 == -- ]] || exit 64
shift 4
exec "$@"
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
                           (tree / "scripts" / "contamination.py", CONTAMINATION),
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
                "FAKE_RECORDED": str(tree / "recorded"),
                **({"TMPDIR": str(tree / "tmp")} if tmpdir else {}),
                **(env or {}),
            },
            capture_output=True,
            text=True,
        )
        return done.returncode, done.stderr

    def moved(self):
        """What a run says as it ends: where its results are, and the directory it ran in."""
        where = self.given()[0]
        return f"Results: {self.tree / 'results'} (the run's {where})\n"

    def given(self):
        """What test-e2e.sh was given: its results directory, its bucket and its arguments."""
        return (self.tree / "results" / "given.txt").read_text().splitlines()

    def recorded(self):
        """How contamination.py was run: its arguments."""
        return (self.tree / "recorded").read_text().splitlines()

    def left(self):
        """What the tree holds once a job was refused: no results, no round run or recorded,
        nothing in the temporary directory, and `asked` if findmnt was."""
        self.assertEqual(list((self.tree / "tmp").iterdir()), [])
        return sorted(path.name for path in self.tree.iterdir())

    def test_the_run_writes_in_the_jobs_temporary_directory_and_its_results_come_back(self):
        status, errors = self.job("bench-fs-fxfs-guest-dax")
        self.assertEqual((status, errors), (0, self.moved()))
        where, bucket, *arguments = self.given()
        # In a directory of its own there, which is gone, as nothing else of the run's is left.
        self.assertEqual(Path(where).parent, self.tree / "tmp")
        self.assertEqual(files(self.tree / "tmp"), [])
        self.assertEqual(list((self.tree / "tmp").iterdir()), [])
        # The bucket is test-e2e.sh's own, on tmpfs: none is named.
        self.assertEqual((bucket, arguments), ("no bucket named", ["bench-fs-fxfs-guest-dax"]))
        self.assertEqual((self.tree / "asked").read_text(), f"-no FSTYPE -T {self.tree / 'tmp'}\n")

    def test_the_run_is_a_round_recorded_of_the_disk_its_cells_are_on(self):
        # The directory the run wrote in, in the job's temporary one, is the round's disk, and no
        # other: not the results', which is the test host's ZFS home under bench-rounds.sh, unchecked.
        status, errors = self.job("bench-fs-fxfs-guest-dax")
        self.assertEqual((status, errors), (0, self.moved()))
        self.assertEqual(self.recorded(), [
            "run", str(self.tree / "results"), self.given()[0], "--",
            str(self.tree / "scripts" / "test-e2e.sh"), "bench-fs-fxfs-guest-dax"])

    def test_the_rounds_record_goes_beside_the_benchs_report(self):
        # Where bench-ab.py reads both: bench-rounds.sh's round directory.
        status, errors = self.job("case", env={"CLADEFS_BENCH_RESULTS": "/round/1"})
        self.assertEqual((status, errors), (0, self.moved()))
        self.assertEqual(self.recorded(),
                         ["run", "/round/1", self.given()[0], "--",
                          str(self.tree / "scripts" / "test-e2e.sh"), "case"])

    def test_the_cells_caches_and_root_filesystems_are_no_results(self):
        status, errors = self.job("--tag", "bench")
        self.assertEqual((status, errors), (0, self.moved()))
        self.assertEqual(files(self.tree / "results"), ["case/log.txt", "given.txt"])
        self.assertEqual(self.given()[2:], ["--tag", "bench"])

    def test_a_bucket_named_is_the_runs(self):
        status, errors = self.job("case", env={"CLADEFSD_BUCKET": "s3://bucket/p"})
        self.assertEqual((status, errors), (0, self.moved()))
        self.assertEqual(self.given()[1], "s3://bucket/p")

    def test_a_failed_runs_results_come_back_with_its_status(self):
        status, errors = self.job("case", status=1)
        self.assertEqual((status, errors), (1, self.moved()))
        self.assertEqual(files(self.tree / "results"), ["case/log.txt", "given.txt"])
        self.assertEqual(list((self.tree / "tmp").iterdir()), [])

    def test_a_temporary_directory_on_another_filesystem_is_refused(self):
        # the test host's home, where a durable sync measured ZFS's syncs, and /tmp, memory.
        for fstype in ["zfs", "tmpfs"]:
            with self.subTest(fstype):
                status, errors = self.job("case", fstype=fstype)
                self.assertEqual(
                    (status, errors),
                    (2, f"{self.tree / 'tmp'} is on {fstype}: the run's cells go on ext4, as a "
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
            (2, "TMPDIR is unset: the run's cells go in the job's temporary directory, fxjob's "
                "(run this under fxjob)\n"))
        self.assertEqual(self.left(), ["bin", "scripts", "tmp"])

    def test_it_takes_a_results_directory(self):
        done = subprocess.run(["/bin/bash", SCRIPTS / "test-e2e-job.sh"], capture_output=True,
                              text=True, env={"PATH": "/usr/bin:/bin"})
        self.assertEqual((done.returncode, done.stderr),
                         (2, "#   scripts/test-e2e-job.sh RESULTS [ARG...]\n"))


# test-e2e.sh for bench-rounds.sh: says, a line of $CLADEFS_FAKE_SAID (a CLADEFS_* variable, which
# bench-rounds.sh passes through sudo), how it was run, building or booting what was built, as work
# or a measurement (scripts/lib/measurement.sh), and with what bounds on cargo; builds, as the real
# one does, refusing to inside a measurement, artifacts.txt its build.
ROUNDS_CASES = """#!/bin/bash
set -euo pipefail
source "$(dirname "$0")/lib/measurement.sh"
where=work
if measuring; then where=bench; fi
if [[ -n ${CLADEFS_E2E_BUILD_ONLY:-} ]]; then
    measurement_refuses test-e2e.sh
    mkdir -p "$CLADEFS_E2E_BUILD_ONLY"
    echo "source x" > "$CLADEFS_E2E_BUILD_ONLY/artifacts.txt"
    mode=build
else
    [[ -f $CLADEFS_E2E_PREBUILT/artifacts.txt ]]
    mode=prebuilt
fi
echo "$mode $where $* jobs=${CARGO_BUILD_JOBS:-} target=${CARGO_TARGET_DIR:-}" >> "$CLADEFS_FAKE_SAID"
"""
# test-e2e-job.sh for bench-rounds.sh: test-e2e.sh's run of its arguments.
ROUNDS_JOB = """#!/bin/bash
shift
exec "$(dirname "$0")/test-e2e.sh" "$@"
"""
# sudo, which runs its command in an environment of its own, as sudo does, but for the cgroup the
# fake fxjob says the caller is in, which a process keeps under sudo.
ROUNDS_SUDO = """#!/bin/bash
exec env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin \\
    ${CLADEFS_SELF_CGROUP:+CLADEFS_SELF_CGROUP=$CLADEFS_SELF_CGROUP} "$@"
"""


class Rounds(unittest.TestCase):
    """scripts/bench-rounds.sh's runs: in a tree of its own, with fxjob, sudo, test-e2e.sh and
    test-e2e-job.sh that say how they ran, and the host's /proc and /sys a tree of their own
    (CLADEFS_TEST_HOST_PROC, CLADEFS_TEST_HOST_SYS); and by its text."""

    def rounds(self, env=None):
        """Runs bench-rounds.sh for one round of the case fxfs-guest: its status, errors and the
        tree it ran in."""
        tree = Path(tempfile.mkdtemp()).resolve()
        scripts, bin = tree / "scripts", tree / "bin"
        (scripts / "lib").mkdir(parents=True)
        bin.mkdir()
        (scripts / "bench-rounds.sh").symlink_to(SCRIPTS / "bench-rounds.sh")
        (scripts / "lib" / "measurement.sh").symlink_to(SCRIPTS / "lib" / "measurement.sh")
        for path, text in [(scripts / "test-e2e.sh", ROUNDS_CASES),
                           (scripts / "test-e2e-job.sh", ROUNDS_JOB),
                           (bin / "fxjob", FXJOB), (bin / "sudo", ROUNDS_SUDO)]:
            path.write_text(text)
            path.chmod(0o755)
        home = tree / "home"
        for directory in [home / ".cargo", home / ".rustup", tree / "proc" / "sys" / "vm"]:
            directory.mkdir(parents=True)
        (tree / "proc" / "loadavg").write_text("1.50 1.20 1.00 2/300 4000\n")
        (tree / "proc" / "sys" / "vm" / "drop_caches").write_text("")
        uid = os.getuid()
        slice = (tree / "sys" / "fs" / "cgroup" / "user.slice" / f"user-{uid}.slice" /
                 f"user@{uid}.service" / "fxdax.slice" / "fxdax-bench.slice")
        slice.mkdir(parents=True)
        for resource in ["cpu", "memory", "io"]:
            (slice / f"{resource}.pressure").write_text(
                "some avg10=0.00 avg60=0.00 avg300=0.00 total=5000\n")
        done = subprocess.run(
            ["/bin/bash", scripts / "bench-rounds.sh", tree / "results", "1", "fxfs-guest"],
            env={"PATH": f"{bin}:/usr/bin:/bin:/usr/sbin:/sbin", "HOME": str(home),
                 "CLADEFS_FAKE_SAID": str(tree / "said"), "CLADEFS_TEST_HOST_PROC": str(tree / "proc"),
                 "CLADEFS_TEST_HOST_SYS": str(tree / "sys"), **(env or {})},
            capture_output=True, text=True)
        return done.returncode, done.stderr, tree

    def test_the_cases_build_as_work_and_each_run_boots_them_alone(self):
        """TASK-368: the build an fxjob of work, the run a measurement that builds nothing."""
        status, errors, tree = self.rounds()
        self.assertEqual(status, 0, errors)
        calls = (tree / "home" / "fxjob-calls").read_text().splitlines()
        self.assertEqual([call.split(" -- ")[0] for call in calls],
                         ["--name rounds-build --mem 8G", "--bench --mem 16G --name rounds-fxfs-guest-1"])
        self.assertEqual((tree / "said").read_text().splitlines(),
                         ["build work fxfs-guest jobs=4 target=",
                          "prebuilt bench fxfs-guest jobs= target="])
        self.assertEqual((tree / "proc" / "sys" / "vm" / "drop_caches").read_text(), "3\n")
        self.assertEqual((tree / "results" / "runs.tsv").read_text().splitlines()[1].split("\t")[:4],
                         ["1", "fxfs-guest", "ok", "1.50"])

    def test_the_build_takes_the_callers_bounds_on_cargo(self):
        """TASK-446: sudo dropped CARGO_BUILD_JOBS and CARGO_TARGET_DIR, and the build ran a job a
        CPU, in the worktree's target."""
        target = tempfile.mkdtemp()
        status, errors, tree = self.rounds({"CARGO_BUILD_JOBS": "2", "CARGO_TARGET_DIR": target})
        self.assertEqual(status, 0, errors)
        self.assertEqual((tree / "said").read_text().splitlines()[0],
                         f"build work fxfs-guest jobs=2 target={target}")

    def test_each_run_goes_through_the_job_with_no_bucket_or_store_of_the_results(self):
        text = (SCRIPTS / "bench-rounds.sh").read_text()
        self.assertIn('"CLADEFS_E2E_PREBUILT=$artifacts" "CLADEFS_BENCH_RESULTS=$out" \\\n'
                      '            "$repo/scripts/test-e2e-job.sh" "$out/e2e" "$case" '
                      '> "$out/run.log" 2>&1 || ran=$?', text)
        # Neither a bucket nor test-e2e.sh's results, the cells with them, in the results'
        # directory, on whatever filesystem that is (TASK-397).
        self.assertNotIn("CLADEFSD_BUCKET=", text)
        self.assertNotIn("CLADEFS_E2E_RESULTS=", text)
        self.assertNotIn("/scripts/test-e2e.sh\" \"$case\"", text)
        # Nor the round's record here, of the results' disk: the job's, of its cells' (TASK-375).
        self.assertNotIn("contamination.py", text.split("set -euo pipefail")[1])


if __name__ == "__main__":
    unittest.main()
