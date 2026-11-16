# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of the fs bench's A/B on a host (TASK-276): scripts/lib/bench-ab-linux-host.sh's order of
the sides' rounds, scripts/bench-ab-host.sh's sides, and scripts/test-e2e.sh's modes of artifacts
built once and runs of them, which refuse what they can't serve before anything is built."""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

from script import SCRIPTS


def bash(command, env=None):
    """Runs `command` in bash: its status, output and errors."""
    done = subprocess.run(["/bin/bash", "-c", command], capture_output=True, text=True,
                          env={"PATH": "/usr/bin:/bin", **(env or {})})
    return done.returncode, done.stdout, done.stderr


class Rounds(unittest.TestCase):
    def test_the_sides_take_turns_going_first(self):
        host = SCRIPTS / "lib" / "bench-ab-linux-host.sh"
        status, out, _ = bash(f"source {host}; ab_order 4")
        self.assertEqual((status, out), (0, "a b\nb a\na b\nb a\n"))

    def test_a_phase_is_the_build_or_the_rounds(self):
        """The driver builds the sides as work, then runs the rounds as a measurement, each a phase
        of the host script (CLADEFS_TEST_HOST_PHASE, scripts/lib/host.sh's host_job, TASK-368);
        another phase is refused before anything runs."""
        host = SCRIPTS / "lib" / "bench-ab-linux-host.sh"
        status, _, err = bash(f"{host} {SCRIPTS.parent} {tempfile.mkdtemp()} a b 2 a b",
                              {"CLADEFS_TEST_HOST_PHASE": "rounds"})
        self.assertEqual((status, err),
                         (2, "CLADEFS_TEST_HOST_PHASE is build or measure, or unset for both\n"))

    def test_sourced_it_does_nothing_but_define(self):
        host = SCRIPTS / "lib" / "bench-ab-linux-host.sh"
        status, out, err = bash(f"source {host}; echo sourced")
        self.assertEqual((status, out, err), (0, "sourced\n", ""))


# The host script's build, its own functions, with those of linux-host-setup.sh it calls in their
# place: each build and count says what it built in $CALLS; a tree's revision is its
# source-revision.txt's; FAIL names a revision that fails to build.
STUBS = """
host_tree() { :; }
revision() { sed -n 1p "$repo/source-revision.txt"; }
host_kernel() { echo "kernel $(revision | cut -c1-4)" >> "$CALLS"; host_kernel_image=$work/vmlinux; }
host_artifacts() {
    [[ $(revision) != "${FAIL:-}" ]] || return 1
    echo "artifacts $2 $(revision | cut -c1-4)" >> "$CALLS"
    mkdir -p "$1"
    echo "source $(revision)" > "$1/artifacts.txt"
    cp "$1/artifacts.txt" "$results/artifacts-$2.txt"
}
"""
# scripts/counts.sh, as the host's tools have it: says which tree it counted, and counts.
COUNTS = """#!/bin/bash
echo "counts $(sed -n 1p "$1/source-revision.txt" | cut -c1-4)" >> "$CALLS"
mkdir -p "$2"
echo "lifecycle 0" > "$2/status.txt"
"""


