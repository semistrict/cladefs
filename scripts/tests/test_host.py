# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/lib/host.sh, which the drivers that run cladefs in a hotbed workspace on a test
host source, of its workspace's half, scripts/lib/host-workspace.sh, and of the drivers (board
6058).

host_command builds the environment and the command a run's job runs its host script with. The
runs, their worktrees and their jobs run in the workspace of fake_hotbed.py's one node: a directory
here, the checkout a clone of a fleet's origin, whose host scripts are FAKE_HOST_SCRIPT."""

import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
import weakref
from pathlib import Path

from script import SCRIPTS

HOST = SCRIPTS / "lib" / "host.sh"
FAKE_HOTBED = Path(__file__).resolve().with_name("fake_hotbed.py")
# The variables that stay here (host_env_local), in its order.
KEPT = [
    "CLADEFS_DOCKER_STATIC",
    "CLADEFS_BUSYBOX",
    "CLADEFS_FIRECRACKER_SECCOMP_FILTER",
    "CLADEFS_FIRECRACKER_JAILER",
    "CLADEFSD_GRPC",
    "CLADEFSD_STATS",
    "CLADEFSD_METRICS",
    "CLADEFSD_ROOTS",
    "CLADEFSD_TENANTS",
    "CLADEFS_BENCH_RESULTS",
    "CLADEFS_TEST_HOST",
    "CLADEFS_HOTBED_WORK",
    "CLADEFS_HOTBED_REPO",
]
# The run host_command's tests name, and its directory in the workspace.
RUN = "run-x"
CHECKOUT = "/work/cladefs"
DIR = f"{CHECKOUT}/target/host-runs/{RUN}"
# What every job's environment holds: the builds' jobs, and the run's own.
BASE = [
    "CARGO_BUILD_JOBS=4",
    "NEXTEST_TEST_THREADS=4",
    "CLADEFS_GUEST_KERNEL_JOBS=4",
    "CLADEFS_TEST_HOST_IN_PLACE=1",
    f"CLADEFS_TEST_HOST_KERNELS={CHECKOUT}/target/guest-kernels",
    "CLADEFS_TEST_HOST_PROVISIONED=1",
    f"CLADEFS_TEST_HOST_WORK={DIR}/work",
]
# Its command: the host script of the run's tree, the checkout's, and its arguments.
COMMAND = ["bash", f"{CHECKOUT}/scripts/lib/test-linux-host.sh", CHECKOUT, f"{DIR}/results"]


class HostCommand(unittest.TestCase):
    def host_command(self, environment, *args):
        """host_command ARGS... of the run run-x, run in `environment`: its environment, sorted,
        and its command."""
        built = subprocess.run(
            [
                # macOS's 3.2 there, as a driver runs in.
                "/bin/bash",
                "-c",
                f'set -euo pipefail; limit=6h; source "$0"; host_name {RUN}; host_tree={CHECKOUT}; '
                'host_command "$@"; printf "%s\\0" "${host_environment[@]}"; printf "\\1\\0"; '
                'printf "%s\\0" "${host_argv[@]}"',
                HOST,
                *args,
            ],
            env={"PATH": "/usr/bin:/bin", "HOME": "/laptop/home", **environment},
            capture_output=True,
            text=True,
        )
        self.assertEqual((built.returncode, built.stderr), (0, ""))
        words = built.stdout.split("\0")[:-1]
        split = words.index("\1")
        return sorted(words[:split]), words[split + 1:]

    def test_every_variable_reaches_the_workspace(self):
        self.assertEqual(
            self.host_command(
                {
                    "CLADEFS_PJDFSTEST_TESTS": "chmod chown",
                    "CLADEFS_ZSTD_LEVEL": "3",
                    "PJDFSTEST_COMMIT": "85a8aea9",
                    # A driver's own, which does nothing there.
                    "CLADEFS_TEST_HOST_RUN": "r1040",
                    "OTHER": "stays",
                },
                "test-linux-host.sh",
                "fxfs-guest fxfs-guest-dax",
                "a",
            ),
            (
                sorted(BASE + [
                    "CLADEFS_TEST_HOST_RUN=r1040",
                    "CLADEFS_PJDFSTEST_TESTS=chmod chown",
                    "CLADEFS_ZSTD_LEVEL=3",
                    "PJDFSTEST_COMMIT=85a8aea9",
                ]),
                COMMAND + ["fxfs-guest fxfs-guest-dax", "a"],
            ),
        )

    def test_this_machines_own_setup_stays_here(self):
        here = {name: "/laptop/" + name for name in KEPT}
        # Unset, as hotbed places the run: a node's name, which it no longer takes, is refused.
        del here["CLADEFS_TEST_HOST"]
        here["CLADEFS_HOTBED_WORK"] = "/work"
        here["CLADEFS_HOTBED_REPO"] = "cladefs"
        # Knobs for the workspace: where its cells keep their cells, and its daemons what they
        # read from them. And a path of this machine's build that the host scripts set themselves.
        host = {
            "CLADEFSD_BUCKET": "s3://bucket/prefix",
            "CLADEFSD_PART_CACHE": "/cache",
            "CLADEFS_FIRECRACKER": "/laptop/firecracker",
        }
        self.assertEqual(
            self.host_command({**here, **host}, "test-linux-host.sh")[0],
            sorted(BASE + [
                "CLADEFSD_PART_CACHE=/cache",
                "CLADEFSD_BUCKET=s3://bucket/prefix",
                "CLADEFS_FIRECRACKER=/laptop/firecracker",
            ]),
        )

    def test_every_variable_kept_here_is_one_the_code_reads(self):
        # A name the code no longer reads, as after a rename, would keep nothing here, and send
        # the new name, which may name this machine's own setup, to the workspace.
        body = re.search(r"\nhost_env_local\(\) \{\n(.*?)\n\}\n", HOST.read_text(), re.S).group(1)
        patterns = re.findall(r"^ *([A-Z0-9_ |]+)\) ;;$", body, re.M)
        self.assertEqual([name for pattern in patterns for name in pattern.split(" | ")], KEPT)
        read = set()
        for directory in ["scripts", "crates", "guest"]:
            for parent, children, files in os.walk(SCRIPTS.parent / directory):
                # Not what was built, nor these tests, whose names are their own.
                children[:] = [
                    child for child in children
                    if child != "target" and Path(parent, child) != SCRIPTS / "tests"
                ]
                for path in (Path(parent, file) for file in files):
                    if path.suffix in [".rs", ".sh", ".py"] and path != HOST:
                        read.update(re.findall(r"\bCLADEFSD?_\w+", path.read_text(errors="replace")))
        self.assertEqual([name for name in KEPT if name not in read], [])

    def test_empty_values_are_not_sent(self):
        empty = {"CLADEFS_ZSTD_LEVEL": "", "CLADEFS_BENCH_PERF": ""}
        self.assertEqual(self.host_command(empty, "test-linux-host.sh"), (sorted(BASE), COMMAND))

    def test_values_arrive_as_they_are(self):
        note = "a 'quoted' \"note\": $HOME, `id`, \\back\\slashes\\, *?, ×\nand a line"
        self.assertEqual(
            self.host_command({"CLADEFS_BENCH_NOTE": note}, "test-linux-host.sh", note),
            (sorted(BASE + [f"CLADEFS_BENCH_NOTE={note}"]), COMMAND + [note]),
        )

    def test_the_callers_values_replace_this_machines(self):
        self.assertEqual(
            self.host_command(
                {"CLADEFSD_BUCKET": "s3://bucket/prefix", "CLADEFS_ZSTD_LEVEL": "3",
                 # The run's own, which no caller's replaces.
                 "CLADEFS_TEST_HOST_WORK": "/laptop/work"},
                "CLADEFSD_BUCKET=s3://bucket/prefix/run-x",
                # Empty, unset there.
                "CLADEFS_ZSTD_LEVEL=",
                "test-linux-host.sh",
                "CLADEFS_NOT=an argument",
            ),
            (sorted(BASE + ["CLADEFSD_BUCKET=s3://bucket/prefix/run-x"]),
             COMMAND + ["CLADEFS_NOT=an argument"]),
        )

    def test_the_builds_jobs_are_the_callers(self):
        sent, _ = self.host_command(
            {"CLADEFS_TEST_HOST_JOBS": "2", "CLADEFS_GUEST_KERNEL_JOBS": "3"}, "test-linux-host.sh")
        self.assertIn("CARGO_BUILD_JOBS=2", sent)
        self.assertIn("NEXTEST_TEST_THREADS=2", sent)
        # The kernel's jobs the caller's, as it named them.
        self.assertIn("CLADEFS_GUEST_KERNEL_JOBS=3", sent)
        self.assertNotIn("CLADEFS_GUEST_KERNEL_JOBS=2", sent)


