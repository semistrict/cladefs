# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of the guard that nothing builds inside a measurement (scripts/lib/measurement.sh,
TASK-368): a process in fxjob --bench's slice, as its cgroup says (CLADEFS_SELF_CGROUP standing for
/proc/self/cgroup), is refused each way a build would start there, before it starts, saying why;
one in work's slice, or on a host of no such cgroups, goes on."""

import subprocess
import tempfile
import unittest
from pathlib import Path

from script import SCRIPTS

SLICES = "0::/user.slice/user-1000.slice/user@1000.service/fxdax.slice"
BENCH = f"{SLICES}/fxdax-bench.slice/fxjob-rounds-1-42.scope"
WORK = f"{SLICES}/fxdax-work.slice/fxjob-rounds-build-41.scope"


def refusal(what):
    """What the guard says, refusing `what` in the bench's cgroup."""
    return (f"{what} would build inside a measurement (fxjob --bench, {BENCH}), holding its cores "
            "and memory from every other measurement for as long as it builds: build first as "
            "work (fxjob without --bench; scripts/lib/host.sh's host_job, test-e2e.sh's "
            "CLADEFS_E2E_BUILD_ONLY), then measure what it built (CLADEFS_E2E_PREBUILT) "
            "(TASK-368)\n")


class Guard(unittest.TestCase):
    def setUp(self):
        self.directory = Path(tempfile.mkdtemp()).resolve()
        # cargo, which says it ran, as a build that was let start would.
        bin = self.directory / "bin"
        bin.mkdir()
        (bin / "cargo").write_text('#!/bin/sh\necho "cargo $*"\n')
        (bin / "cargo").chmod(0o755)

    def cgroup(self, text):
        path = self.directory / "cgroup"
        path.write_text(text + "\n")
        return str(path)

    def bash(self, command, cgroup=BENCH, **env):
        """Runs `command` in bash as in the cgroup `cgroup` (None: no such file, as on macOS): its
        status, output and errors."""
        place = self.cgroup(cgroup) if cgroup is not None else str(self.directory / "none")
        done = subprocess.run(["/bin/bash", "-c", command], capture_output=True, text=True,
                              env={"PATH": f"{self.directory / 'bin'}:/usr/bin:/bin",
                                   "HOME": str(self.directory), "CLADEFS_SELF_CGROUP": place,
                                   **env})
        return done.returncode, done.stdout, done.stderr

    def test_a_measurement_is_its_cgroups_and_no_other(self):
        measuring = f"source {SCRIPTS / 'lib' / 'measurement.sh'}; measuring && echo yes || echo no"
        for cgroup, said in [(BENCH, "yes\n"), (WORK, "no\n"), (None, "no\n"),
                             ("0::/user.slice/user-1000.slice/session-c7267.scope", "no\n")]:
            with self.subTest(cgroup=cgroup):
                self.assertEqual(self.bash(measuring, cgroup), (0, said, ""))

    def test_test_e2e_builds_nothing_inside_a_measurement(self):
        results = tempfile.mkdtemp()
        for mode, what in [({}, "test-e2e.sh, CLADEFS_E2E_PREBUILT unset,"),
                           ({"CLADEFS_E2E_BUILD_ONLY": str(self.directory / "artifacts")},
                            "test-e2e.sh (CLADEFS_E2E_BUILD_ONLY), CLADEFS_E2E_PREBUILT unset,")]:
            with self.subTest(mode=mode):
                self.assertEqual(
                    self.bash(f"{SCRIPTS / 'test-e2e.sh'} fxfs-guest",
                              CLADEFS_E2E_RESULTS=results, **mode),
                    (2, "", refusal(what)))

    def test_prebuilt_artifacts_are_every_one_there(self):
        """CLADEFS_E2E_PREBUILT complete, in a measurement or not: an artifact missing is refused
        before anything runs, not met as a case fails."""
        artifacts = self.directory / "artifacts"
        artifacts.mkdir()
        (artifacts / "artifacts.txt").write_text("source x\nseccomp real\n")
        for name in ["runner", "guest-agent", "cladefs-extentd", "cladefsctl", "firecracker"]:
            (artifacts / name).write_text("")
            (artifacts / name).chmod(0o755)
        for cgroup in [BENCH, WORK]:
            with self.subTest(cgroup=cgroup):
                self.assertEqual(
                    self.bash(f"{SCRIPTS / 'test-e2e.sh'} fxfs-guest", cgroup,
                              CLADEFS_E2E_RESULTS=tempfile.mkdtemp(),
                              CLADEFS_E2E_PREBUILT=str(artifacts)),
                    (2, "", f"{artifacts}/cladefs-gc: no such artifact (CLADEFS_E2E_BUILD_ONLY "
                            "makes them)\n"))

    def test_the_suite_builds_nothing_inside_a_measurement(self):
        # In a tree of its own, whose Unicode data no download fills: let start, it fails there,
        # running no part.
        tree = self.directory / "tree"
        (tree / "scripts" / "lib").mkdir(parents=True)
        (tree / "target").mkdir()
        (tree / "target" / "UCD-12.1.0.zip").write_text("")
        (tree / "scripts" / "test.sh").symlink_to(SCRIPTS / "test.sh")
        (tree / "scripts" / "lib" / "measurement.sh").symlink_to(SCRIPTS / "lib" / "measurement.sh")
        self.assertEqual(
            self.bash(f"{tree / 'scripts' / 'test.sh'}", CLADEFS_TEST_PARTS="workspace scripts"),
            (2, "", refusal("scripts/test.sh's parts workspace scripts")))

    def host_setup(self, command, cgroup=BENCH, **env):
        """Runs `command` after sourcing linux-host-setup.sh, as a host script does."""
        return self.bash(
            f"set -euo pipefail; results={self.directory}/results; work={self.directory}/work; "
            f"source {SCRIPTS / 'lib' / 'linux-host-setup.sh'}; {command}", cgroup, **env)

    def test_host_scripts_build_nothing_inside_a_measurement(self):
        self.assertEqual(self.host_setup("host_cargo build --release; echo built"),
                         (2, "", refusal("cargo build")))
        self.assertEqual(self.host_setup("kind=release; host_kernel_build /k; echo built"),
                         (2, "", refusal("The release guest kernel's build")))

    def test_host_scripts_build_as_work(self):
        self.assertEqual(self.host_setup("host_cargo build --release", WORK),
                         (0, "cargo build --release\n", ""))

    def test_a_phase_is_the_build_or_the_measurement(self):
        self.assertEqual(self.host_setup("host_setup", WORK, CLADEFS_TEST_HOST_PHASE="rounds"),
                         (2, "", "CLADEFS_TEST_HOST_PHASE is build or measure, or unset for both\n"))

    def test_a_measurement_runs_what_was_built(self):
        artifacts = self.directory / "artifacts"
        self.assertEqual(self.host_setup(f"host_prebuilt {artifacts}", WORK),
                         (2, "", f"{artifacts} holds no artifacts: CLADEFS_TEST_HOST_PHASE=build makes "
                                 "them\n"))
        artifacts.mkdir()
        (artifacts / "artifacts.txt").write_text("source x\n")
        (artifacts / "vmlinux").write_text("")
        self.assertEqual(
            self.host_setup(f"host_prebuilt {artifacts}; "
                            'echo "$CLADEFS_E2E_PREBUILT $CLADEFS_GUEST_KERNEL"', WORK),
            (0, f"{artifacts} {artifacts}/vmlinux\n", ""))

    def test_gc_sweeps_build_nothing_inside_a_measurement(self):
        sweep = SCRIPTS / "gc-sweep.sh"
        work, results = self.directory / "w", self.directory / "r"
        self.assertEqual(self.bash(f"{sweep} {work} {results}"),
                         (2, "", refusal("Building cladefs-gc")))
        # What --prepare made is swept; with nothing made, refused before a sweep.
        self.assertEqual(self.bash(f"{sweep} --prepared {work} {results} a={SCRIPTS.parent}"),
                         (2, "", f"{work}/target-a/release holds no cladefs-gc and "
                                 "cladefs-gc-fill: --prepare builds them\n"))


if __name__ == "__main__":
    unittest.main()