class Kept(unittest.TestCase):
    """The nightly's baseline is the commit the night before measured, built then: a night builds
    one tree, its baseline's artifacts kept (CLADEFS_AB_KEEP, TASK-441.1)."""

    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.dir = Path(scratch.name)
        self.keep = self.dir / "keep"
        self.calls = self.dir / "calls"
        tools = self.dir / "tools" / "scripts"
        tools.mkdir(parents=True)
        (tools / "counts.sh").write_text(COUNTS)
        (tools / "counts.sh").chmod(0o755)

    def tree(self, revision):
        tree = self.dir / "trees" / revision
        tree.mkdir(parents=True, exist_ok=True)
        (tree / "source-revision.txt").write_text(revision * 40 + "\n")
        return tree

    def night(self, base, head, **environment):
        """A night's build of side a, `base`, and side b, `head`: its status, the calls, each side's
        artifacts and its errors."""
        self.calls.write_text("")
        work = self.dir / f"work-{base}{head}"
        results = self.dir / f"results-{base}{head}"
        work.mkdir()
        results.mkdir()
        script = (f"source {SCRIPTS / 'lib' / 'bench-ab-linux-host.sh'}\n{STUBS}\n"
                  f"work={work} results={results} kernel_build=release "
                  f"host_tools={self.dir / 'tools'}\n"
                  f"build a {self.tree(base)} && build b {self.tree(head)}\nstatus=$?\n"
                  'echo "$artifacts_a"; echo "$artifacts_b"; exit $status')
        status, out, err = bash(script, {"CALLS": str(self.calls), "CLADEFS_AB_KEEP": str(self.keep),
                                         **environment})
        return status, self.calls.read_text().splitlines(), out.splitlines(), err, results

    def kept(self, revision):
        return str(self.keep / f"{revision * 40}-release")

    def test_a_night_builds_its_head_alone_the_base_kept_from_the_night_it_was_head(self):
        status, calls, artifacts, err, _ = self.night("1", "2")
        self.assertEqual((status, calls), (0, ["kernel 1111", "artifacts a 1111",
                                               "kernel 2222", "artifacts b 2222"]), err)
        self.assertEqual(artifacts, [self.kept("1"), self.kept("2")])
        status, calls, artifacts, err, results = self.night("2", "3")
        self.assertEqual((status, calls), (0, ["kernel 3333", "artifacts b 3333"]), err)
        self.assertEqual(artifacts, [self.kept("2"), self.kept("3")])
        self.assertIn(f"Side a's artifacts kept from a run before: {self.kept('2')}; building "
                      "nothing", err)
        self.assertEqual((results / "artifacts-a.txt").read_text(), f"source {'2' * 40}\n")

    def test_a_base_whose_artifacts_are_gone_builds_again(self):
        self.night("1", "2")
        for path in sorted(Path(self.kept("2")).rglob("*"), reverse=True):
            path.unlink() if path.is_file() else path.rmdir()
        Path(self.kept("2")).rmdir()
        status, calls, _, err, _ = self.night("2", "3")
        self.assertEqual((status, calls), (0, ["kernel 2222", "artifacts a 2222",
                                               "kernel 3333", "artifacts b 3333"]), err)

    def test_counts_are_counted_once_a_tree_and_kept_with_its_artifacts(self):
        status, calls, _, err, _ = self.night("1", "2", CLADEFS_AB_COUNTS="1")
        self.assertEqual((status, calls), (0, ["kernel 1111", "artifacts a 1111", "counts 1111",
                                               "kernel 2222", "artifacts b 2222", "counts 2222"]),
                         err)
        status, calls, _, err, _ = self.night("2", "3", CLADEFS_AB_COUNTS="1")
        self.assertEqual((status, calls), (0, ["kernel 3333", "artifacts b 3333", "counts 3333"]),
                         err)
        self.assertEqual((Path(self.kept("2")) / "counts" / "status.txt").read_text(),
                         "lifecycle 0\n")

    def test_only_the_artifacts_used_last_are_kept(self):
        self.night("1", "2", CLADEFS_AB_KEEP_COUNT="2")
        old = Path(self.kept("1"))
        os.utime(old, (old.stat().st_mtime - 3600, old.stat().st_mtime - 3600))
        status, _, _, err, _ = self.night("2", "3", CLADEFS_AB_KEEP_COUNT="2")
        self.assertEqual(status, 0, err)
        self.assertEqual(sorted(path.name for path in self.keep.iterdir()),
                         [f"{'2' * 40}-release", f"{'3' * 40}-release"])

    def test_a_side_that_fails_to_build_says_which(self):
        status, calls, _, err, results = self.night("1", "2", FAIL="2" * 40)
        self.assertEqual((status, calls), (1, ["kernel 1111", "artifacts a 1111", "kernel 2222"]))
        self.assertEqual((results / "build-failed").read_text(), "b\n")
        self.assertFalse(Path(self.kept("2")).exists())

    def test_a_tree_changed_from_its_commit_is_not_kept(self):
        tree = self.tree("4")
        (tree / "source-revision.txt").write_text("4" * 40 + "-dirty\n")
        status, out, _ = bash(f"source {SCRIPTS / 'lib' / 'bench-ab-linux-host.sh'}\n"
                              f"kernel_build=release; ab_key {tree}; echo end")
        self.assertEqual((status, out), (0, "end\n"))