# A host script, as the fleet's origin has every one of them: says each job of it, its phase
# (CLADEFS_TEST_HOST_PHASE, all if none) and whether it ran as work or a measurement, a line of
# RESULTS/jobs.txt; builds, as the host scripts do, unless it measures, which a measurement refuses
# (scripts/lib/measurement.sh, TASK-368); and, but for a build alone, says how it was run, its
# arguments and the variables it was given, in RESULTS/ran.txt, and does as CLADEFS_FAKE_HOST says:
# fail; block until it is ended, its pid in RESULTS/waiting.pid; or a phase of a migration, the
# source's leaving its snapshot in the bundle, the target's taking it from there.
# CLADEFS_FAKE_HOST=fail-build fails its build.
FAKE_HOST_SCRIPT = f"""#!/bin/bash
set -euo pipefail
source=$1 results=$2
shift 2
source "$source/scripts/lib/measurement.sh"
phase=${{CLADEFS_TEST_HOST_PHASE:-all}} where=work
if measuring; then where=bench; fi
echo "$(basename "$0") $phase $where" >> "$results/jobs.txt"
if [[ $phase != measure ]]; then
    measurement_refuses "$(basename "$0")'s build"
    [[ ${{CLADEFS_FAKE_HOST:-}} != fail-build ]] || exit 4
fi
if [[ $phase == build ]]; then exit 0; fi
{{
    echo "$(basename "$0") $*"
    echo "source $source"
    env | grep -E '^(CLADEFSD?_|PJDFSTEST_|CARGO_BUILD_JOBS=)' | sort
}} > "$results/ran.txt"
echo "ran $(basename "$0") $*"
case ${{CLADEFS_FAKE_HOST:-}} in
    fail) exit 3 ;;
    wait)
        echo $$ > "$results/waiting.pid"
        exec {shlex.quote(sys.executable)} -c 'import signal; signal.pause()' ;;
    migrate)
        if [[ $CLADEFS_MIGRATE_PHASE == source ]]; then
            mkdir -p "$CLADEFS_MIGRATE_BUNDLE"
            echo "a VM, from $source" > "$CLADEFS_MIGRATE_BUNDLE/snapshot"
        else
            cat "$CLADEFS_MIGRATE_BUNDLE/snapshot"
        fi
        ;;
esac
"""
HOST_SCRIPTS = ["test", "bench", "bench-ab", "gets", "mutants", "fork-time", "gc-sweep"]
# The drivers, and what they run, as the origin has them.
DRIVERS = ["test-host.sh", "bench-host.sh", "bench-ab-host.sh", "gets-host.sh", "mutants-host.sh",
           "fork-time-host.sh", "gc-sweep-host.sh", "migrate-host.sh", "perf-gate-host.sh",
           "soak-host.sh", "build-commit.sh", "lib/host.sh", "lib/host-workspace.sh",
           "lib/measurement.sh"]
# Copied, not linked: they find the tree they keep their results in by where they are.
COPIED = ["bench-history.py", "bench_results.py"]
IDENTITY = {"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t",
            "GIT_COMMITTER_EMAIL": "t@t", "GIT_CONFIG_COUNT": "1",
            "GIT_CONFIG_KEY_0": "protocol.file.allow", "GIT_CONFIG_VALUE_0": "always"}


