# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/lib/host.sh, which the drivers that run cladefs on a Linux host of one's own
source, of its host's half, scripts/lib/host-remote.sh, and of the drivers.

host_command builds the command a run's job runs its host script with, and the environment it
runs in: bash builds the command, as a driver does, and another bash, as the host's shell, takes
it apart into the words it runs. The runs, their worktrees and their jobs run on hosts of
fake_ssh.py's: this machine, each host a home of its own, its clone of an origin made here, whose
host scripts are FAKE_HOST_SCRIPT."""

import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
import weakref
from pathlib import Path

import fake_ssh
from script import SCRIPTS

HOST = SCRIPTS / "lib" / "host.sh"
FAKE_SSH = Path(__file__).resolve().with_name("fake_ssh.py")
HOST_HOME = "/host/home"
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
]
# The run host_command's tests name, and its directory on the host.
RUN = "run-x"
DIR = f"/mnt/fxdax-scratch/fxdax-host/{RUN}"
# What every job's command sets: the user's Rust, the builds' jobs, and the run's own.
BASE = [
    f"PATH={HOST_HOME}/.cargo/bin:/usr/bin:/bin",
    f"HOME={HOST_HOME}",
    # The job's own, fxjob's (TASK-379): sudo passes none, so host_command names it, /tmp outside a
    # job.
    "TMPDIR=/tmp",
    "CARGO_BUILD_JOBS=4",
    "NEXTEST_TEST_THREADS=4",
    "GIT_CONFIG_COUNT=1",
    "GIT_CONFIG_KEY_0=safe.directory",
    "GIT_CONFIG_VALUE_0=*",
    "CLADEFS_TEST_HOST_IN_PLACE=1",
    "CLADEFS_GUEST_KERNEL_JOBS=4",
    "CLADEFS_TEST_HOST_KERNELS=/mnt/fxdax-scratch/fxdax-host/kernels",
    "CLADEFS_TEST_HOST_PROVISIONED=1",
    f"CLADEFS_TEST_HOST_WORK={DIR}/work",
]
# What follows the environment: the host script and its arguments.
COMMAND = [
    "timeout",
    "--signal=TERM",
    "--kill-after=30s",
    "6h",
    "bash",
    f"{DIR}/tree/scripts/lib/test-linux-host.sh",
    f"{DIR}/tree",
    f"{DIR}/results",
]


class HostCommand(unittest.TestCase):
    def host_command(self, environment, *args, preamble=""):
        """The words the host runs for `host_command ARGS...` of the run run-x, run in
        `environment`: what runs sudo, sudo env's variables, sorted, and the command they run."""
        built = subprocess.run(
            [
                # macOS's 3.2 there, as a driver runs in.
                "/bin/bash",
                "-c",
                f'set -euo pipefail; limit=6h; source "$0"; host_name {RUN}; {preamble}'
                'host_command "$@"',
                HOST,
                *args,
            ],
            env={"PATH": "/usr/bin:/bin", "HOME": "/laptop/home", **environment},
            capture_output=True,
            text=True,
        )
        self.assertEqual((built.returncode, built.stderr), (0, ""))
        words = subprocess.run(
            ["/bin/bash", "-c", 'eval "set -- $0"; printf "%s\\0" "$@"', built.stdout],
            env={"PATH": "/usr/bin:/bin", "HOME": HOST_HOME},
            capture_output=True,
            text=True,
            check=True,
        ).stdout.split("\0")[:-1]
        sudo = words.index("sudo")
        self.assertEqual(words[sudo + 1], "env")
        command = words.index("timeout")
        return words[:sudo], sorted(words[sudo + 2:command]), words[command:]

    def test_every_variable_reaches_the_host(self):
        self.assertEqual(
            self.host_command(
                {
                    "CLADEFS_FOLLOW_SECONDS": "600",
                    "CLADEFS_PJDFSTEST_TESTS": "chmod chown",
                    "CLADEFS_BENCH_WATCHDOG_SECS": "900",
                    "CLADEFS_ZSTD_LEVEL": "3",
                    "CLADEFS_BENCH_PGBENCH_SCALE": "50",
                    "CLADEFS_BUCKET_LATENCY_MS": "25",
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
                ["nice", "-n", "19"],
                sorted(BASE + [
                    "CLADEFS_BENCH_PGBENCH_SCALE=50",
                    "CLADEFS_BENCH_WATCHDOG_SECS=900",
                    "CLADEFS_FOLLOW_SECONDS=600",
                    "CLADEFS_TEST_HOST_RUN=r1040",
                    "CLADEFS_PJDFSTEST_TESTS=chmod chown",
                    "CLADEFS_BUCKET_LATENCY_MS=25",
                    "CLADEFS_ZSTD_LEVEL=3",
                    "PJDFSTEST_COMMIT=85a8aea9",
                ]),
                COMMAND + ["fxfs-guest fxfs-guest-dax", "a"],
            ),
        )

    def test_this_machines_own_setup_stays_here(self):
        here = {name: "/laptop/" + name for name in KEPT}
        # Knobs for the host: where its stores keep their branches, and its daemons what they read
        # from them. And a path of this machine's build that the host scripts set themselves.
        host = {
            "CLADEFSD_BUCKET": "s3://bucket/prefix",
            "CLADEFS_S3_URL": "s3://bucket/test",
            "CLADEFSD_PART_CACHE": "/cache",
            "CLADEFS_FIRECRACKER": "/laptop/firecracker",
        }
        self.assertEqual(
            self.host_command({**here, **host}, "test-linux-host.sh")[1],
            sorted(BASE + [
                "CLADEFSD_PART_CACHE=/cache",
                "CLADEFSD_BUCKET=s3://bucket/prefix",
                "CLADEFS_FIRECRACKER=/laptop/firecracker",
                "CLADEFS_S3_URL=s3://bucket/test",
            ]),
        )

    def test_every_variable_kept_here_is_one_the_code_reads(self):
        # A name the code no longer reads, as after a rename, would keep nothing here, and send
        # the new name, which may name this machine's own setup, to the hosts.
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
        self.assertEqual(self.host_command(empty, "test-linux-host.sh")[1:], (sorted(BASE), COMMAND))

    def test_values_arrive_as_they_are(self):
        note = "a 'quoted' \"note\": $HOME, `id`, \\back\\slashes\\, *?, ×\nand a line"
        self.assertEqual(
            self.host_command({"CLADEFS_BENCH_NOTE": note}, "test-linux-host.sh", note)[1:],
            (sorted(BASE + [f"CLADEFS_BENCH_NOTE={note}"]), COMMAND + [note]),
        )

    def test_the_guest_kernel_is_the_copy_on_the_host(self):
        self.assertEqual(
            self.host_command({"CLADEFS_GUEST_KERNEL": "/laptop/vmlinux"}, "test-linux-host.sh")[1],
            sorted(BASE + [f"CLADEFS_GUEST_KERNEL={DIR}/guest-kernel"]),
        )

    def test_a_path_in_home_is_in_the_hosts(self):
        self.assertEqual(
            self.host_command({"CLADEFSD_PART_CACHE": "~/cache"}, "test-linux-host.sh")[1],
            sorted(BASE + [f"CLADEFSD_PART_CACHE={HOST_HOME}/cache"]),
        )

    def test_the_callers_values_replace_this_machines(self):
        self.assertEqual(
            self.host_command(
                {"CLADEFSD_BUCKET": "s3://bucket/prefix", "CLADEFS_ZSTD_LEVEL": "3",
                 # The run's own, which no caller's replaces.
                 "CLADEFS_TEST_HOST_WORK": "/laptop/work"},
                "CLADEFSD_BUCKET=s3://bucket/prefix/run-x",
                "CLADEFS_MIGRATE_BUNDLE=~/run-x/bundle",
                # Empty, unset there.
                "CLADEFS_ZSTD_LEVEL=",
                "test-linux-host.sh",
                "CLADEFS_NOT=an argument",
            )[1:],
            (
                sorted(BASE + [
                    "CLADEFSD_BUCKET=s3://bucket/prefix/run-x",
                    f"CLADEFS_MIGRATE_BUNDLE={HOST_HOME}/run-x/bundle",
                ]),
                COMMAND + ["CLADEFS_NOT=an argument"],
            ),
        )

    def test_nothing_to_send_and_no_arguments(self):
        self.assertEqual(self.host_command({}, "test-linux-host.sh")[1:], (sorted(BASE), COMMAND))

    def test_the_builds_jobs_and_niceness_are_the_callers(self):
        nice, sent, _ = self.host_command(
            {"CLADEFS_TEST_HOST_JOBS": "2", "CLADEFS_TEST_HOST_NICE": "5", "CLADEFS_GUEST_KERNEL_JOBS": "3"},
            "test-linux-host.sh")
        self.assertEqual(nice, ["nice", "-n", "5"])
        self.assertIn("CARGO_BUILD_JOBS=2", sent)
        self.assertIn("NEXTEST_TEST_THREADS=2", sent)
        # The kernel's jobs the caller's, as it named them.
        self.assertIn("CLADEFS_GUEST_KERNEL_JOBS=3", sent)
        self.assertNotIn("CLADEFS_GUEST_KERNEL_JOBS=2", sent)

    def test_a_measurement_runs_at_its_own_niceness(self):
        # Its builds' niceness is its script's (bench-ab-linux-host.sh's CLADEFS_AB_BUILD_NICE).
        self.assertEqual(
            self.host_command({}, "test-linux-host.sh", preamble="host_bench=1; ")[0], [])

    def test_the_limit_is_the_callers(self):
        self.assertEqual(
            self.host_command({"CLADEFS_TEST_HOST_LIMIT": "30m"}, "test-linux-host.sh")[2][:4],
            ["timeout", "--signal=TERM", "--kill-after=30s", "30m"])


# A host script, as the tests' hosts have every one of them: says each job of it, its phase
# (CLADEFS_TEST_HOST_PHASE, all if none) and whether it ran as work or a measurement, a line of
# RESULTS/jobs.txt; builds, as the host scripts do, unless it measures, which a measurement refuses
# (scripts/lib/measurement.sh, TASK-368); and, but for a build alone, says how it was run, its
# arguments and the variables it was given, in RESULTS/ran.txt, and does as CLADEFS_FAKE_HOST says:
# fail; block until it is ended (signal.pause); or a phase of a migration, the source's leaving its
# snapshot in the bundle, the target's taking it from there. CLADEFS_FAKE_HOST=fail-build fails its
# build.
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
    # The build fills the scratch disk to CLADEFS_FAKE_FILL KiB free (fake_ssh.py's df).
    if [[ -n ${{CLADEFS_FAKE_FILL:-}} ]]; then echo "$CLADEFS_FAKE_FILL" > "$HOME/df-free"; fi
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
    wait) exec {shlex.quote(sys.executable)} -c 'import signal; signal.pause()' ;;
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
           "soak-host.sh", "build-commit.sh", "lib/host.sh", "lib/host-remote.sh",
           "lib/measurement.sh"]