class Sides(unittest.TestCase):
    def side(self, spec):
        # The driver's function alone, without its sourcing of host.sh, which names a run.
        driver = (SCRIPTS / "bench-ab-host.sh").read_text()
        start = driver.index("side() {")
        function = driver[start:driver.index("\n}\n", start) + 3]
        return bash(f"{function}\nside '{spec}'")

    def test_a_side_is_a_name_and_a_commit(self):
        self.assertEqual(self.side("base=origin/main~3"), (0, "base origin/main~3\n", ""))

    def test_a_side_without_a_name_is_refused(self):
        self.assertEqual(self.side("origin/main"), (2, "", "a side is NAME=COMMIT, NAME in "
                                                         "lower-case letters, digits and -: "
                                                         "origin/main\n"))


class Artifacts(unittest.TestCase):
    """test-e2e.sh's modes, refused before any build, on any host."""

    def e2e(self, **env):
        results = tempfile.mkdtemp()
        return bash(f"{SCRIPTS / 'test-e2e.sh'} fxfs-guest",
                    {"CLADEFS_E2E_RESULTS": results, **env})

    def test_building_artifacts_and_running_prebuilt_ones_are_one_or_the_other(self):
        status, _, err = self.e2e(CLADEFS_E2E_BUILD_ONLY="/a", CLADEFS_E2E_PREBUILT="/b")
        self.assertEqual(
            (status, err), (2, "CLADEFS_E2E_BUILD_ONLY and CLADEFS_E2E_PREBUILT: one or the other\n"))

    def test_prebuilt_artifacts_take_no_build_per_run(self):
        for mode in ["CLADEFS_E2E_PREBUILT", "CLADEFS_E2E_BUILD_ONLY"]:
            status, _, err = self.e2e(**{mode: "/b"}, CLADEFS_FIRECRACKER_SECCOMP="log")
            self.assertEqual(
                (status, err), (2, "prebuilt artifacts take no CLADEFS_FIRECRACKER_SECCOMP=log\n"),
                mode)

    def test_prebuilt_artifacts_are_jailed_by_their_own_jailer(self):
        """TASK-284: artifacts built under a seccomp filter hold the jailer, which a jailed run of
        them takes; those built with none hold none, and a jailed run of them is refused."""
        unjailed = tempfile.mkdtemp()
        (Path(unjailed) / "artifacts.txt").write_text("seccomp none\n")
        status, _, err = self.e2e(CLADEFS_E2E_PREBUILT=unjailed, CLADEFS_FIRECRACKER_JAIL="1")
        self.assertEqual((status, err), (2, f"{unjailed} holds no jailer, built with no seccomp "
                                            "filter: CLADEFS_FIRECRACKER_JAIL needs one\n"))
        text = (SCRIPTS / "test-e2e.sh").read_text()
        self.assertIn('export CLADEFS_FIRECRACKER_JAILER="$prebuilt/jailer"', text)
        self.assertIn('if [[ -n $build_only || -n ${CLADEFS_FIRECRACKER_JAIL:-} ]]; then\n'
                      '            env "$musl_cflags" "$firecracker_cargo" build --release -p jailer',
                      text)
        self.assertIn("artifacts+=(jailer)", text)

    def test_a_directory_of_no_artifacts_is_refused(self):
        empty = tempfile.mkdtemp()
        status, _, err = self.e2e(CLADEFS_E2E_PREBUILT=empty)
        self.assertEqual(
            (status, err), (2, f"{empty} holds no artifacts (CLADEFS_E2E_BUILD_ONLY makes them)\n"))


if __name__ == "__main__":
    unittest.main()