class Fleet:
    """fake_hotbed.py's node, in a temporary directory: an origin, a checkout of cladefs whose
    vendor/firecracker is a submodule, its scripts the drivers and FAKE_HOST_SCRIPT, which the
    drivers run from; and the workspace, work, whose checkout hotbed syncs from it."""

    def __init__(self, arch=None):
        # As git names it, its links resolved (macOS's /var); removed with the fleet.
        self.directory = Path(tempfile.mkdtemp()).resolve()
        weakref.finalize(self, shutil.rmtree, self.directory, True)
        (self.directory / "calls").write_text("")
        if arch:
            (self.directory / "arch").write_text(arch + "\n")
        (self.directory / "bin").mkdir()
        hotbed = self.directory / "bin" / "hotbed"
        hotbed.write_text(
            f'#!/bin/sh\nexec {shlex.quote(sys.executable)} {shlex.quote(str(FAKE_HOTBED))} "$@"\n')
        hotbed.chmod(0o755)
        self.work = self.directory / "work"
        self.checkout = self.work / "cladefs"
        self.environment = {
            "PATH": f"{self.directory / 'bin'}:/usr/bin:/bin",
            "HOME": str(self.directory),
            "FAKE_HOTBED": str(self.directory),
            "CLADEFS_HOTBED_WORK": str(self.work),
            **IDENTITY,
        }
        self.firecracker = self.commit(self.directory / "firecracker", {"README": "firecracker\n"})
        self.origin = self.directory / "origin"
        files = {"README": "cladefs\n", ".gitignore": "/docs/measurements/\n/target/\n"}
        files.update({f"scripts/lib/{name}-linux-host.sh": FAKE_HOST_SCRIPT for name in HOST_SCRIPTS})
        self.commit(self.origin, files)
        for name in DRIVERS:
            (self.origin / "scripts" / name).symlink_to(SCRIPTS / name)
        for name in COPIED:
            shutil.copy2(SCRIPTS / name, self.origin / "scripts" / name)
        self.git(self.origin, "submodule", "add", "-q", str(self.directory / "firecracker"),
                 "vendor/firecracker")
        self.git(self.origin, "add", ".")
        self.git(self.origin, "commit", "-q", "-m", "cladefs")

    def git(self, tree, *args):
        return subprocess.run(["git", "-C", str(tree), *args], env=self.environment,
                              capture_output=True, text=True, check=True).stdout.strip()

    def commit(self, tree, files):
        """Commits `files`, by their paths, to `tree`, made a checkout if it is not one: its
        commit."""
        if not (tree / ".git").exists():
            tree.mkdir(parents=True, exist_ok=True)
            self.git(tree, "init", "-q", "-b", "main")
        for name, text in files.items():
            (tree / name).parent.mkdir(parents=True, exist_ok=True)
            (tree / name).write_text(text)
            if name.endswith(".sh"):
                (tree / name).chmod(0o755)
        self.git(tree, "add", ".")
        self.git(tree, "commit", "-q", "-m", "a commit")
        return self.git(tree, "rev-parse", "HEAD")

    def head(self):
        return self.git(self.origin, "rev-parse", "HEAD")

    def calls(self):
        return (self.directory / "calls").read_text().splitlines()

    def runs(self):
        """The runs' directories in the workspace."""
        runs = self.checkout / "target" / "host-runs"
        return sorted(path.name for path in runs.iterdir()) if runs.exists() else []

    def worktrees(self):
        """The workspace checkout's worktrees, but its own."""
        if not self.checkout.exists():
            return []
        listed = self.git(self.checkout, "worktree", "list", "--porcelain")
        return [line.split(" ", 1)[1] for line in listed.splitlines()
                if line.startswith("worktree ")][1:]

    def drive(self, script, preamble="", environment=None):
        """Runs `script` in macOS's bash, after sourcing host.sh as a driver in the origin does,
        `preamble` before it: its status, output and errors."""
        driver = f"""set -euo pipefail
repo={shlex.quote(str(self.origin))} results=$HOME/results limit=6h
{preamble}
source "$repo/scripts/lib/host.sh"
"""
        done = subprocess.run(["/bin/bash", "-c", driver + script], capture_output=True,
                              text=True, env={**self.environment, **(environment or {})})
        return done.returncode, done.stdout, done.stderr

    def run_driver(self, driver, *args, environment=None):
        """Runs the origin's scripts/`driver` with `args`: its status, output and errors."""
        done = subprocess.run(["/bin/bash", self.origin / "scripts" / driver, *args],
                              capture_output=True, text=True,
                              env={**self.environment, **(environment or {})})
        return done.returncode, done.stdout, done.stderr


# A run's command: start, run the test host script, fetch, as test-host.sh does.
RAN = """host_start
status=0
host_job test-linux-host.sh a b > "$results/remote.log" 2>&1 || status=$?
if [[ $host_state == ended ]]; then host_fetch || status=$?; fi
echo "status $status"
"""
NAME = r"run-\d{8}T\d{6}Z-[0-9a-f]{8}"


class RunNames(unittest.TestCase):
    """Each run's name, and its directory in the workspace (host_run)."""

    def test_two_drivers_started_at_once_name_their_runs_apart(self):
        # Two drivers, in the same second: each sources host.sh, with a mark of its own.
        fleet = Fleet()
        status, output, errors = fleet.drive(
            'echo "$host_run"; bash -c \'source "$0"; echo "$host_run"\' "$repo/scripts/lib/host.sh"'
        )
        self.assertEqual((status, errors), (0, ""))
        first, second = output.splitlines()
        self.assertRegex(first, f"^{NAME}$")
        self.assertRegex(second, f"^{NAME}$")
        self.assertNotEqual(first.rsplit("-", 1)[1], second.rsplit("-", 1)[1])

    def test_a_run_is_named_for_its_number_and_once(self):
        fleet = Fleet()
        status, output, errors = fleet.drive('echo "$host_run"; echo "$host_dir"',
                                             environment={"CLADEFS_TEST_HOST_RUN": "r1040"})
        self.assertEqual((status, errors), (0, ""))
        run, directory = output.splitlines()
        self.assertRegex(run, r"^run-r1040-\d{8}T\d{6}Z-[0-9a-f]{8}$")
        self.assertEqual(directory, f"{fleet.checkout}/target/host-runs/{run}")

    def test_a_node_and_a_run_number_are_named_as_they_may_be(self):
        fleet = Fleet()
        # A node's name, which hotbed no longer takes (board 14457), refused rather than ignored.
        for named in ["node1", "a host"]:
            self.assertEqual(
                fleet.drive("true", environment={"CLADEFS_TEST_HOST": named}),
                (2, "", f"CLADEFS_TEST_HOST is unset (hotbed places the run) or local, not {named}\n"))
        self.assertEqual(
            fleet.drive("true", environment={"CLADEFS_TEST_HOST_RUN": "R1"}),
            (2, "", "CLADEFS_TEST_HOST_RUN names a run in lower-case letters and digits, such as r1040\n"))