# Copied, not linked: they find the tree they keep their results in by where they are.
COPIED = ["bench-history.py", "bench_results.py"]
IDENTITY = {"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t",
            "GIT_COMMITTER_EMAIL": "t@t", "GIT_CONFIG_COUNT": "1",
            "GIT_CONFIG_KEY_0": "protocol.file.allow", "GIT_CONFIG_VALUE_0": "always"}


class Fleet:
    """Hosts of fake_ssh.py's, as `state` (fake_ssh.py) has them, in a temporary directory: an
    origin, a checkout of cladefs whose vendor/firecracker is a submodule, its scripts the drivers
    and FAKE_HOST_SCRIPT; ubu1, a host whose home's src/fxdax is a clone of it; and the scratch
    directory runs are made in on the hosts (CLADEFS_TEST_HOST_SCRATCH)."""

    def __init__(self, state=None):
        # As git names it, its links resolved (macOS's /var); removed with the fleet.
        self.directory = Path(tempfile.mkdtemp()).resolve()
        weakref.finalize(self, shutil.rmtree, self.directory, True)
        (self.directory / "state.json").write_text(json.dumps(state or {}))
        (self.directory / "calls").write_text("")
        (self.directory / "bin").mkdir()
        ssh = self.directory / "bin" / "ssh"
        ssh.write_text(
            f'#!/bin/sh\nexec {shlex.quote(sys.executable)} {shlex.quote(str(FAKE_SSH))} "$@"\n')
        ssh.chmod(0o755)
        # A driver waits a while between tries to reach a host: here not at all.
        (self.directory / "bin" / "sleep").write_text("#!/bin/sh\n")
        (self.directory / "bin" / "sleep").chmod(0o755)
        self.scratch = self.directory / "scratch"
        self.environment = {
            "PATH": f"{self.directory / 'bin'}:/usr/bin:/bin",
            "HOME": str(self.directory),
            "FAKE_SSH": str(self.directory),
            "CLADEFS_TEST_HOST_SCRATCH": str(self.scratch),
            **IDENTITY,
        }
        self.firecracker = self.commit(self.directory / "firecracker", {"README": "firecracker\n"})
        self.origin = self.directory / "origin"
        files = {"README": "cladefs\n", ".gitignore": "/docs/measurements/\n"}
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
        self.clone = self.home("ubu1") / "src" / "fxdax"
        self.git(self.directory, "clone", "-q", str(self.origin), str(self.clone))

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

    def home(self, host):
        home = self.directory / "hosts" / host / "home"
        home.mkdir(parents=True, exist_ok=True)
        return home

    def state(self):
        return json.loads((self.directory / "state.json").read_text())

    def calls(self):
        """The calls of ssh, each its host and the first word of what it ran there, and its
        action, if it was host-remote.sh's."""
        said = []
        for call in (self.directory / "calls").read_text().splitlines():
            host, *words = call.split(" ")
            said.append(" ".join([host, *words[:4]] if words[:3] == ["bash", "-s", "--"]
                                 else [host, words[0]]))
        return said

    def runs(self):
        """The runs' directories on the hosts."""
        return sorted(path.name for path in self.scratch.glob("run-*")) if self.scratch.exists() else []

    def worktrees(self):
        """The host's clone's worktrees, but its own."""
        listed = self.git(self.clone, "worktree", "list", "--porcelain")
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
    """Each run's name, its directory on the host and its job's (host_run)."""

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
        # Apart whatever second each started in: by their marks.
        self.assertNotEqual(first.rsplit("-", 1)[1], second.rsplit("-", 1)[1])

    def test_a_run_is_named_for_its_number_and_once(self):
        fleet = Fleet()
        status, output, errors = fleet.drive('echo "$host_run"; echo "$host_dir"',
                                             environment={"CLADEFS_TEST_HOST_RUN": "r1040"})
        self.assertEqual((status, errors), (0, ""))
        run, directory = output.splitlines()
        self.assertRegex(run, r"^run-r1040-\d{8}T\d{6}Z-[0-9a-f]{8}$")
        self.assertEqual(directory, f"{fleet.scratch}/{run}")

    def test_a_host_and_a_run_number_are_named_as_they_may_be(self):
        fleet = Fleet()
        self.assertEqual(fleet.drive("true", environment={"CLADEFS_TEST_HOST": "a host"}),
                         (2, "", "CLADEFS_TEST_HOST names a host, as ssh does: a host\n"))
        self.assertEqual(
            fleet.drive("true", environment={"CLADEFS_TEST_HOST_RUN": "R1"}),
            (2, "", "CLADEFS_TEST_HOST_RUN names a run in lower-case letters and digits, such as r1040\n"))


