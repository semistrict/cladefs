# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/test-e2e-job.sh, which runs scripts/test-e2e.sh with what it writes, the
cases' cells among it, in the command's temporary directory, and copies the results back
(TASK-397): here in a tree of its own, test-e2e-job.sh as it is, and in place of test-e2e.sh one
that says where it ran and leaves what a case leaves, and of scripts/contamination.py, under which the cases run (TASK-375), one that says how it
was run; and of scripts/bench-rounds.sh's runs, which go through it, and its build, which they
boot."""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

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
def files(directory):
    """The files under `directory`, by their paths in it."""
    return sorted(str(path.relative_to(directory)) for path in directory.rglob("*")
                  if path.is_file())


class Job(unittest.TestCase):
    def job(self, *arguments, status=0, env=None):
        """Runs test-e2e-job.sh with `arguments` after its results directory, in a command with a
        temporary directory of its own, its cases ending with `status`: its status and errors."""
        self.tree = tree = Path(tempfile.mkdtemp()).resolve()
        (tree / "scripts").mkdir()
        (tree / "scripts" / "test-e2e-job.sh").symlink_to(SCRIPTS / "test-e2e-job.sh")
        (tree / "bin").mkdir()
        for path, text in [(tree / "scripts" / "test-e2e.sh", CASES),
                           (tree / "scripts" / "contamination.py", CONTAMINATION)]:
            path.write_text(text)
            path.chmod(0o755)
        (tree / "tmp").mkdir()
        done = subprocess.run(
            ["/bin/bash", tree / "scripts" / "test-e2e-job.sh", tree / "results", *arguments],
            env={
                "PATH": f"{tree / 'bin'}:/usr/bin:/bin",
                "FAKE_STATUS": str(status),
                "FAKE_RECORDED": str(tree / "recorded"),
                "TMPDIR": str(tree / "tmp"),
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

    def test_the_run_is_a_round_recorded_of_the_disk_its_cells_are_on(self):
        # The directory the run wrote in, in the command's temporary one, is the round's disk, and
        # no other: not the results'.
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

    def test_it_takes_a_results_directory(self):
        done = subprocess.run(["/bin/bash", SCRIPTS / "test-e2e-job.sh"], capture_output=True,
                              text=True, env={"PATH": "/usr/bin:/bin"})
        self.assertEqual((done.returncode, done.stderr),
                         (2, "#   scripts/test-e2e-job.sh RESULTS [ARG...]\n"))


# test-e2e.sh for bench-rounds.sh: says, a line of $CLADEFS_FAKE_SAID, how it was run, building or
# booting what was built, as work or a measurement (scripts/lib/measurement.sh), and with what
# bounds on cargo; builds, as the real one does, refusing to inside a measurement, artifacts.txt
# its build.
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
WORK = "0::/user.slice/user-1000.slice/user-1000-hotbed.slice/hotbed-build.service"
BENCH = "0::/user.slice/user-1000.slice/user-1000-hotbed_bench.slice/hotbed-rounds.service"


class Rounds(unittest.TestCase):
    """scripts/bench-rounds.sh's runs: in a tree of its own, with test-e2e.sh and test-e2e-job.sh
    that say how they ran, the host's /proc and /sys a tree of their own (CLADEFS_TEST_HOST_PROC,
    CLADEFS_TEST_HOST_SYS), and its phases each a hotbed command's, a measurement's in hotbed's
    measurement slice (CLADEFS_SELF_CGROUP); and by its text."""

    def rounds(self, phases, env=None):
        """Runs bench-rounds.sh for one round of the case fxfs-guest, once for each phase, as
        hotbed runs it: its status, errors and the tree it ran in."""
        tree = Path(tempfile.mkdtemp()).resolve()
        scripts = tree / "scripts"
        (scripts / "lib").mkdir(parents=True)
        (scripts / "bench-rounds.sh").symlink_to(SCRIPTS / "bench-rounds.sh")
        (scripts / "lib" / "measurement.sh").symlink_to(SCRIPTS / "lib" / "measurement.sh")
        for path, text in [(scripts / "test-e2e.sh", ROUNDS_CASES),
                           (scripts / "test-e2e-job.sh", ROUNDS_JOB)]:
            path.write_text(text)
            path.chmod(0o755)
        (tree / "proc" / "self").mkdir(parents=True)
        (tree / "proc" / "loadavg").write_text("1.50 1.20 1.00 2/300 4000\n")
        for cgroup in (WORK, BENCH):
            unit = tree / "sys" / "fs" / "cgroup" / cgroup.removeprefix("0::/")
            unit.mkdir(parents=True)
            for resource in ["cpu", "memory", "io"]:
                (unit / f"{resource}.pressure").write_text(
                    "some avg10=0.00 avg60=0.00 avg300=0.00 total=5000\n")
        for phase in phases:
            # A phase and the cgroup it runs in, or a phase alone, a measurement's measure.
            phase, cgroup = phase if isinstance(phase, tuple) else (
                phase, BENCH if phase == "measure" else WORK)
            (tree / "proc" / "self" / "cgroup").write_text(cgroup + "\n")
            (tree / "cgroup").write_text(cgroup + "\n")
            done = subprocess.run(
                ["/bin/bash", scripts / "bench-rounds.sh", tree / "results", "1", "fxfs-guest"],
                env={"PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "CLADEFS_FAKE_SAID": str(tree / "said"),
                     "CLADEFS_TEST_HOST_PROC": str(tree / "proc"),
                     "CLADEFS_TEST_HOST_SYS": str(tree / "sys"),
                     "CLADEFS_SELF_CGROUP": str(tree / "cgroup"),
                     **({"CLADEFS_TEST_HOST_PHASE": phase} if phase else {}), **(env or {})},
                capture_output=True, text=True)
            if done.returncode:
                return done.returncode, done.stderr, tree
        return 0, "", tree

    def test_the_cases_build_as_work_and_each_run_boots_them_alone(self):
        """TASK-368: the build a command of work, the runs a measurement that builds nothing."""
        status, errors, tree = self.rounds(["build", "measure"])
        self.assertEqual(status, 0, errors)
        self.assertEqual((tree / "said").read_text().splitlines(),
                         ["build work fxfs-guest jobs=4 target=",
                          "prebuilt bench fxfs-guest jobs= target="])
        self.assertEqual((tree / "results" / "runs.tsv").read_text().splitlines()[1].split("\t")[:4],
                         ["1", "fxfs-guest", "ok", "1.50"])

    def test_a_measurement_that_would_build_is_refused(self):
        """Both phases at once inside a measurement, as before TASK-368: refused before it builds."""
        status, errors, tree = self.rounds([("", BENCH)])
        self.assertEqual(status, 2)
        self.assertTrue(errors.startswith("bench-rounds.sh's build would build inside a measurement "
                                          f"(hotbed run --bench, {BENCH})"), errors)
        self.assertFalse((tree / "said").exists())

    def test_the_build_takes_the_callers_bounds_on_cargo(self):
        target = tempfile.mkdtemp()
        status, errors, tree = self.rounds(["build"], {"CARGO_BUILD_JOBS": "2",
                                                       "CARGO_TARGET_DIR": target})
        self.assertEqual(status, 0, errors)
        self.assertEqual((tree / "said").read_text().splitlines()[0],
                         f"build work fxfs-guest jobs=2 target={target}")

    def test_each_run_goes_through_the_job_with_no_bucket_or_store_of_the_results(self):
        text = (SCRIPTS / "bench-rounds.sh").read_text()
        self.assertIn('CLADEFS_E2E_PREBUILT=$artifacts CLADEFS_BENCH_RESULTS=$out \\\n'
                      '            "$repo/scripts/test-e2e-job.sh" "$out/e2e" "$case" '
                      '> "$out/run.log" 2>&1 || ran=$?', text)
        # Neither a bucket nor test-e2e.sh's results, the cells with them, in the results'
        # directory, on whatever filesystem that is (TASK-397).
        self.assertNotIn("CLADEFSD_BUCKET=", text)
        self.assertNotIn("CLADEFS_E2E_RESULTS=", text)
        self.assertNotIn("/scripts/test-e2e.sh\" \"$case\"", text)
        # Nor the round's record here, of the results' disk: test-e2e-job.sh's, of its cells'
        # (TASK-375).
        self.assertNotIn("contamination.py", text.split("set -euo pipefail")[1])


if __name__ == "__main__":
    unittest.main()