class Runs(unittest.TestCase):
    """A run's life: its directory in the workspace, its tree, which its job runs in, removed
    whatever happens."""

    def test_a_run_runs_the_checkout_and_leaves_nothing(self):
        fleet = Fleet()
        status, out, errors = fleet.drive(RAN)
        self.assertEqual((status, out), (0, "status 0\n"), errors)
        results = fleet.directory / "results"
        ran = (results / "ran.txt").read_text().splitlines()
        self.assertEqual(ran[:2], ["test-linux-host.sh a b", f"source {fleet.checkout}"])
        self.assertEqual((results / "remote.log").read_text(), "ran test-linux-host.sh a b\n")
        self.assertEqual((fleet.runs(), fleet.worktrees()), ([], []))
        # A session started for the run, and ended with it; the checkout synced once, first.
        self.assertEqual(
            fleet.calls(),
            ["session current", "session start", "run bash", "run --detach --no-sync --mem 8G bash",
             "run --no-sync bash", "run --no-sync bash", "run --no-sync bash", "session end"])

    def test_a_run_keeps_to_the_session_it_has(self):
        fleet = Fleet()
        (fleet.directory / "session").write_text("mine\n")
        status, out, errors = fleet.drive(RAN)
        self.assertEqual((status, out), (0, "status 0\n"), errors)
        self.assertEqual(fleet.calls()[:2], ["session current", "run bash"])
        self.assertNotIn("session end", fleet.calls())
        self.assertEqual((fleet.directory / "session").read_text(), "mine\n")

    def test_a_local_host_runs_here_without_hotbed(self):
        """CLADEFS_TEST_HOST=local: the run runs in this checkout, as a run inside a hotbed command
        does."""
        fleet = Fleet()
        status, out, errors = fleet.drive(RAN + 'host_label\n',
                                          environment={"CLADEFS_TEST_HOST": "local"})
        hostname = subprocess.run(["hostname", "-s"], capture_output=True, text=True).stdout
        self.assertEqual((status, out), (0, f"status 0\n{hostname}"), errors)
        results = fleet.directory / "results"
        self.assertEqual((results / "ran.txt").read_text().splitlines()[:2],
                         ["test-linux-host.sh a b", f"source {fleet.origin}"])
        self.assertEqual(fleet.calls(), [])
        self.assertFalse((fleet.origin / "target" / "host-runs").exists() and
                         any((fleet.origin / "target" / "host-runs").iterdir()))

    def test_a_run_whose_job_fails_leaves_nothing(self):
        fleet = Fleet()
        status, out, _ = fleet.drive(RAN, environment={"CLADEFS_FAKE_HOST": "fail"})
        self.assertEqual((status, out), (0, "status 3\n"))
        self.assertEqual((fleet.directory / "results" / "remote.log").read_text(),
                         "ran test-linux-host.sh a b\n")
        self.assertEqual((fleet.runs(), fleet.worktrees()), ([], []))

    def test_a_run_needing_another_architecture_makes_nothing_there(self):
        fleet = Fleet(arch="aarch64")
        need = 'host_need_arch x86_64 "a driver: its guests boot" "an x86-64 kernel"\n'
        # Placed by architecture, which no node has.
        status, out, errors = fleet.drive(need + RAN)
        self.assertEqual((status, out), (2, ""))
        self.assertEqual(errors, "error: no node of architecture amd64\n")
        self.assertEqual((fleet.runs(), fleet.worktrees()), ([], []))
        # A run that needs none runs there; one that needs the node's own runs too.
        self.assertEqual(fleet.drive(RAN)[:2], (0, "status 0\n"))
        self.assertEqual(fleet.drive('host_need_arch aarch64 "arm64"\n' + RAN)[:2],
                         (0, "status 0\n"))

    def test_test_host_runs_the_workspaces_tests_alone_on_an_arm64_node(self):
        fleet = Fleet(arch="aarch64")
        results = fleet.directory / "results"
        status, _, errors = fleet.run_driver("test-host.sh", str(results))
        self.assertEqual(status, 2)
        self.assertIn("error: no node of architecture amd64", errors)
        self.assertEqual(fleet.runs(), [])
        status, _, errors = fleet.run_driver("test-host.sh", str(results),
                                             environment={"CLADEFS_GUEST_TESTS": "0"})
        self.assertEqual(status, 0, errors)
        self.assertEqual((results / "remote.log").read_text(), "ran test-linux-host.sh \n")

    def test_a_driver_that_fails_leaves_nothing(self):
        fleet = Fleet()
        status, _, _ = fleet.drive("host_start\nfalse\necho unreached")
        self.assertEqual(status, 1)
        self.assertEqual((fleet.runs(), fleet.worktrees()), ([], []))

    def test_a_checkout_that_changes_what_it_tracks_runs_its_changes(self):
        fleet = Fleet()
        (fleet.origin / "README").write_text("changed, not committed\n")
        self.assertEqual(fleet.drive(RAN + 'cat "$results/source.txt"\n')[:2],
                         (0, f"status 0\nrevision {fleet.head()}+changes\n"
                             f"firecracker {fleet.firecracker}\n"))
        self.assertEqual((fleet.checkout / "README").read_text(), "changed, not committed\n")

    def test_a_named_commit_is_the_one_run(self):
        fleet = Fleet()
        older = fleet.head()
        fleet.commit(fleet.origin, {"README": "newer\n"})
        status, out, errors = fleet.drive(RAN, environment={"CLADEFS_TEST_HOST_COMMIT": "HEAD~"})
        self.assertEqual((status, out), (0, "status 0\n"), errors)
        results = fleet.directory / "results"
        self.assertEqual((results / "source.txt").read_text().splitlines()[0], f"revision {older}")
        run = (results / "run.txt").read_text().splitlines()[0].removeprefix("run ")
        self.assertEqual((results / "ran.txt").read_text().splitlines()[1],
                         f"source {fleet.checkout}/target/host-runs/{run}/tree")
        self.assertEqual((fleet.runs(), fleet.worktrees()), ([], []))
        self.assertEqual(fleet.drive(RAN, environment={"CLADEFS_TEST_HOST_COMMIT": "nowhere"}),
                         (2, "", f"nowhere: no commit of {fleet.origin}'s\n"))

    def test_two_runs_at_once_keep_apart(self):
        # The first stopped after its start, its trap not run, as a driver killed: the second's
        # run and end leave it alone.
        fleet = Fleet()
        (fleet.directory / "session").write_text("mine\n")
        status, first, _ = fleet.drive('host_start; echo "$host_run"; trap - EXIT')
        self.assertEqual(status, 0)
        self.assertEqual(fleet.drive(RAN)[:2], (0, "status 0\n"))
        self.assertEqual(fleet.runs(), [first.strip()])

    def test_a_job_runs_as_work_and_a_measurement_alone_after_its_build_as_work(self):
        """TASK-368: a measurement's run is two commands, its build as work, then the measurement
        alone under hotbed run --bench, building nothing; any other run's one command of work, of
        no phase, kept though it idles with CLADEFS_TEST_HOST_KEEP_IDLE=1 (board 14415)."""
        for bench, keep_idle, said, jobs in [
            ("0", "0", ["run --detach --no-sync --mem 8G bash"], ["test-linux-host.sh all work"]),
            ("0", "1", ["run --detach --keep-idle --no-sync --mem 8G bash"],
             ["test-linux-host.sh all work"]),
            ("1", "0", ["run --detach --no-sync --mem 8G bash",
                        "run --bench --detach --keep-idle --no-sync --mem 12G bash"],
             ["test-linux-host.sh build work", "test-linux-host.sh measure bench"]),
        ]:
            with self.subTest(bench=bench, keep_idle=keep_idle):
                fleet = Fleet()
                status, out, errors = fleet.drive(RAN, environment={
                    "CLADEFS_TEST_HOST_BENCH": bench, "CLADEFS_TEST_HOST_KEEP_IDLE": keep_idle})
                self.assertEqual((status, out), (0, "status 0\n"), errors)
                results = fleet.directory / "results"
                self.assertEqual([call for call in fleet.calls() if "--detach" in call], said)
                self.assertEqual((results / "jobs.txt").read_text().splitlines(), jobs)
                # The build's log apart, the measurement's the run's own.
                self.assertEqual((results / "remote-build.log").exists(), bench == "1")
                self.assertEqual((results / "remote.log").read_text(), "ran test-linux-host.sh a b\n")

    def test_a_measurement_whose_build_failed_measures_nothing(self):
        fleet = Fleet()
        status, out, errors = fleet.drive(
            RAN, environment={"CLADEFS_TEST_HOST_BENCH": "1", "CLADEFS_FAKE_HOST": "fail-build"})
        self.assertEqual((status, out), (0, "status 4\n"), errors)
        self.assertEqual((fleet.directory / "results" / "jobs.txt").read_text().splitlines(),
                         ["test-linux-host.sh build work"])
        self.assertFalse(any("--bench" in call for call in fleet.calls()))
        self.assertEqual(fleet.runs(), [])

    def test_a_measurement_that_would_build_is_refused(self):
        """The guard (scripts/lib/measurement.sh): a driver that runs its host script whole as one
        measurement, its builds in it, as before TASK-368, is refused, saying why, at once."""
        fleet = Fleet()
        status, out, errors = fleet.drive(
            "host_start\nstatus=0\n"
            'host_launch test-linux-host.sh a b > "$results/remote.log" 2>&1 || status=$?\n'
            'if [[ $host_state == ended ]]; then host_fetch || status=$?; fi\necho "status $status"\n',
            environment={"CLADEFS_TEST_HOST_BENCH": "1"})
        self.assertEqual((status, out), (0, "status 2\n"), errors)
        results = fleet.directory / "results"
        self.assertEqual((results / "jobs.txt").read_text().splitlines(),
                         ["test-linux-host.sh all bench"])
        self.assertFalse((results / "ran.txt").exists())
        refusal = (results / "remote.log").read_text()
        self.assertRegex(refusal, r"^test-linux-host.sh's build would build inside a measurement "
                                  r"\(hotbed run --bench, 0::/user.slice/user-1000.slice/"
                                  r"user-1000-hotbed_bench.slice/hotbed-[0-9a-f]+.service\)")
        self.assertTrue(refusal.endswith("(TASK-368)\n"), refusal)