class Runs(unittest.TestCase):
    """A run's life: its directory on the host, a worktree of its commit, which its job runs in,
    removed whatever happens."""

    def test_a_run_runs_its_commit_and_leaves_nothing(self):
        fleet = Fleet()
        status, out, errors = fleet.drive(RAN)
        self.assertEqual((status, out, errors), (0, "status 0\n", ""))
        results = fleet.directory / "results"
        ran = (results / "ran.txt").read_text().splitlines()
        run = (results / "run.txt").read_text().splitlines()[0].removeprefix("run ")
        self.assertEqual(ran[:2], ["test-linux-host.sh a b", f"source {fleet.scratch}/{run}/tree"])
        self.assertEqual((results / "remote.log").read_text(), "ran test-linux-host.sh a b\n")
        # Its directory gone, and its worktree with it.
        self.assertEqual((fleet.runs(), fleet.worktrees()), ([], []))
        self.assertEqual(
            fleet.calls(),
            ["ubu1 bash -s -- prepare", "ubu1 mkdir", "ubu1 bash -s -- launch",
             "ubu1 bash -s -- wait", "ubu1 bash -s -- log", "ubu1 bash -s -- fetch",
             "ubu1 bash -s -- clean"],
        )

    def test_a_local_host_runs_here_without_ssh(self):
        """CLADEFS_TEST_HOST=local: the run's host half runs on this machine, in its home, as the
        nightly's on ubu1 does from cron (scripts/nightly-host.sh, TASK-441)."""
        fleet = Fleet()
        tools = fleet.directory / "local-bin"
        fake_ssh.tools(tools)
        status, out, errors = fleet.drive(
            RAN + 'host_label\n',
            environment={"CLADEFS_TEST_HOST": "local", "CLADEFS_TEST_HOST_REPO": str(fleet.clone),
                         "PATH": f"{tools}:{fleet.environment['PATH']}"})
        hostname = subprocess.run(["hostname", "-s"], capture_output=True, text=True).stdout
        self.assertEqual((status, out, errors), (0, f"status 0\n{hostname}", ""))
        results = fleet.directory / "results"
        run = (results / "run.txt").read_text().splitlines()[0].removeprefix("run ")
        self.assertEqual((results / "ran.txt").read_text().splitlines()[:2],
                         ["test-linux-host.sh a b", f"source {fleet.scratch}/{run}/tree"])
        self.assertEqual((fleet.directory / "fxjob-calls").read_text(), f"--name {run} --mem 12G -- "
                         f"bash {fleet.scratch}/{run}/job/command\n")
        self.assertEqual((fleet.calls(), fleet.runs(), fleet.worktrees()), ([], [], []))

    def test_a_run_whose_job_fails_leaves_nothing(self):
        fleet = Fleet()
        status, out, _ = fleet.drive(RAN, environment={"CLADEFS_FAKE_HOST": "fail"})
        self.assertEqual((status, out), (0, "status 3\n"))
        self.assertEqual((fleet.directory / "results" / "remote.log").read_text(),
                         "ran test-linux-host.sh a b\n")
        self.assertTrue((fleet.directory / "results" / "ran.txt").exists())
        self.assertEqual((fleet.runs(), fleet.worktrees()), ([], []))

    def test_a_driver_that_fails_leaves_nothing(self):
        fleet = Fleet()
        status, _, _ = fleet.drive("host_start\nfalse\necho unreached")
        self.assertEqual(status, 1)
        self.assertEqual((fleet.runs(), fleet.worktrees()), ([], []))

    def test_a_commit_origin_lacks_runs_nothing(self):
        fleet = Fleet()
        # A commit here, not pushed.
        local = fleet.directory / "local"
        fleet.git(fleet.directory, "clone", "-q", str(fleet.origin), str(local))
        unpushed = fleet.commit(local, {"README": "changed\n"})
        status, out, errors = fleet.drive(RAN, preamble=f"repo={shlex.quote(str(local))}")
        self.assertEqual((status, out), (1, ""))
        self.assertEqual(errors, f"{unpushed} is not on origin: push it first\n")
        self.assertEqual((fleet.runs(), fleet.worktrees()), ([], []))

    def test_a_commit_pushed_since_the_hosts_clone_is_fetched(self):
        fleet = Fleet()
        pushed = fleet.commit(fleet.origin, {"README": "newer\n"})
        self.assertEqual(fleet.drive(RAN)[:2], (0, "status 0\n"))
        self.assertEqual((fleet.directory / "results" / "source.txt").read_text().splitlines()[0],
                         f"revision {pushed}")

    def test_a_tree_that_changes_what_it_tracks_runs_nothing(self):
        fleet = Fleet()
        (fleet.origin / "README").write_text("changed, not committed\n")
        self.assertEqual(
            fleet.drive(RAN),
            (2, "", f"{fleet.origin} changes what it tracks, which would not run: commit and push "
                    "it, or name a commit with CLADEFS_TEST_HOST_COMMIT.\n"))
        self.assertEqual(fleet.calls(), [])
        # Unless the commit is named: then that runs.
        older = fleet.head()
        status, out, _ = fleet.drive(RAN, environment={"CLADEFS_TEST_HOST_COMMIT": older})
        self.assertEqual((status, out), (0, "status 0\n"))

    def test_a_named_commit_is_the_one_run(self):
        fleet = Fleet()
        older = fleet.head()
        fleet.commit(fleet.origin, {"README": "newer\n"})
        self.assertEqual(fleet.drive(RAN, environment={"CLADEFS_TEST_HOST_COMMIT": "HEAD~"})[:2],
                         (0, "status 0\n"))
        self.assertEqual((fleet.directory / "results" / "source.txt").read_text().splitlines()[0],
                         f"revision {older}")
        self.assertEqual(fleet.drive(RAN, environment={"CLADEFS_TEST_HOST_COMMIT": "nowhere"}),
                         (2, "", f"nowhere: no commit of {fleet.origin}'s\n"))

    def test_two_runs_at_once_keep_apart(self):
        # The first stopped after its start, its trap not run, as a driver killed: the second's
        # run and end leave it alone.
        fleet = Fleet()
        status, first, _ = fleet.drive('host_start; echo "$host_run"; trap - EXIT')
        self.assertEqual(status, 0)
        self.assertEqual(fleet.drive(RAN)[:2], (0, "status 0\n"))
        self.assertEqual(fleet.runs(), [first.strip()])
        self.assertEqual(fleet.worktrees(), [f"{fleet.scratch}/{first.strip()}/tree"])

    def test_the_runs_of_three_days_ago_go_but_a_running_one(self):
        fleet = Fleet()
        fleet.scratch.mkdir()
        old, running = (fleet.scratch / f"run-2026010{n}T000000Z-0000000{n}" for n in (1, 2))
        for run in (old, running):
            (run / "job").mkdir(parents=True)
            (run / "job" / "lock").touch()
        # The running one's job holds its lock.
        hold(self, running / "job" / "lock")
        for run in (old, running):
            os.utime(run, (0, 0))
        status, out, errors = fleet.drive(RAN)
        self.assertEqual((status, out, errors), (0, "status 0\n", f"Removing {old}, of three days ago.\n"))
        self.assertEqual(fleet.runs(), [running.name])

    def test_a_host_that_does_not_answer_runs_nothing(self):
        fleet = Fleet({"down": ["ubu1"]})
        status, out, errors = fleet.drive(RAN)
        # Its end failed too, which fails it.
        self.assertEqual((status, out), (1, ""))
        self.assertEqual(errors.splitlines()[0], "ssh: connect to host ubu1 port 22: Connection refused")
        # Its end tried too.
        self.assertEqual(fleet.calls(), ["ubu1 bash -s -- prepare", "ubu1 bash -s -- clean"])
        self.assertIn(f"Removing {fleet.scratch}/", errors.splitlines()[-1])

    def test_a_job_runs_under_fxjob_and_a_measurement_alone_after_its_build_as_work(self):
        """TASK-368: a measurement's run is two jobs, its build as work, in work's memory, then
        the measurement alone under fxjob --bench, in its own, building nothing; any other run's
        one job of work, of no phase."""
        for bench, said, jobs in [
            ("0", ["--name RUN --mem 12G -- bash"], ["test-linux-host.sh all work"]),
            ("1", ["--name RUN --mem 12G -- bash", "--name RUN --bench --mem 16G -- bash"],
             ["test-linux-host.sh build work", "test-linux-host.sh measure bench"]),
        ]:
            with self.subTest(bench=bench):
                fleet = Fleet()
                memory = {} if bench == "0" else {"CLADEFS_TEST_HOST_MEM": "16G"}
                status, out, errors = fleet.drive(
                    RAN, environment={"CLADEFS_TEST_HOST_BENCH": bench, **memory})
                self.assertEqual((status, out), (0, "status 0\n"), errors)
                results = fleet.directory / "results"
                run = (results / "run.txt").read_text().split()[1]
                calls = (fleet.home("ubu1") / "fxjob-calls").read_text()
                self.assertEqual(calls.replace(run, "RUN").splitlines(),
                                 [f"{call} {fleet.scratch}/RUN/job/command" for call in said])
                self.assertEqual((results / "jobs.txt").read_text().splitlines(), jobs)
                # The build's log apart, the measurement's the run's own.
                self.assertEqual((results / "remote-build.log").exists(), bench == "1")
                self.assertEqual((results / "remote.log").read_text(), "ran test-linux-host.sh a b\n")

    def test_a_measurement_whose_build_failed_measures_nothing(self):
        fleet = Fleet()
        status, out, errors = fleet.drive(
            RAN, environment={"CLADEFS_TEST_HOST_BENCH": "1", "CLADEFS_FAKE_HOST": "fail-build"})
        self.assertEqual((status, out), (0, "status 4\n"), errors)
        results = fleet.directory / "results"
        self.assertEqual((results / "jobs.txt").read_text().splitlines(),
                         ["test-linux-host.sh build work"])
        self.assertNotIn("--bench", (fleet.home("ubu1") / "fxjob-calls").read_text())
        self.assertEqual(fleet.runs(), [])

    def test_a_measurement_that_would_build_is_refused(self):
        """The guard (scripts/lib/measurement.sh): a driver that runs its host script whole as one
        measurement, its builds in it, as before TASK-368, is refused, saying why, at once."""
        fleet = Fleet()
        status, out, errors = fleet.drive(
            "host_start\nstatus=0\n"
            'host_launch 12G test-linux-host.sh a b > "$results/remote.log" 2>&1 || status=$?\n'
            'if [[ $host_state == ended ]]; then host_fetch || status=$?; fi\necho "status $status"\n',
            environment={"CLADEFS_TEST_HOST_BENCH": "1"})
        self.assertEqual((status, out), (0, "status 2\n"), errors)
        results = fleet.directory / "results"
        self.assertEqual((results / "jobs.txt").read_text().splitlines(),
                         ["test-linux-host.sh all bench"])
        self.assertFalse((results / "ran.txt").exists())
        refusal = (results / "remote.log").read_text()
        self.assertRegex(refusal, r"^test-linux-host.sh's build would build inside a measurement "
                                  r"\(fxjob --bench, 0::/user.slice/user-1000.slice/user@1000.service/"
                                  r"fxdax.slice/fxdax-bench.slice/fxjob-\d+.scope\), holding its "
                                  r"cores and memory from every other measurement")
        self.assertTrue(refusal.endswith("(TASK-368)\n"), refusal)


GIB = 1 << 30


def hold(test, lock):
    """Holds `lock`, a file, as a running job holds its run's, until `test` ends."""
    holder = subprocess.Popen(
        [sys.executable, "-c", "import fcntl, sys; f = open(sys.argv[1]); "
         "fcntl.flock(f, fcntl.LOCK_EX); print('held', flush=True); sys.stdin.read()", lock],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    test.addCleanup(holder.stdout.close)
    test.addCleanup(holder.wait)
    test.addCleanup(holder.stdin.close)
    test.assertEqual(holder.stdout.readline(), "held\n")


class Room(unittest.TestCase):
    """TASK-489: a run that names the room it needs on the scratch disk, which every job on the
    host shares, checks it is there before it builds (CLADEFS_TEST_HOST_ROOM) and before its
    measurement (CLADEFS_TEST_HOST_ROOM_MEASURE), each check a line of RESULTS/room.txt; and its
    earlier runs of the same name go first (CLADEFS_TEST_HOST_SWEEP)."""

    def test_a_run_short_of_room_prepares_nothing(self):
        fleet = Fleet()
        (fleet.home("ubu1") / "df-free").write_text(f"{GIB >> 10}\n")
        status, out, errors = fleet.drive(RAN, environment={"CLADEFS_TEST_HOST_ROOM": str(2 * GIB)})
        self.assertEqual((status, out, errors), (
            28, "", "Not enough room: the scratch disk /mnt/fake-scratch has 1073741824 bytes free "
                    "before the run builds, which needs 2147483648 (CLADEFS_TEST_HOST_ROOM).\n"))
        self.assertEqual((fleet.directory / "results" / "room.txt").read_text(),
                         "build 1073741824 2147483648 short /mnt/fake-scratch\n")
        self.assertEqual((fleet.calls(), fleet.runs()), (["ubu1 bash -s -- room"], []))

    def test_a_measurement_whose_build_filled_the_disk_measures_nothing(self):
        """The first night on ubu1: its sides built, the disk was full as its measurement began,
        which could not write even its status."""
        for fill, status_, jobs, measured in [
            (str(4 * GIB >> 10), "status 0", ["test-linux-host.sh build work",
                                              "test-linux-host.sh measure bench"],
             "measure 4294967296 2147483648 ok"),
            ("1024", "status 28", ["test-linux-host.sh build work"],
             "measure 1048576 2147483648 short"),
        ]:
            with self.subTest(fill=fill):
                fleet = Fleet()
                (fleet.home("ubu1") / "df-free").write_text(f"{8 * GIB >> 10}\n")
                (fleet.home("ubu1") / "du-size").write_text("3\n")
                status, out, errors = fleet.drive(RAN, environment={
                    "CLADEFS_TEST_HOST_BENCH": "1", "CLADEFS_TEST_HOST_ROOM": str(2 * GIB),
                    "CLADEFS_TEST_HOST_ROOM_MEASURE": str(2 * GIB), "CLADEFS_FAKE_FILL": fill})
                self.assertEqual((status, out, errors), (0, f"{status_}\n", ""))
                results = fleet.directory / "results"
                self.assertEqual((results / "jobs.txt").read_text().splitlines(), jobs)
                self.assertEqual((results / "room.txt").read_text().splitlines(), [
                    "build 8589934592 2147483648 ok /mnt/fake-scratch", "built 3072",
                    f"{measured} /mnt/fake-scratch"])
                self.assertEqual(fleet.runs(), [])
        self.assertEqual((results / "remote.log").read_text(),
                         "Not enough room: the scratch disk /mnt/fake-scratch has 1048576 bytes "
                         "free before the run measures, which needs 2147483648 "
                         "(CLADEFS_TEST_HOST_ROOM_MEASURE).\n")

    def test_a_run_names_bytes(self):
        fleet = Fleet()
        self.assertEqual(fleet.drive(RAN, environment={"CLADEFS_TEST_HOST_ROOM": "100G"}),
                         (2, "", "CLADEFS_TEST_HOST_ROOM is a count of bytes: 100G\n"))

    def test_the_earlier_runs_of_its_name_go_but_a_running_one_and_others(self):
        fleet = Fleet()
        fleet.scratch.mkdir()
        ended, running = (fleet.scratch / f"run-nightly-2026111{n}T070003Z-{n}1111111"
                          for n in (5, 4))
        others = [fleet.scratch / name for name in (
            "run-t435-20261115T070003Z-22222222", "run-nightlyx-20261115T070003Z-33333333",
            "run-20261115T070003Z-44444444", "run-nightly-old", "kernels")]
        for run in (ended, running, *others):
            (run / "job").mkdir(parents=True)
            (run / "job" / "lock").touch()
            (run / "trees" / "head" / "target").mkdir(parents=True)
        (ended / "job" / "status").write_text("0\n")
        hold(self, running / "job" / "lock")
        status, out, errors = fleet.drive(
            RAN, environment={"CLADEFS_TEST_HOST_RUN": "nightly", "CLADEFS_TEST_HOST_SWEEP": "1"})
        self.assertEqual((status, out, errors), (
            0, "status 0\n", f"Keeping {running}: its job runs.\n"
                             f"Removing {ended}, an earlier run of nightly's.\n"))
        self.assertEqual(fleet.runs(), [
            "run-20261115T070003Z-44444444", running.name, "run-nightly-old",
            "run-nightlyx-20261115T070003Z-33333333", "run-t435-20261115T070003Z-22222222"])
        self.assertTrue((fleet.scratch / "kernels").is_dir())

    def test_only_a_named_run_sweeps(self):
        fleet = Fleet()
        self.assertEqual(fleet.drive(RAN, environment={"CLADEFS_TEST_HOST_SWEEP": "1"}),
                         (2, "", "CLADEFS_TEST_HOST_SWEEP sweeps the runs of "
                                 "CLADEFS_TEST_HOST_RUN's name: name them\n"))


class Source(unittest.TestCase):
    """host_start: the run's worktrees, what they hold, and their records here."""

    def test_the_commit_is_run_and_recorded(self):
        fleet = Fleet()
        self.assertEqual(fleet.drive(RAN)[:2], (0, "status 0\n"))
        results = fleet.directory / "results"
        self.assertEqual(
            (results / "source.txt").read_text().splitlines(),
            [f"revision {fleet.head()}", f"firecracker {fleet.firecracker}"])
        run, host, directory = (results / "run.txt").read_text().splitlines()
        self.assertRegex(run, f"^run {NAME}$")
        self.assertEqual((host, directory), ("host ubu1", f"directory {fleet.scratch}/{run[4:]}"))

    def worktree(self, script, environment=None):
        """Runs `script` after host_start, which leaves the run on the host: its status, and
        the run's directory there."""
        fleet = Fleet()
        status, out, errors = fleet.drive(f'host_start\n{script}\necho "$host_dir"\ntrap - EXIT',
                                          environment=environment)
        self.assertEqual((status, errors), (0, ""))
        return fleet, Path(out.splitlines()[-1])

    def test_a_worktree_has_firecracker_and_reports_its_commit(self):
        fleet, directory = self.worktree("")
        tree = directory / "tree"
        self.assertEqual((tree / "vendor" / "firecracker" / "README").read_text(), "firecracker\n")
        # The commit its builds report, as the host scripts' copies, which have no .git, do.
        self.assertEqual((tree / "source-revision.txt").read_text(), f"{fleet.head()}\n")
        self.assertEqual(sorted(path.name for path in directory.iterdir()),
                         ["job", "results", "tree", "trees", "work"])

    def test_a_patch_is_applied_and_recorded(self):
        patch = Path(tempfile.mkdtemp()) / "mutant.patch"
        patch.write_text("--- a/README\n+++ b/README\n@@ -1 +1 @@\n-cladefs\n+mutated\n")
        fleet, directory = self.worktree("", {"CLADEFS_TEST_HOST_PATCH": str(patch)})
        tree = directory / "tree"
        self.assertEqual((tree / "README").read_text(), "mutated\n")
        self.assertEqual((tree / "source-revision.txt").read_text(), f"{fleet.head()}-dirty\n")
        digest = subprocess.run(["shasum", "-a", "256", patch], capture_output=True, text=True,
                                check=True).stdout.split()[0]
        self.assertEqual(
            (fleet.directory / "results" / "source.txt").read_text().splitlines()[2],
            f"patch {digest} mutant.patch")

    def test_a_guest_kernel_is_copied_and_recorded(self):
        kernel = Path(tempfile.mkdtemp()) / "vmlinux"
        kernel.write_bytes(b"a kernel")
        fleet, directory = self.worktree("", {"CLADEFS_GUEST_KERNEL": str(kernel)})
        self.assertEqual((directory / "guest-kernel").read_bytes(), b"a kernel")
        digest = subprocess.run(["shasum", "-a", "256", kernel], capture_output=True, text=True,
                                check=True).stdout.split()[0]
        self.assertEqual(
            (fleet.directory / "results" / "source.txt").read_text().splitlines()[2],
            f"guest kernel {digest}")

    def test_other_commits_have_worktrees_of_their_own(self):
        fleet = Fleet()
        older = fleet.head()
        newer = fleet.commit(fleet.origin, {"README": "newer\n"})
        status, out, errors = fleet.drive(
            'host_start base=HEAD~ same=HEAD; echo "$host_dir"; trap - EXIT')
        self.assertEqual((status, errors), (0, ""))
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

    def test_a_firecracker_the_host_cannot_get_runs_nothing(self):
        fleet = Fleet()
        # vendor/firecracker at a commit its origin doesn't have.
        missing = "0123456789abcdef0123456789abcdef01234567"
        fleet.git(fleet.origin, "update-index", "--cacheinfo", f"160000,{missing},vendor/firecracker")
        fleet.git(fleet.origin, "commit", "-q", "-m", "a firecracker of nowhere")
        # The origin's own checkout of it the earlier one, which the commit names.
        status, out, errors = fleet.drive(RAN, environment={"CLADEFS_TEST_HOST_COMMIT": "HEAD"})
        # git's status.
        self.assertEqual((status, out), (128, ""))
        self.assertIn(missing, errors)
        self.assertEqual((fleet.runs(), fleet.worktrees()), ([], []))

    def test_a_copy_that_failed_runs_nothing(self):
        kernel = Path(tempfile.mkdtemp()) / "vmlinux"
        kernel.write_bytes(b"a kernel")
        fleet = Fleet({"calls": [{"fail": 1}]})
        status, out, _ = fleet.drive(RAN, environment={"CLADEFS_GUEST_KERNEL": str(kernel)})
        self.assertEqual((status, out), (1, ""))
        self.assertEqual(fleet.calls(), ["ubu1 mkdir", "ubu1 bash -s -- clean"])


class Jobs(unittest.TestCase):
    """What runs on a host, under a driver's `|| status=$?`."""

    def started(self, state, preamble="", environment=None):
        """Runs a run's job, after host_start and `preamble`, on hosts as `state` has them from
        the call after host_start's: its status, output (the job's, its status, `kept` if its pid
        was kept on the host, and where the run stands) and errors, and the fleet."""
        fleet = Fleet()
        status, out, errors = fleet.drive(
            f"host_start\n{preamble}\n"
            f"echo {shlex.quote(json.dumps(state))} > \"$FAKE_SSH/state.json\"\n"
            'status=0; host_job test-linux-host.sh || status=$?; echo "status $status"\n'
            'if [[ -e $host_dir/job/pid ]]; then echo kept; fi; echo "$host_state"',
            environment=environment)
        return status, out, errors, fleet

    def test_a_job_that_did_not_start_is_not_waited_for(self):
        # Its launch fails, and a fresh connection's look for its pid finds none.
        status, out, errors, fleet = self.started(
            {"calls": [None, {"fail": 255}, {"fail": 1}]},
            # Waiting would take until the host answered no more.
            preamble='sleep() { echo "waited for the job" >&2; exit 3; }')
        self.assertEqual((status, out, errors), (0, "status 255\nprepared\n", ""))
        self.assertEqual(fleet.calls()[-3:], ["ubu1 bash -s -- launch", "ubu1 test",
                                              "ubu1 bash -s -- clean"])

    def test_a_job_keeps_its_pid_and_is_waited_for(self):
        status, out, _, _ = self.started({}, environment={"CLADEFS_FAKE_HOST": "fail"})
        self.assertEqual((status, out), (0, "ran test-linux-host.sh \nstatus 3\nkept\nended\n"))

    def test_a_job_whose_launching_connection_dropped_is_waited_for(self):
        # TASK-235: the launch's SSH fails once the job runs, as GCE r942's did: a fresh
        # connection finds its pid, and it is waited for.
        status, out, _, fleet = self.started({"calls": [None, {"status": 255}]})
        self.assertEqual((status, out), (0, "ran test-linux-host.sh \nstatus 0\nkept\nended\n"))
        self.assertEqual(fleet.calls()[2:5], ["ubu1 bash -s -- launch", "ubu1 test",
                                              "ubu1 bash -s -- wait"])

    def test_a_dropped_wait_is_tried_again(self):
        status, out, _, fleet = self.started({"calls": [None, None, {"fail": 255}, {"fail": 255}]})
        self.assertEqual((status, out), (0, "ran test-linux-host.sh \nstatus 0\nkept\nended\n"))
        self.assertEqual(fleet.calls().count("ubu1 bash -s -- wait"), 3)

    def test_a_host_gone_quiet_is_waited_for_no_longer_and_its_run_left(self):
        # Every try fails: the job may run on, and its run's directory stays for a resume.
        status, out, errors, fleet = self.started({"calls": [None, None] + [{"fail": 255}] * 20})
        self.assertEqual((status, out), (0, "status 1\nkept\nlaunched\n"))
        self.assertEqual(fleet.calls().count("ubu1 bash -s -- wait"), 20)
        run = fleet.runs()[0]
        self.assertEqual(errors, f"ubu1 does not answer: {run}'s job may run on.\n"
                                 f"{run} runs on, on ubu1: wait for it and fetch its results with "
                                 f"`/bin/bash resume {run} {fleet.directory}/results`, or end it "
                                 f"with `/bin/bash cancel {run}`.\n")
        self.assertEqual(fleet.worktrees(), [f"{fleet.scratch}/{run}/tree"])

    def fetch(self, state):
        """Runs host_fetch on hosts as `state` has them, the run's results holding a file, and a
        root filesystem, which isn't results: its status, its errors, and the files then in the
        results."""
        fleet = Fleet()
        status, out, errors = fleet.drive(
            'host_start\n'
            'echo results > "$host_dir/results/host.txt"\n'
            'mkdir -p "$host_dir/results/e2e/rootfs"; echo no > "$host_dir/results/e2e/rootfs/sh"\n'
            f"echo {shlex.quote(json.dumps(state))} > \"$FAKE_SSH/state.json\"\n"
            'status=0; host_fetch || status=$?; echo "status $status"')
        results = fleet.directory / "results"
        return out, errors, sorted(str(path.relative_to(results)) for path in results.rglob("*"))

    def test_results_are_fetched_but_root_filesystems(self):
        self.assertEqual(self.fetch({}), ("status 0\n", "", ["e2e", "host.txt", "remote.log",
                                                             "run.txt", "source.txt"]))

    def test_results_are_fetched_from_an_archive_tar_failed_to_make_whole(self):
        # tar ends with 1 when a file changed as it read it, as the logs of a daemon a killed host
        # script left running do, and with 2 when it could not read one: it archived every other.
        self.assertEqual(self.fetch({"calls": [{"status": 2}]}),
                         ("status 2\n", "", ["e2e", "host.txt", "remote.log", "run.txt",
                                              "source.txt"]))

    def test_a_fetch_that_failed_fails(self):
        out, _, files = self.fetch({"calls": [{"fail": 255}]})
        self.assertEqual((out, files), ("status 255\n", ["remote.log", "run.txt", "source.txt"]))


class Resume(unittest.TestCase):
    """A driver's resume and cancel (host_actions), of a run whose driver stopped waiting for its
    job."""

    def left(self, environment):
        """A run left on the host as a driver killed leaves it, its job launched with
        `environment`: the fleet and the run's name."""
        fleet = Fleet()
        status, out, _ = fleet.drive(
            'host_start\nhost_put <(printf "%s\\n" "$(host_command test-linux-host.sh)") '
            '"$host_dir/job/command"\n'
            'host_side launch "$host_dir" "$host_run" 0 ""\necho "$host_run"\ntrap - EXIT',
            environment=environment)
        self.assertEqual(status, 0)
        return fleet, out.strip()

    def test_a_run_whose_job_ended_is_resumed_and_ended(self):
        fleet, run = self.left({})
        results = fleet.directory / "resumed"
        status, out, errors = fleet.run_driver("test-host.sh", "resume", run, str(results))
        self.assertEqual((status, out), (0, ""))
        self.assertEqual(errors, f"Results: {results}\nran test-linux-host.sh \n"
                                 f"Results: {results}\n")
        self.assertEqual((results / "ran.txt").read_text().splitlines()[0], "test-linux-host.sh ")
        self.assertEqual((fleet.runs(), fleet.worktrees()), ([], []))

    def test_a_job_whose_shell_went_without_its_status_is_lost(self):
        fleet, run = self.left({})
        directory = fleet.scratch / run
        # Its status gone, as a job's is whose host restarted under it.
        fleet.drive(f'host_name {run}; host_side wait "$host_dir"')
        (directory / "job" / "status").unlink()
        results = fleet.directory / "resumed"
        status, _, errors = fleet.run_driver("test-host.sh", "resume", run, str(results))
        self.assertEqual(status, 1)
        self.assertIn(f"{run} on ubu1 is lost: its job ended without its status.\n",
                      (results / "remote.log").read_text())
        self.assertEqual(fleet.runs(), [])

    def test_a_job_whose_status_is_empty_is_lost(self):
        """TASK-489: on a full disk the job's shell made its status file and could write nothing in
        it; the wait read `status ` and bash's return refused the empty number."""
        fleet, run = self.left({})
        directory = fleet.scratch / run
        fleet.drive(f'host_name {run}; host_side wait "$host_dir"')
        for written in ("", "no space\n"):
            with self.subTest(status=written):
                (directory / "job" / "status").write_text(written)
                status, out, errors = fleet.drive(
                    f'host_name {run}; host_state=launched; status=0; '
                    'host_wait > /dev/null || status=$?; echo "status $status $host_state"')
                self.assertEqual((status, out, errors), (
                    0, "status 1 ended\n",
                    f"{run} on ubu1 is lost: its job ended without its status (its status file "
                    "empty or unreadable: was its disk full?).\n"))

    def test_a_running_job_is_cancelled(self):
        fleet, run = self.left({"CLADEFS_FAKE_HOST": "wait"})
        status, out, errors = fleet.run_driver("test-host.sh", "cancel", run)
        self.assertEqual((status, out, errors), (0, "", f"Ended {run} on ubu1.\n"))
        self.assertEqual((fleet.runs(), fleet.worktrees()), ([], []))

    def test_a_running_jobs_run_is_not_removed(self):
        fleet, run = self.left({"CLADEFS_FAKE_HOST": "wait"})
        status, _, errors = fleet.drive(f'host_name {run}; host_side clean "$host_dir" "$host_repo"')
        self.assertEqual((status, errors), (1, f"clean: a job runs in {fleet.scratch}/{run}\n"))
        self.assertEqual(fleet.run_driver("test-host.sh", "cancel", run)[0], 0)

    def test_only_a_run_is_resumed(self):
        fleet = Fleet()
        self.assertEqual(fleet.run_driver("test-host.sh", "resume", "cladefs-test-x", "results"),
                         (2, "", "Name the run, run-...\n"))
        self.assertEqual(fleet.run_driver("test-host.sh", "resume", "run-x"),
                         (2, "", f"Usage: {fleet.origin}/scripts/test-host.sh resume RUN RESULTS\n"))


class Reap(unittest.TestCase):
    """host-remote.sh's reap: what is left of a job's scope once it ends, root's processes among
    it, which the user's systemd manager can't kill. Here processes of this machine's, and a
    directory of processes (CLADEFS_TEST_HOST_PROC) whose cgroup files place them, a process's entry
    gone as sudo's kill kills it, as /proc's goes."""

    SUDO = """#!/bin/bash
[[ $1 == kill ]] || exit 3
for pid in "${@:3}"; do rm -r "$CLADEFS_TEST_HOST_PROC/$pid"; done
exec "$@"
"""

    def test_what_is_left_of_a_jobs_scope_is_killed_and_no_other(self):
        directory = Path(tempfile.mkdtemp())
        (directory / "bin").mkdir()
        (directory / "bin" / "sudo").write_text(self.SUDO)
        (directory / "bin" / "sudo").chmod(0o755)
        processes = {}
        for scope in ["fxjob-run-x-41.scope", "fxjob-run-x-41.scope", "fxjob-run-xy-7.scope"]:
            process = subprocess.Popen([sys.executable, "-c", "import signal; signal.pause()"])
            self.addCleanup(process.wait)
            self.addCleanup(process.kill)
            processes[process] = scope
            (directory / "proc" / str(process.pid)).mkdir(parents=True)
            (directory / "proc" / str(process.pid) / "cgroup").write_text(
                f"0::/user.slice/user-1000.slice/user@1000.service/fxdax.slice/{scope}\n")
        done = subprocess.run(
            ["/bin/bash", SCRIPTS / "lib" / "host-remote.sh", "reap", "run-x"],
            env={"PATH": f"{directory / 'bin'}:/usr/bin:/bin",
                 "CLADEFS_TEST_HOST_PROC": str(directory / "proc")},
            capture_output=True, text=True)
        self.assertEqual((done.returncode, done.stderr), (0, ""))
        for process, scope in processes.items():
            if scope == "fxjob-run-x-41.scope":
                self.assertEqual(process.wait(timeout=10), -9)
            else:
                self.assertIsNone(process.poll())


class Names(unittest.TestCase):
    """The drivers' results: two runs at once keep theirs apart, each's directory ending in its
    run's mark, here on a host that doesn't answer."""

    # Each driver's results directory, before the time, and its own settings.
    DRIVERS = {
        "bench-host.sh": ("ubu1-fs-bench", {}),
        "bench-ab-host.sh": ("ubu1-fs-bench-ab", {"CLADEFS_AB_A": "a=HEAD", "CLADEFS_AB_B": "b=same"}),
        "gets-host.sh": ("ubu1-gets", {"CLADEFS_GETS_URL": "file:///tmp/gets/"}),
        "mutants-host.sh": ("ubu1-mutants", {}),
        "gc-sweep-host.sh": ("ubu1-gc-sweep", {}),
    }

    def test_runs_at_once_have_names_of_their_own(self):
        for driver, (results, environment) in self.DRIVERS.items():
            with self.subTest(driver):
                fleet = Fleet({"down": ["ubu1"]})
                for _ in range(2):
                    self.assertEqual(fleet.run_driver(driver, environment=environment)[0], 1)
                kept = sorted((fleet.origin / "docs" / "measurements").glob(f"{results}-2*"))
                marks = {
                    re.fullmatch(rf"{results}-\d{{4}}-\d{{2}}-\d{{2}}T\d{{4}}Z-([0-9a-f]{{8}})",
                                 path.name).group(1)
                    for path in kept
                }
                self.assertEqual(len(marks), 2, kept)


class Drivers(unittest.TestCase):
    """Each driver's run, end to end, on a host of fake_ssh.py's."""

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

    def test_a_bench_is_a_measurement_kept_in_the_history(self):
        fleet = Fleet()
        results = fleet.directory / "origin" / "docs" / "measurements" / "ubu1-fs-bench-x"
        status, _, errors = fleet.run_driver("bench-host.sh", str(results),
                                             environment={"CLADEFS_BENCH_NOTE": "a note"})
        self.assertEqual(status, 0, errors)
        self.assertEqual((results / "ran.txt").read_text().splitlines()[0], "bench-linux-host.sh 3")
        self.assertIn("--bench", (fleet.home("ubu1") / "fxjob-calls").read_text())
        self.assertEqual((results / "note.md").read_text().splitlines()[0], "a note")
        history = (fleet.origin / "docs" / "measurements" / "fs-bench-history.md").read_text()
        self.assertIn("[x](ubu1-fs-bench-x/)", history)

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
        trees = f"{fleet.scratch}/{run}/trees"
        self.assertEqual((results / "ran.txt").read_text().splitlines()[0],
                         f"bench-ab-linux-host.sh {trees}/base {trees}/change 2 base change")
        self.assertEqual((results / "base" / "source.txt").read_text().splitlines()[0],
                         f"revision {base}")
        # Its sides built as work, then its rounds alone measured, jobs of one run.
        self.assertIn("CLADEFS_TEST_HOST_PHASE=measure", (results / "ran.txt").read_text().splitlines())
        self.assertEqual((results / "jobs.txt").read_text().splitlines(),
                         ["bench-ab-linux-host.sh build work", "bench-ab-linux-host.sh measure bench"])
        calls = (fleet.home("ubu1") / "fxjob-calls").read_text().splitlines()
        self.assertEqual([("--bench" in call, f"--name {run}" in call) for call in calls],
                         [(False, True), (True, True)])
        self.assertTrue((results / "remote-build.log").exists())
        # B the same tree: A's, built once.
        status, _, errors = fleet.run_driver(
            "bench-ab-host.sh", str(fleet.directory / "same"),
            environment={"CLADEFS_AB_A": "base=HEAD", "CLADEFS_AB_B": "knob=same"})
        self.assertEqual(status, 0, errors)
        self.assertRegex((fleet.directory / "same" / "ran.txt").read_text().splitlines()[0],
                         r"^bench-ab-linux-host.sh \S+/trees/base same 3 base knob$")

    def test_every_timed_driver_builds_as_work_then_measures_alone(self):
        """TASK-368: no driver's fxjob --bench job builds. Each measures in two jobs of its run, its
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
                # Where bench-host.sh's history finds it.
                results = fleet.origin / "docs" / "measurements" / "ubu1-x"
                args = [str(results) if arg == "RESULTS" else arg for arg in args]
                status, _, errors = fleet.run_driver(driver, *args, environment=environment)
                self.assertEqual(status, 0, errors)
                for out, script in outs.items():
                    self.assertEqual((results / out / "jobs.txt").read_text().splitlines(),
                                     [f"{script} build work", f"{script} measure bench"], out)
                calls = (fleet.home("ubu1") / "fxjob-calls").read_text().splitlines()
                self.assertEqual(["--bench" in call for call in calls], [False, True] * len(outs))
                self.assertEqual(fleet.runs(), [])

    def test_a_driver_that_is_no_measurement_runs_one_job_of_work(self):
        fleet = Fleet()
        results = fleet.directory / "soaked"
        status, _, errors = fleet.run_driver("soak-host.sh", str(results), "1024:2:2048")
        self.assertEqual(status, 0, errors)
        self.assertEqual((results / "1024m-2c-2048w" / "jobs.txt").read_text().splitlines(),
                         ["test-linux-host.sh all work"])
        self.assertNotIn("--bench", (fleet.home("ubu1") / "fxjob-calls").read_text())

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
        trees = f"{fleet.scratch}/{run}/trees"
        self.assertEqual((results / "ran.txt").read_text().splitlines()[0],
                         f"fork-time-linux-host.sh main={trees}/main old={trees}/old")
        self.assertIn("--bench", (fleet.home("ubu1") / "fxjob-calls").read_text())
        self.assertEqual(fleet.run_driver("fork-time-host.sh", str(results))[0], 2)

    def test_gc_sweeps_are_a_measurement(self):
        fleet = Fleet()
        results = fleet.directory / "gc"
        status, _, errors = fleet.run_driver(
            "gc-sweep-host.sh", str(results), environment={"CLADEFS_GC_AT_ONCE": "1 8"})
        self.assertEqual(status, 0, errors)
        ran = (results / "ran.txt").read_text().splitlines()
        # The fake host script says its name, then its arguments: none.
        self.assertEqual(ran[0], "gc-sweep-linux-host.sh ")
        self.assertIn("CLADEFS_GC_AT_ONCE=1 8", ran)
        self.assertIn("--bench", (fleet.home("ubu1") / "fxjob-calls").read_text())
        self.assertEqual((results / "note.md").read_text().splitlines()[0],
                         "cladefs-gc's sweeps over a bucket of files of many cells (TASK-395.5).")
        self.assertEqual(fleet.run_driver("gc-sweep-host.sh", "a", "b")[0], 2)

    def test_gc_sweeps_of_sides_measure_each_commit_in_one_job(self):
        """TASK-395.17: an A/B of commits' sweeps, each side a worktree of its own, measured in the
        run's one job, by the run's scripts."""
        fleet = Fleet()
        results = fleet.directory / "gc-ab"
        status, _, errors = fleet.run_driver("gc-sweep-host.sh", str(results), "old=HEAD",
                                             "new=HEAD")
        self.assertEqual(status, 0, errors)
        run = (results / "run.txt").read_text().split()[1]
        trees = f"{fleet.scratch}/{run}/trees"
        self.assertEqual((results / "ran.txt").read_text().splitlines()[0],
                         f"gc-sweep-linux-host.sh old={trees}/old new={trees}/new")
        self.assertTrue((results / "old" / "source.txt").is_file())
        self.assertEqual((fleet.home("ubu1") / "fxjob-calls").read_text().count("--bench"), 1)
        # Sides come after the results, which a side can't stand for.
        self.assertEqual(fleet.run_driver("gc-sweep-host.sh", "old=HEAD")[0], 2)
        self.assertEqual(fleet.run_driver("gc-sweep-host.sh", str(results), "old")[0], 2)

    def test_a_migration_restores_on_the_second_run_what_the_first_left(self):
        fleet = Fleet()
        results = fleet.directory / "migrated"
        status, _, errors = fleet.run_driver("migrate-host.sh", str(results),
                                             environment={"CLADEFS_FAKE_HOST": "migrate"})
        self.assertEqual(status, 0, errors)
        a, b = ((results / side / "ran.txt").read_text().splitlines() for side in "ab")
        run = (results / "a" / "run.txt").read_text().split()[1]
        held = f"{fleet.scratch}/{run}.held"
        # The same paths on both, the second's run of the same name.
        self.assertEqual(a[1], b[1])
        for side, phase, token in [(a, "source", "1"), (b, "target", "2")]:
            self.assertIn(f"CLADEFS_MIGRATE_PHASE={phase}", side)
            self.assertIn(f"CLADEFSD_TOKEN={token}", side)
            self.assertIn(f"CLADEFSD_BUCKET=file://{held}/bucket/branches", side)
            self.assertIn(f"CLADEFS_MIGRATE_BUNDLE={held}/bundle", side)
        self.assertEqual((results / "b" / "remote.log").read_text(),
                         "ran test-linux-host.sh fxfs-guest-migrate-dax\n"
                         f"a VM, from {fleet.scratch}/{run}/tree\n")
        # Nothing left of it.
        self.assertEqual((list(fleet.scratch.iterdir()), fleet.worktrees()), ([], []))
        self.assertEqual(errors.splitlines()[-1], f"Results: {results}")

    def test_a_migration_that_fails_leaves_nothing(self):
        fleet = Fleet()
        results = fleet.directory / "migrated"
        status, _, errors = fleet.run_driver("migrate-host.sh", str(results),
                                             environment={"CLADEFS_FAKE_HOST": "fail"})
        self.assertEqual(status, 3)
        self.assertFalse((results / "b").exists())
        self.assertEqual((list(fleet.scratch.iterdir()), fleet.worktrees()), ([], []))
        self.assertEqual(errors.splitlines()[-1], f"Results: {results}")

    def test_a_migration_between_two_hosts_names_a_bucket(self):
        fleet = Fleet()
        self.assertEqual(
            fleet.run_driver("migrate-host.sh", environment={"CLADEFS_MIGRATE_HOSTS": "a b"}),
            (2, "", "Name a bucket both hosts reach: CLADEFS_MIGRATE_BUCKET.\n"))


if __name__ == "__main__":
    unittest.main()