class Source(unittest.TestCase):
    """host_start: the run's trees, what they hold, and their records here."""

    def test_the_checkout_is_run_and_recorded(self):
        fleet = Fleet()
        self.assertEqual(fleet.drive(RAN)[:2], (0, "status 0\n"))
        results = fleet.directory / "results"
        self.assertEqual(
            (results / "source.txt").read_text().splitlines(),
            [f"revision {fleet.head()}", f"firecracker {fleet.firecracker}"])
        run, node, session, directory = (results / "run.txt").read_text().splitlines()
        self.assertRegex(run, f"^run {NAME}$")
        self.assertEqual((node, session, directory),
                         ("host node1", "session fake-session",
                          f"directory {fleet.checkout}/target/host-runs/{run[4:]}"))

    def worktree(self, script, environment=None):
        """Runs `script` after host_start, which leaves the run in the workspace: the fleet and
        the run's directory there."""
        fleet = Fleet()
        status, out, errors = fleet.drive(f'host_start\n{script}\necho "$host_dir"\ntrap - EXIT',
                                          environment=environment)
        self.assertEqual((status, errors.replace(self.placed(errors), "")), (0, ""))
        return fleet, Path(out.splitlines()[-1])

    @staticmethod
    def placed(errors):
        """What hotbed said of where it ran each command, which a driver passes on."""
        return "".join(line + "\n" for line in errors.splitlines() if line.startswith("hotbed: "))

    def test_a_patch_is_applied_to_a_worktree_and_recorded(self):
        patch = Path(tempfile.mkdtemp()) / "mutant.patch"
        patch.write_text("--- a/README\n+++ b/README\n@@ -1 +1 @@\n-cladefs\n+mutated\n")
        fleet, directory = self.worktree("", {"CLADEFS_TEST_HOST_PATCH": str(patch)})
        tree = directory / "tree"
        self.assertEqual((tree / "README").read_text(), "mutated\n")
        self.assertEqual((tree / "vendor" / "firecracker" / "README").read_text(), "firecracker\n")
        self.assertEqual((tree / "source-revision.txt").read_text(), f"{fleet.head()}-dirty\n")
        digest = subprocess.run(["shasum", "-a", "256", patch], capture_output=True, text=True,
                                check=True).stdout.split()[0]
        source = (fleet.directory / "results" / "source.txt").read_text().splitlines()
        self.assertEqual(source[0], f"revision {fleet.head()}+changes")
        self.assertEqual(source[2], f"patch {digest} mutant.patch")
        # The checkout itself untouched.
        self.assertEqual((fleet.checkout / "README").read_text(), "cladefs\n")

    def test_a_guest_kernel_is_one_of_the_workspaces(self):
        fleet = Fleet()
        kernel = f"{fleet.work}/kernels/vmlinux"
        status, _, errors = fleet.drive(RAN, environment={"CLADEFS_GUEST_KERNEL": kernel})
        self.assertEqual(status, 0, errors)
        self.assertEqual((fleet.directory / "results" / "source.txt").read_text().splitlines()[2],
                         f"guest kernel {kernel}")
        self.assertIn(f"CLADEFS_GUEST_KERNEL={kernel}",
                      (fleet.directory / "results" / "ran.txt").read_text().splitlines())
        self.assertEqual(
            fleet.drive(RAN, environment={"CLADEFS_GUEST_KERNEL": "/laptop/vmlinux"}),
            (2, "", "CLADEFS_GUEST_KERNEL names a kernel in the workspace (/work/...), or none, "
                    "which the workspace builds from the tree: /laptop/vmlinux is this machine's\n"))

    def test_other_commits_have_worktrees_of_their_own(self):
        fleet = Fleet()
        older = fleet.head()
        newer = fleet.commit(fleet.origin, {"README": "newer\n"})
        status, out, errors = fleet.drive(
            'host_start base=HEAD~ same=HEAD; echo "$host_dir"; trap - EXIT')
        self.assertEqual(status, 0, errors)
        directory = Path(out.strip())
        self.assertEqual((directory / "trees" / "base" / "README").read_text(), "cladefs\n")
        self.assertEqual((directory / "trees" / "same" / "README").read_text(), "newer\n")
        for label, commit in [("base", older), ("same", newer)]:
            self.assertEqual(
                (fleet.directory / "results" / label / "source.txt").read_text().splitlines(),
                [f"revision {commit}", f"firecracker {fleet.firecracker}"])
        self.assertEqual(fleet.drive("host_start BASE=HEAD"),
                         (2, "", "BASE=HEAD: LABEL=COMMIT, LABEL in lower-case letters, digits "
                                 "and -\n"))


class Fetch(unittest.TestCase):
    """host_fetch: the results come back, but what isn't results."""

    def test_cells_buckets_images_and_root_filesystems_are_not_fetched(self):
        """board 1868: a job killed mid-case never prunes its cells, bases and images, and the fetch
        copied 7.1 GB of them. Those directories stay; files of the same names, as perf's, are
        results."""
        fleet = Fleet()
        left = ["e2e/fxfs-guest/extents", "e2e/fxfs-guest/vm.root", "e2e/fxfs-guest/vm.data",
                "e2e/rootfs", "sandboxes/a.base", "sandboxes/sandbox-image", "sandboxes/store-1",
                "sandboxes/x.bucket", "sandboxes/disk.img", "sandboxes/.cladefsd-cache.ab12"]
        remote = "".join(f'mkdir -p "$d/{d}"; echo big > "$d/{d}/f"; ' for d in left)
        remote += 'echo samples > "$d/e2e/fxfs-guest/perf.data"; echo log > "$d/sandboxes/store-1.log"'
        status, out, errors = fleet.drive(
            'host_start\n'
            f'host_shell "$(printf \'d=%q; \' "$host_dir/results")"{shlex.quote(remote)} > /dev/null\n'
            'status=0; host_fetch || status=$?; echo "status $status"')
        results = fleet.directory / "results"
        self.assertEqual((status, out.splitlines()[-1]), (0, "status 0"), errors)
        self.assertEqual(
            sorted(str(path.relative_to(results)) for path in results.rglob("*")),
            ["e2e", "e2e/fxfs-guest", "e2e/fxfs-guest/perf.data", "remote.log", "run.txt",
             "sandboxes", "sandboxes/store-1.log", "source.txt"])


    def test_a_run_whose_results_did_not_come_back_stays_for_a_resume(self):
        """board 14634: the fetch's archive cut short (hotbed issue 26) removed the run all the
        same, the only whole copy of its results."""
        fleet = Fleet()
        (fleet.directory / "session").write_text("mine\n")
        cut_short = 'base64() { cat > /dev/null; return 1; }\necho "$host_run"\n'
        status, out, errors = fleet.drive(cut_short + RAN)
        run, said = out.splitlines()
        self.assertEqual((status, said), (0, "status 1"), errors)
        self.assertEqual(fleet.runs(), [run])
        self.assertEqual(errors.splitlines()[-1],
                         f"{run}'s results did not come back; it stays in the workspace: fetch them "
                         f"with `/bin/bash resume {run} {fleet.directory / 'results'}`.")
        results = fleet.directory / "resumed"
        status, out, errors = fleet.run_driver("test-host.sh", "resume", run, str(results))
        self.assertEqual((status, out), (0, ""), errors)
        self.assertEqual((results / "ran.txt").read_text().splitlines()[0], "test-linux-host.sh a b")
        self.assertEqual(fleet.runs(), [])


class Resume(unittest.TestCase):
    """A driver's resume and cancel (host_actions), of a run whose driver stopped waiting for its
    job."""

    def left(self, environment):
        """A run left in the workspace as a driver killed leaves it, its job launched with
        `environment` and not waited for: the fleet and the run's name."""
        fleet = Fleet()
        (fleet.directory / "session").write_text("mine\n")
        status, out, errors = fleet.drive(
            'host_start\nhost_wait() { :; }\nhost_launch test-linux-host.sh\n'
            'host_state=launched\necho "$host_run"\ntrap - EXIT', environment=environment)
        self.assertEqual(status, 0, errors)
        return fleet, out.strip()

    def test_a_run_whose_job_ended_is_resumed_and_ended(self):
        fleet, run = self.left({})
        results = fleet.directory / "resumed"
        status, out, errors = fleet.run_driver("test-host.sh", "resume", run, str(results))
        self.assertEqual((status, out), (0, ""), errors)
        self.assertEqual((results / "ran.txt").read_text().splitlines()[0], "test-linux-host.sh ")
        self.assertEqual((results / "remote.log").read_text(), "ran test-linux-host.sh \n")
        self.assertEqual(fleet.runs(), [])

    def test_a_running_job_is_cancelled(self):
        fleet, run = self.left({"CLADEFS_FAKE_HOST": "wait"})
        status, out, errors = fleet.run_driver("test-host.sh", "cancel", run)
        self.assertEqual((status, out, errors.splitlines()[-1]), (0, "", f"Ended {run}."))
        self.assertEqual(fleet.runs(), [])

    def test_only_a_run_is_resumed(self):
        fleet = Fleet()
        self.assertEqual(fleet.run_driver("test-host.sh", "resume", "cladefs-test-x", "results"),
                         (2, "", "Name the run, run-...\n"))
        self.assertEqual(fleet.run_driver("test-host.sh", "resume", "run-x"),
                         (2, "", f"Usage: {fleet.origin}/scripts/test-host.sh resume RUN RESULTS\n"))


class Drivers(unittest.TestCase):
    """Each driver's run, end to end, in fake_hotbed.py's workspace."""

    def test_the_tests_run_as_asked(self):
        fleet = Fleet()
        results = fleet.directory / "tested"
        status, _, errors = fleet.run_driver(
            "test-host.sh", str(results),
            environment={"CLADEFS_E2E_TEST": "fxfs-guest --tag dax-gate",
                         "CLADEFSD_BUCKET": "s3://bucket/p"})
        self.assertEqual(status, 0, errors)
        ran = (results / "ran.txt").read_text().splitlines()
        self.assertEqual(ran[0], "test-linux-host.sh fxfs-guest --tag dax-gate")
        run = (results / "run.txt").read_text().split()[1]
        self.assertIn(f"CLADEFSD_BUCKET=s3://bucket/p/{run}", ran)
        self.assertEqual(errors.splitlines()[-1], f"Results: {results}")
        self.assertEqual(fleet.runs(), [])

    def test_a_bench_is_a_measurement_with_its_note(self):
        fleet = Fleet()
        results = fleet.origin / "docs" / "measurements" / "node1-fs-bench-x"
        status, _, errors = fleet.run_driver("bench-host.sh", str(results),
                                             environment={"CLADEFS_BENCH_NOTE": "a note"})
        self.assertEqual(status, 0, errors)
        self.assertEqual((results / "ran.txt").read_text().splitlines()[0], "bench-linux-host.sh 3")
        self.assertTrue(any("--bench" in call for call in fleet.calls()))
        self.assertEqual((results / "note.md").read_text().splitlines()[0], "a note")
        # The history page is rewritten by hand, where every run it lists is (bench-history.py).
        self.assertFalse((fleet.origin / "docs" / "measurements" / "fs-bench-history.md").exists())

    def test_an_ab_builds_each_sides_commit(self):
        fleet = Fleet()
        base = fleet.head()
        fleet.commit(fleet.origin, {"README": "newer\n"})
        results = fleet.directory / "ab"
        status, _, errors = fleet.run_driver(
            "bench-ab-host.sh", str(results),
            environment={"CLADEFS_AB_A": "base=HEAD~", "CLADEFS_AB_B": "change=HEAD",
                         "CLADEFS_AB_ROUNDS": "2"})
        self.assertEqual(status, 0, errors)
        run = (results / "run.txt").read_text().split()[1]
        trees = f"{fleet.checkout}/target/host-runs/{run}/trees"
        self.assertEqual((results / "ran.txt").read_text().splitlines()[0],
                         f"bench-ab-linux-host.sh {trees}/base {trees}/change 2 base change")
        self.assertEqual((results / "base" / "source.txt").read_text().splitlines()[0],
                         f"revision {base}")
        self.assertEqual((results / "jobs.txt").read_text().splitlines(),
                         ["bench-ab-linux-host.sh build work", "bench-ab-linux-host.sh measure bench"])
        self.assertTrue((results / "remote-build.log").exists())

    def test_every_timed_driver_builds_as_work_then_measures_alone(self):
        """TASK-368: no driver's measurement builds. Each measures in two commands of its run, its
        builds as work, then the measurement alone, which would refuse to build (FAKE_HOST_SCRIPT,
        scripts/lib/measurement.sh); perf-gate-host.sh's and soak-host.sh's through test-host.sh."""
        for driver, args, environment, outs in [
            ("bench-host.sh", ["RESULTS"], {}, {".": "bench-linux-host.sh"}),
            ("fork-time-host.sh", ["RESULTS", "main=HEAD"], {}, {".": "fork-time-linux-host.sh"}),
            ("gets-host.sh", ["RESULTS"], {"CLADEFS_GETS_URL": "file:///gets/"},
             {".": "gets-linux-host.sh"}),
            ("gc-sweep-host.sh", ["RESULTS"], {}, {".": "gc-sweep-linux-host.sh"}),
            ("bench-ab-host.sh", ["RESULTS"], {"CLADEFS_AB_A": "a=HEAD", "CLADEFS_AB_B": "b=same"},
             {".": "bench-ab-linux-host.sh"}),
            ("test-host.sh", ["RESULTS"], {"CLADEFS_TEST_HOST_BENCH": "1"}, {".": "test-linux-host.sh"}),
            ("perf-gate-host.sh", ["RESULTS", "single", "density"], {},
             {"single": "test-linux-host.sh", "density": "test-linux-host.sh"}),
            ("soak-host.sh", ["RESULTS", "1024:2:2048"], {"CLADEFS_TEST_HOST_BENCH": "1"},
             {"1024m-2c-2048w": "test-linux-host.sh"}),
        ]:
            with self.subTest(driver=driver):
                fleet = Fleet()
                results = fleet.origin / "docs" / "measurements" / "node1-x"
                args = [str(results) if arg == "RESULTS" else arg for arg in args]
                status, _, errors = fleet.run_driver(driver, *args, environment=environment)
                self.assertEqual(status, 0, errors)
                for out, script in outs.items():
                    self.assertEqual((results / out / "jobs.txt").read_text().splitlines(),
                                     [f"{script} build work", f"{script} measure bench"], out)
                launched = [call for call in fleet.calls() if "--detach" in call]
                self.assertEqual(["--bench" in call for call in launched], [False, True] * len(outs))
                self.assertEqual(fleet.runs(), [])

    def test_a_driver_that_is_no_measurement_runs_one_job_of_work(self):
        fleet = Fleet()
        results = fleet.directory / "soaked"
        status, _, errors = fleet.run_driver("soak-host.sh", str(results), "1024:2:2048")
        self.assertEqual(status, 0, errors)
        self.assertEqual((results / "1024m-2c-2048w" / "jobs.txt").read_text().splitlines(),
                         ["test-linux-host.sh all work"])
        self.assertFalse(any("--bench" in call for call in fleet.calls()))

    def test_an_ab_needs_its_sides(self):
        fleet = Fleet()
        for a, b, said in [
            ("/src/fxdax", "b=same", "a side is NAME=COMMIT, NAME in lower-case letters, digits "
                                     "and -: /src/fxdax\n"),
            ("a=HEAD", "a=same", "the sides need names of their own\n"),
        ]:
            with self.subTest(a=a, b=b):
                status, _, errors = fleet.run_driver("bench-ab-host.sh", "results",
                                                     environment={"CLADEFS_AB_A": a, "CLADEFS_AB_B": b})
                self.assertEqual((status, errors), (2, said))

    def test_gets_puts_its_object_under_the_runs_prefix(self):
        fleet = Fleet()
        results = fleet.directory / "gets"
        status, _, errors = fleet.run_driver(
            "gets-host.sh", str(results),
            environment={"CLADEFS_GETS_URL": "file:///scratch/gets/", "CLADEFS_GETS_MIB": "16",
                         "CLADEFS_GETS_SECS": "1"})
        self.assertEqual(status, 0, errors)
        run = (results / "run.txt").read_text().split()[1]
        self.assertEqual((results / "ran.txt").read_text().splitlines()[0],
                         f"gets-linux-host.sh file:///scratch/gets/{run} 16 1")
        self.assertEqual(fleet.run_driver("gets-host.sh", environment={"CLADEFS_GETS_URL": "a b"}),
                         (2, "", "CLADEFS_GETS_URL is a URL, as s3://bucket/prefix\n"))

    def test_mutants_sweep_the_crates_asked(self):
        fleet = Fleet()
        results = fleet.directory / "mutants"
        status, _, errors = fleet.run_driver(
            "mutants-host.sh", str(results),
            environment={"CLADEFS_MUTANTS_CRATES": "local_cell cladefs", "CLADEFS_MUTANTS_SHARD": "1/4"})
        self.assertEqual(status, 0, errors)
        ran = (results / "ran.txt").read_text().splitlines()
        self.assertEqual(ran[0], "mutants-linux-host.sh local_cell cladefs")
        self.assertIn("CLADEFS_MUTANTS_SHARD=1/4", ran)

    def test_fork_times_each_commit(self):
        fleet = Fleet()
        results = fleet.directory / "forks"
        status, _, errors = fleet.run_driver("fork-time-host.sh", str(results), "main=HEAD",
                                             "old=HEAD")
        self.assertEqual(status, 0, errors)
        run = (results / "run.txt").read_text().split()[1]
        trees = f"{fleet.checkout}/target/host-runs/{run}/trees"
        self.assertEqual((results / "ran.txt").read_text().splitlines()[0],
                         f"fork-time-linux-host.sh main={trees}/main old={trees}/old")
        self.assertEqual(fleet.run_driver("fork-time-host.sh", str(results))[0], 2)

    def test_gc_sweeps_of_sides_measure_each_commit_in_one_measurement(self):
        """TASK-395.17: an A/B of commits' sweeps, each side a worktree of its own, measured in the
        run's one measurement, by the run's scripts."""
        fleet = Fleet()
        results = fleet.directory / "gc-ab"
        status, _, errors = fleet.run_driver("gc-sweep-host.sh", str(results), "old=HEAD",
                                             "new=HEAD")
        self.assertEqual(status, 0, errors)
        run = (results / "run.txt").read_text().split()[1]
        trees = f"{fleet.checkout}/target/host-runs/{run}/trees"
        self.assertEqual((results / "ran.txt").read_text().splitlines()[0],
                         f"gc-sweep-linux-host.sh old={trees}/old new={trees}/new")
        self.assertEqual(sum("--bench" in call for call in fleet.calls()), 1)
        self.assertEqual(fleet.run_driver("gc-sweep-host.sh", "old=HEAD")[0], 2)

    def test_a_migration_restores_on_the_second_run_what_the_first_left(self):
        fleet = Fleet()
        results = fleet.directory / "migrated"
        status, _, errors = fleet.run_driver("migrate-host.sh", str(results),
                                             environment={"CLADEFS_FAKE_HOST": "migrate"})
        self.assertEqual(status, 0, errors)
        a, b = ((results / side / "ran.txt").read_text().splitlines() for side in "ab")
        run = (results / "a" / "run.txt").read_text().split()[1]
        held = f"{fleet.checkout}/target/host-runs/{run}.held"
        # The same tree on both: the workspace's checkout.
        self.assertEqual(a[1], b[1])
        for side, phase, token in [(a, "source", "1"), (b, "target", "2")]:
            self.assertIn(f"CLADEFS_MIGRATE_PHASE={phase}", side)
            self.assertIn(f"CLADEFSD_TOKEN={token}", side)
            self.assertIn(f"CLADEFSD_BUCKET=file://{held}/bucket/cells", side)
            self.assertIn(f"CLADEFS_MIGRATE_BUNDLE={held}/bundle", side)
        self.assertEqual((results / "b" / "remote.log").read_text(),
                         "ran test-linux-host.sh fxfs-guest-migrate-dax\n"
                         f"a VM, from {fleet.checkout}\n")
        # Nothing left of it, and one session for both runs, ended with them.
        self.assertEqual(fleet.runs(), [])
        self.assertEqual([call for call in fleet.calls() if call.startswith("session")],
                         ["session current", "session start", "session end"])
        self.assertEqual(errors.splitlines()[-1], f"Results: {results}")

    def test_a_migration_that_fails_leaves_nothing(self):
        fleet = Fleet()
        results = fleet.directory / "migrated"
        status, _, errors = fleet.run_driver("migrate-host.sh", str(results),
                                             environment={"CLADEFS_FAKE_HOST": "fail"})
        self.assertEqual(status, 3)
        self.assertFalse((results / "b").exists())
        self.assertEqual(fleet.runs(), [])
        self.assertEqual(errors.splitlines()[-1], f"Results: {results}")

    def test_a_migration_between_two_nodes_is_refused(self):
        fleet = Fleet()
        self.assertEqual(
            fleet.run_driver("migrate-host.sh", environment={"CLADEFS_MIGRATE_HOSTS": "a b"}),
            (2, "", "CLADEFS_MIGRATE_HOSTS is gone: both runs are on one node, CLADEFS_TEST_HOST's "
                    "(board 7817)\n"))


if __name__ == "__main__":
    unittest.main()
