# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of the nightly (TASK-441.1, TASK-441.2), end to end: scripts/nightly-host.sh as cron runs
it, in the nightly's clone of an origin, on this machine as its local test host
(scripts/lib/host.sh's CLADEFS_TEST_HOST=local), with fake_ssh.py's host tools (fxjob, which says
whether a job measured, sudo, setsid, timeout, flock), a backlog CLI of its own (FAKE_BACKLOG) and a host script in the
bench-ab-linux-host.sh's place (FAKE_AB) whose rounds are fixtures shaped as ubu1's
(test_ab_rounds.write_round). And scripts/nightly-install.sh's cron entry."""

import json
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest
import weakref
from pathlib import Path

import fake_ssh
from script import SCRIPTS
from test_ab_rounds import CONTROL, write_round
from test_host import hold

# The host script of the A/B, as the nightly's tests have it: says each job of it, its phase, where
# it ran (work, or a measurement) and the sides' revisions, a line of RESULTS/jobs.txt; builds the
# sides, or fails the one CLADEFS_FAKE_NIGHTLY=fail-build-SIDE names, as the real one does
# (RESULTS/build-failed), and fills the scratch disk to CLADEFS_FAKE_FILL KiB free if set
# (fake_ssh.py's df); measures by copying the rounds CLADEFS_FAKE_ROUNDS holds, and says the
# rounds' results held CLADEFS_FAKE_ROUNDS_SIZE bytes (RESULTS/rounds-size.txt), if set.
FAKE_AB = """#!/bin/bash
set -euo pipefail
tools=$1 results=$2 a_source=$3 b_source=$4
source "$tools/scripts/lib/measurement.sh"
phase=${CLADEFS_TEST_HOST_PHASE:-all} where=work
if measuring; then where=bench; fi
echo "$phase $where $(sed -n 1p "$a_source/source-revision.txt") $(sed -n 1p "$b_source/source-revision.txt")" >> "$results/jobs.txt"
if [[ $phase == build ]]; then
    for side in a b; do
        if [[ ${CLADEFS_FAKE_NIGHTLY:-} == fail-build-$side ]]; then
            echo "$side" > "$results/build-failed"
            exit 1
        fi
        echo "built" > "$results/artifacts-$side.txt"
    done
    if [[ -n ${CLADEFS_FAKE_FILL:-} ]]; then echo "$CLADEFS_FAKE_FILL" > "$HOME/df-free"; fi
    exit 0
fi
cp -R "$CLADEFS_FAKE_ROUNDS/." "$results/"
if [[ -n ${CLADEFS_FAKE_ROUNDS_SIZE:-} ]]; then
    echo "$CLADEFS_FAKE_ROUNDS_SIZE" > "$results/rounds-size.txt"
fi
"""

# Backlog.md's CLI, as the nightly calls it, its tasks in $BACKLOG_STATE: `task list --labels L
# --json`, `task create TITLE -d D -l LABELS --priority P --plain` (a file of the task in
# backlog/tasks, as the real one writes), `task edit ID --append-notes NOTE`.
FAKE_BACKLOG = r"""
import json, os, sys
from pathlib import Path
state = Path(os.environ["BACKLOG_STATE"])
tasks = json.loads(state.read_text()) if state.exists() else []
args = sys.argv[1:]
def option(name):
    return args[args.index(name) + 1]
def write(task):
    path = Path("backlog/tasks") / f"{task['id'].lower()} - nightly.md"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(task, indent=1))
if args[:2] == ["task", "list"]:
    label = option("--labels")
    print(json.dumps({"schemaVersion": 1, "kind": "task-list",
                      "tasks": [t for t in tasks if label in t["labels"]]}))
elif args[:2] == ["task", "create"]:
    task = {"id": f"TASK-{900 + len(tasks)}", "title": args[2], "status": "To Do",
            "labels": option("-l").split(","), "priority": option("--priority"),
            "description": option("-d"), "notes": []}
    tasks.append(task)
    write(task)
    print(f"File: backlog/tasks/x.md\n\nTask {task['id']} - {task['title']}\n=====")
elif args[:2] == ["task", "edit"]:
    task = next(t for t in tasks if t["id"] == args[2])
    task["notes"].append(option("--append-notes"))
    write(task)
    print(f"Task {task['id']} - {task['title']}")
else:
    sys.exit(f"backlog: no such call: {args}")
state.write_text(json.dumps(tasks))
"""

# The scripts the nightly runs, linked into the origin; the host script of the A/B is FAKE_AB.
LINKED = ["nightly-host.sh", "nightly.py", "ab-rounds.py", "bench-ab.py", "contamination.py",
          "bench-ab-host.sh", "build-commit.sh", "lib/host.sh", "lib/host-remote.sh",
          "lib/measurement.sh"]
IDENTITY = {"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t",
            "GIT_COMMITTER_EMAIL": "t@t", "GIT_CONFIG_COUNT": "1",
            "GIT_CONFIG_KEY_0": "protocol.file.allow", "GIT_CONFIG_VALUE_0": "always"}
BASE_IOPS = [6077.151, 6100.0]


class Ubu1:
    """This machine as ubu1, in a temporary directory: a bare origin of cladefs whose scripts are
    the nightly's and FAKE_AB; a home whose src/cladefs-nightly is the nightly's clone of it; and
    the host's tools."""

    def __init__(self):
        self.dir = Path(tempfile.mkdtemp()).resolve()
        weakref.finalize(self, shutil.rmtree, self.dir, True)
        self.home = self.dir / "home"
        self.home.mkdir()
        bin_ = self.dir / "bin"
        fake_ssh.tools(bin_)
        (bin_ / "fake_backlog.py").write_text(FAKE_BACKLOG)
        (bin_ / "backlog").write_text(
            f'#!/bin/sh\nexec {shlex.quote(sys.executable)} "{bin_}/fake_backlog.py" "$@"\n')
        (bin_ / "backlog").chmod(0o755)
        self.state = self.home / ".local" / "state" / "cladefs" / "nightly"
        # The scratch disk as fake_ssh.py's df has it, 250 GiB free, as ubu1's whole: room for a
        # night, whatever this machine's disk has.
        (self.home / "df-free").write_text(f"{250 << 20}\n")
        self.backlog = self.dir / "backlog.json"
        self.environment = {
            "PATH": f"{bin_}:/usr/bin:/bin:/usr/sbin:/sbin",
            "HOME": str(self.home),
            "CLADEFS_TEST_HOST_SCRATCH": str(self.dir / "scratch"),
            "CLADEFS_NIGHTLY_KEEP": str(self.dir / "keep"),
            "BACKLOG_STATE": str(self.backlog),
            **IDENTITY,
        }
        firecracker = self.dir / "firecracker"
        self.commit(firecracker, {"README": "firecracker\n"})
        seed = self.dir / "seed"
        files = {"README": "cladefs\n", "scripts/lib/bench-ab-linux-host.sh": FAKE_AB,
                 "docs/measurements/README.md": "measurements\n"}
        self.commit(seed, files)
        for name in LINKED:
            (seed / "scripts" / name).symlink_to(SCRIPTS / name)
        self.git(seed, "submodule", "add", "-q", str(firecracker), "vendor/firecracker")
        self.git(seed, "add", ".")
        self.git(seed, "commit", "-q", "-m", "cladefs")
        self.origin = self.dir / "origin.git"
        self.git(self.dir, "clone", "-q", "--bare", str(seed), str(self.origin))
        self.clone = self.home / "src" / "cladefs-nightly"
        self.git(self.dir, "clone", "-q", str(self.origin), str(self.clone))
        self.pusher = self.dir / "pusher"
        self.git(self.dir, "clone", "-q", str(self.origin), str(self.pusher))

    def git(self, tree, *args):
        return subprocess.run(["git", "-C", str(tree), *args], env=self.environment,
                              capture_output=True, text=True, check=True).stdout.strip()

    def commit(self, tree, files):
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

    def advance(self, name="change"):
        """Lands a commit on origin's main, as another's push does: its SHA."""
        self.git(self.pusher, "pull", "-q", "--rebase", "origin", "main")
        sha = self.commit(self.pusher, {f"{name}.txt": f"{name}\n"})
        self.git(self.pusher, "push", "-q", "origin", "HEAD:main")
        return sha

    def main(self):
        return self.git(self.origin, "rev-parse", "main")

    def rounds(self, factor, verdicts=None):
        """Three rounds of both sides, the head's fxfs-guest-dax fsync-append the base's times
        `factor`, the control steady: their directory."""
        directory = Path(tempfile.mkdtemp(dir=self.dir))
        for number in (1, 2, 3):
            write_round(directory, "base", number, {"ext4-guest-kernel": CONTROL,
                                                    "fxfs-guest-dax": {"fsync-append": BASE_IOPS}})
            write_round(directory, "head", number,
                        {"ext4-guest-kernel": CONTROL,
                         "fxfs-guest-dax": {"fsync-append": [v * factor for v in BASE_IOPS]}},
                        verdict=(verdicts or {}).get(number, "clean"))
        return directory

    def night(self, rounds=None, **environment):
        """A night, as cron runs it: its status, output and errors."""
        if rounds is not None:
            environment["CLADEFS_FAKE_ROUNDS"] = str(rounds)
        done = subprocess.run(["/bin/bash", str(self.clone / "scripts" / "nightly-host.sh")],
                              capture_output=True, text=True, cwd=self.home,
                              env={**self.environment, **environment})
        return done.returncode, done.stdout, done.stderr

    def tasks(self):
        return json.loads(self.backlog.read_text()) if self.backlog.exists() else []

    def nights(self):
        """The nights' records on origin's main, oldest first."""
        listed = self.git(self.origin, "ls-tree", "-r", "--name-only", "main", "docs/measurements")
        found = [json.loads(self.git(self.origin, "show", f"main:{path}"))
                 for path in listed.splitlines() if path.endswith("/night.json")]
        return sorted(found, key=lambda record: record["date"])

    def history(self):
        return self.git(self.origin, "show", "main:docs/measurements/nightly-history.md")

    def log(self):
        return (self.state / "nightly.log").read_text()

    def jobs(self, record):
        """What the host script's jobs said of the night `record`."""
        listed = self.git(self.origin, "ls-tree", "-r", "--name-only", "main", "docs/measurements")
        path = next(path for path in listed.splitlines()
                    if path.endswith(f"-{record['head'][:8]}/jobs.txt"))
        return self.git(self.origin, "show", f"main:{path}").splitlines()


class Nights(unittest.TestCase):
    def test_a_night_main_did_not_move_does_nothing(self):
        ubu1 = Ubu1()
        ubu1.advance()
        status, _, err = ubu1.night(ubu1.rounds(1.0))
        self.assertEqual(status, 0, err)
        before = ubu1.main()
        calls = (ubu1.home / "fxjob-calls").read_text()
        status, _, err = ubu1.night(ubu1.rounds(1.0))
        self.assertEqual(status, 0, err)
        self.assertEqual(ubu1.main(), before)
        self.assertEqual((ubu1.home / "fxjob-calls").read_text(), calls)
        [record] = ubu1.nights()
        self.assertEqual(ubu1.log().splitlines()[-1].split(" ", 1)[1],
                         f"nothing to measure: main's code is {record['head'][:12]}'s, which the "
                         f"night of {record['date']} measured")
        # Nor does a night after a commit of the backlog's alone.
        ubu1.git(ubu1.pusher, "pull", "-q", "--rebase", "origin", "main")
        ubu1.commit(ubu1.pusher, {"backlog/tasks/task-1 - x.md": "a task\n"})
        ubu1.git(ubu1.pusher, "push", "-q", "origin", "HEAD:main")
        before = ubu1.main()
        status, _, err = ubu1.night(ubu1.rounds(1.0))
        self.assertEqual((status, ubu1.main()), (0, before), err)

    def test_a_quiet_night_builds_as_work_measures_alone_and_files_nothing(self):
        ubu1 = Ubu1()
        parent = ubu1.main()
        head = ubu1.advance()
        status, _, err = ubu1.night(ubu1.rounds(1.0))
        self.assertEqual(status, 0, err)
        [record] = ubu1.nights()
        self.assertEqual((record["base"], record["head"], record["outcome"], record["moved"],
                          record["tasks"]), (parent, head, "measured", [], []))
        self.assertEqual(ubu1.jobs(record), [f"build work {parent} {head}",
                                             f"measure bench {parent} {head}"])
        self.assertEqual(ubu1.tasks(), [])
        self.assertEqual(ubu1.git(ubu1.origin, "log", "-1", "--format=%s", "main"),
                         f"nightly {record['date'][:10]}: {parent[:8]}..{head[:8]}, nothing moved")

    def test_a_metric_that_moved_is_one_task_and_on_later_nights_a_note(self):
        ubu1 = Ubu1()
        parent = ubu1.main()
        first = ubu1.advance("first")
        status, _, err = ubu1.night(ubu1.rounds(0.8))
        self.assertEqual(status, 0, err)
        [task] = ubu1.tasks()
        marker = "[nightly:fxfs-guest-dax fsync-append slower]"
        self.assertEqual(task["title"], f"Nightly: fxfs-guest-dax fsync-append 25% slower on main, "
                                        f"{parent[:8]}..{first[:8]} {marker}")
        self.assertEqual((task["labels"], task["notes"]), (["perf", "nightly"], []))
        [record] = ubu1.nights()
        where = f"docs/measurements/{record_directory(ubu1, record)}/nightly.md"
        self.assertIn(f"- Base, the commit the night before measured: {parent}\n"
                      f"- Head, main: {first}\n"
                      "- Each round, base against head: r1: base 6,089, head 4,871 (1.250×); "
                      "r2: base 6,089, head 4,871 (1.250×); r3: base 6,089, head 4,871 (1.250×)\n"
                      f"- Results: {where}\n", task["description"])
        self.assertIn(f"    CLADEFS_AB_A=base={parent} CLADEFS_AB_B=head={first} "
                      "CLADEFS_AB_ROUNDS=3 CLADEFS_AB_RERUN=1 CLADEFS_BENCH_CASES='bench-fs-ext4-"
                      "guest-kernel bench-fs-fxfs-guest-dax bench-fork-fxfs-guest-dax-cold "
                      "bench-fork-fxfs-guest-dax-warm' CLADEFS_BENCH_GROUPS='io,files,fsync,sync' "
                      "scripts/bench-ab-host.sh RESULTS && scripts/ab-rounds.py compare RESULTS "
                      "base head\n", task["description"])
        # The commit holds the results, the history page and the task.
        changed = ubu1.git(ubu1.origin, "show", "--name-only", "--format=", "main").splitlines()
        self.assertIn("docs/measurements/nightly-history.md", changed)
        self.assertIn("backlog/tasks/task-900 - nightly.md", changed)
        self.assertIn(f"docs/measurements/{record_directory(ubu1, record)}/night.json", changed)
        self.assertEqual(record["tasks"], [{"key": "fxfs-guest-dax fsync-append slower",
                                            "id": "TASK-900", "action": "filed"}])

        second = ubu1.advance("second")
        status, _, err = ubu1.night(ubu1.rounds(0.8))
        self.assertEqual(status, 0, err)
        [task] = ubu1.tasks()
        _, record = ubu1.nights()
        self.assertEqual((record["base"], record["head"]), (first, second))
        self.assertEqual(task["notes"], [
            f"{record['date'][:10]}: again on {first[:8]}..{second[:8]}: r1: base 6,089, head 4,871 "
            "(1.250×); r2: base 6,089, head 4,871 (1.250×); r3: base 6,089, head 4,871 (1.250×) "
            f"(docs/measurements/{record_directory(ubu1, record)}/nightly.md)"])
        self.assertEqual(record["tasks"], [{"key": "fxfs-guest-dax fsync-append slower",
                                            "id": "TASK-900", "action": "noted"}])
        self.assertIn("| fxfs-guest-dax fsync-append 25% slower | TASK-900 (noted) |",
                      ubu1.history())

    def test_a_done_task_is_filed_again(self):
        ubu1 = Ubu1()
        ubu1.advance("first")
        ubu1.night(ubu1.rounds(0.8))
        tasks = ubu1.tasks()
        tasks[0]["status"] = "Done"
        ubu1.backlog.write_text(json.dumps(tasks))
        ubu1.advance("second")
        status, _, err = ubu1.night(ubu1.rounds(0.8))
        self.assertEqual(status, 0, err)
        self.assertEqual([task["id"] for task in ubu1.tasks()], ["TASK-900", "TASK-901"])

    def test_three_nights_in_a_row_without_a_result_file_a_task(self):
        ubu1 = Ubu1()
        parent = ubu1.main()
        heads = []
        for night in range(3):
            heads.append(ubu1.advance(f"n{night}"))
            status, _, err = ubu1.night(ubu1.rounds(1.0), CLADEFS_FAKE_NIGHTLY="fail-build-b")
            self.assertEqual(status, 0, err)
            self.assertEqual(len(ubu1.tasks()), 0 if night < 2 else 1)
        records = ubu1.nights()
        self.assertEqual([(r["base"], r["head"], r["outcome"], r["why"]) for r in records],
                         [(parent, head, "could not run",
                           "head failed to build (see build-b.log and guest-kernel-b.log)")
                          for head in heads])
        [task] = ubu1.tasks()
        self.assertEqual(task["title"], "Nightly: 3 nights in a row measured nothing "
                                        "[nightly:no-result]")
        self.assertIn("**The last 3 nights measured nothing** (a task is filed).", ubu1.history())
        # A night that measured ends the streak.
        ubu1.advance("fixed")
        status, _, err = ubu1.night(ubu1.rounds(1.0))
        self.assertEqual(status, 0, err)
        self.assertEqual(ubu1.nights()[-1]["outcome"], "measured")
        self.assertNotIn("measured nothing**", ubu1.history())

    def test_a_night_whose_rounds_were_all_left_out_has_no_result(self):
        ubu1 = Ubu1()
        ubu1.advance()
        status, _, err = ubu1.night(ubu1.rounds(0.8, verdicts={1: "contaminated", 2: "contaminated",
                                                               3: "contaminated"}))
        self.assertEqual(status, 0, err)
        [record] = ubu1.nights()
        self.assertEqual((record["outcome"], record["why"], record["moved"]),
                         ("no result", "every round was left out (contaminated, or its control "
                                       "moved)", []))
        self.assertIn("**The last night measured nothing**: a task is filed at 3.",
                      ubu1.history())
        self.assertEqual(ubu1.tasks(), [])

    def test_a_night_that_measured_nothing_is_measured_again(self):
        """TASK-489: the first night on ubu1 built both sides and measured nothing; main did not
        move, and the next night did nothing, as the night had measured main's code."""
        ubu1 = Ubu1()
        parent = ubu1.main()
        first = ubu1.advance("first")
        left_out = {1: "contaminated", 2: "contaminated", 3: "contaminated"}
        for rounds, environment in [(None, {}), (ubu1.rounds(1.0, left_out), {}),
                                    (ubu1.rounds(1.0), {"CLADEFS_FAKE_NIGHTLY": "fail-build-b"}),
                                    (ubu1.rounds(1.0), {})]:
            status, _, err = ubu1.night(rounds, **environment)
            self.assertEqual(status, 0, err)
        self.assertEqual([(r["base"], r["head"], r["outcome"]) for r in ubu1.nights()],
                         [(parent, first, "could not run"), (parent, first, "no result"),
                          (parent, first, "could not run"), (parent, first, "measured")])
        # Measured, it is not measured again; main's next head is, against it.
        status, _, err = ubu1.night(ubu1.rounds(1.0))
        self.assertEqual((status, len(ubu1.nights())), (0, 4), err)
        second = ubu1.advance("second")
        ubu1.night(ubu1.rounds(1.0, left_out))
        third = ubu1.advance("third")
        ubu1.night(ubu1.rounds(1.0))
        self.assertEqual([(r["base"], r["head"], r["outcome"]) for r in ubu1.nights()[4:]],
                         [(first, second, "no result"), (first, third, "measured")])


GIB = 1 << 30


class Room(unittest.TestCase):
    """TASK-489: the scratch disk, which every job on ubu1 shares, full as the first night's
    measurement began."""

    def test_a_night_short_of_room_before_measuring_says_so(self):
        ubu1 = Ubu1()
        (ubu1.home / "df-free").write_text(f"{200 * GIB >> 10}\n")
        (ubu1.home / "du-size").write_text(f"{83 * GIB >> 10}\n")
        ubu1.advance()
        status, _, err = ubu1.night(ubu1.rounds(1.0), CLADEFS_FAKE_FILL=f"{5 * GIB >> 10}")
        self.assertEqual(status, 0, err)
        [record] = ubu1.nights()
        why = ("the scratch disk /mnt/fake-scratch had 5.0 GiB free before measuring, the night "
               "needs 20.0 GiB (see room.txt; CLADEFS_NIGHTLY_ROOM_MEASURE)")
        self.assertEqual((record["outcome"], record["why"], record["status"]),
                         ("could not run", why, 28))
        self.assertEqual(record["room"], {
            "checks": [{"phase": "build", "free": 200 * GIB, "need": 120 * GIB, "verdict": "ok",
                        "disk": "/mnt/fake-scratch"},
                       {"phase": "measure", "free": 5 * GIB, "need": 20 * GIB,
                        "verdict": "short", "disk": "/mnt/fake-scratch"}],
            "built": 83 * GIB, "rounds": None})
        self.assertEqual(ubu1.jobs(record), [f"build work {record['base']} {record['head']}"])
        directory = record_directory(ubu1, record)
        self.assertEqual(ubu1.git(ubu1.origin, "show",
                                  f"main:docs/measurements/{directory}/nightly.md").splitlines()[2:9],
                         [f"Base {record['base']}, head (main) {record['head']}: could not run: "
                          f"{why}.", "", "## The scratch disk", "",
                          "- Before building: 200.0 GiB free on /mnt/fake-scratch, 120.0 GiB "
                          "needed (ok).",
                          "- Before measuring: 5.0 GiB free on /mnt/fake-scratch, 20.0 GiB "
                          "needed (short).",
                          "- The run's directory once built: 83.0 GiB."])
        lines = [line.split(" ", 1)[1] for line in ubu1.log().splitlines()]
        self.assertIn(f"night {record['base'][:8]}..{record['head'][:8]}: could not run: {why}; "
                      "0 moved, 0 broke; tasks none", lines)

    def test_a_night_short_of_room_before_building_builds_nothing(self):
        ubu1 = Ubu1()
        (ubu1.home / "df-free").write_text(f"{100 * GIB >> 10}\n")
        ubu1.advance()
        status, _, err = ubu1.night(ubu1.rounds(1.0))
        self.assertEqual(status, 0, err)
        [record] = ubu1.nights()
        self.assertEqual((record["outcome"], record["why"]), (
            "could not run", "the scratch disk /mnt/fake-scratch had 100.0 GiB free before "
                             "building, the night needs 120.0 GiB (see room.txt; "
                             "CLADEFS_NIGHTLY_ROOM_BUILD)"))
        self.assertFalse((ubu1.home / "fxjob-calls").exists())

    def test_the_need_follows_what_earlier_nights_took(self):
        ubu1 = Ubu1()
        (ubu1.home / "df-free").write_text(f"{250 * GIB >> 10}\n")
        (ubu1.home / "du-size").write_text(f"{90 * GIB >> 10}\n")
        ubu1.advance("first")
        status, _, err = ubu1.night(ubu1.rounds(1.0), CLADEFS_FAKE_ROUNDS_SIZE=str(30 * GIB))
        self.assertEqual(status, 0, err)
        ubu1.advance("second")
        status, _, err = ubu1.night(ubu1.rounds(1.0))
        self.assertEqual(status, 0, err)
        first, second = ubu1.nights()
        self.assertEqual((first["room"]["built"], first["room"]["rounds"]), (90 * GIB, 30 * GIB))
        # A fifth more than the most taken: 108 GiB built and 36 measured.
        self.assertEqual([check["need"] for check in second["room"]["checks"]],
                         [144 * GIB, 36 * GIB])
        # Or the settings, if more.
        self.assertEqual(
            subprocess.run([sys.executable, str(SCRIPTS / "nightly.py"), "--root", str(ubu1.clone),
                            "room"], capture_output=True, text=True, check=True,
                           env={**ubu1.environment, "CLADEFS_NIGHTLY_ROOM_BUILD": "1T",
                                "CLADEFS_NIGHTLY_ROOM_MEASURE": "40G"}).stdout,
            f"{(1 << 40) + 40 * GIB} {40 * GIB}\n")

    def test_earlier_nights_runs_go_but_what_others_and_the_next_night_use(self):
        ubu1 = Ubu1()
        scratch = ubu1.dir / "scratch"
        ended = scratch / "run-nightly-20261116T070003Z-2ad571cf"
        running = scratch / "run-nightly-20261115T070003Z-11111111"
        others = [scratch / "run-t435-20261116T070003Z-22222222",
                  scratch / "run-nightlyx-20261116T070003Z-33333333"]
        for run in (ended, running, *others):
            (run / "job").mkdir(parents=True)
            (run / "job" / "lock").touch()
            (run / "trees" / "head" / "target" / "release").mkdir(parents=True)
        hold(self, running / "job" / "lock")
        ubu1.advance()
        status, _, err = ubu1.night(ubu1.rounds(1.0))
        self.assertEqual(status, 0, err)
        self.assertEqual(sorted(path.name for path in scratch.iterdir()),
                         sorted([running.name] + [path.name for path in others]))
        [record] = ubu1.nights()
        driver = ubu1.git(ubu1.origin, "show", f"main:docs/measurements/"
                                               f"{record_directory(ubu1, record)}/driver.log")
        self.assertEqual(driver.splitlines()[:2], [f"Keeping {running}: its job runs.",
                                                   f"Removing {ended}, an earlier run of "
                                                   "nightly's."])


def short(sha):
    return sha[:8]


def record_directory(ubu1, record):
    listed = ubu1.git(ubu1.origin, "ls-tree", "-r", "--name-only", "main", "docs/measurements")
    path = next(path for path in listed.splitlines()
                if path.endswith(f"-{record['head'][:8]}/night.json"))
    return path.split("/")[2]


# A pre-push hook that, the first time, lands another's commit on origin's main as the nightly
# pushes, so that its push is refused; then lets pushes go.
MOVES_MAIN = """#!/bin/bash
unset $(git rev-parse --local-env-vars)
if [[ ! -e {marker} ]]; then
    touch {marker}
    cd {pusher} && git pull -q --rebase origin main && echo raced > raced.txt && git add raced.txt \\
        && git commit -q -m raced && git push -q origin HEAD:main
fi
"""


class Pushes(unittest.TestCase):
    def hook(self, ubu1, text):
        hooks = ubu1.dir / "hooks"
        hooks.mkdir(exist_ok=True)
        (hooks / "pre-push").write_text(text)
        (hooks / "pre-push").chmod(0o755)
        ubu1.git(ubu1.clone, "config", "core.hooksPath", str(hooks))

    def test_a_push_refused_as_main_moved_is_rebased_and_pushed_again(self):
        ubu1 = Ubu1()
        ubu1.advance()
        self.hook(ubu1, MOVES_MAIN.format(marker=ubu1.dir / "raced", pusher=ubu1.pusher))
        status, _, err = ubu1.night(ubu1.rounds(1.0))
        self.assertEqual(status, 0, err)
        subjects = ubu1.git(ubu1.origin, "log", "--format=%s", "-2", "main").splitlines()
        self.assertEqual(subjects[1], "raced")
        self.assertRegex(subjects[0], r"^nightly \d{4}-\d{2}-\d{2}: [0-9a-f]{8}\.\.[0-9a-f]{8}, "
                                      r"nothing moved$")
        lines = [line.split(" ", 1)[1] for line in ubu1.log().splitlines()]
        self.assertTrue(lines[-2].startswith("push 1 failed: "), lines)
        self.assertEqual(lines[-1], "pushed to origin's main (try 2)")
        self.assertFalse((ubu1.state / "push-failed.txt").exists())

    def test_a_failed_push_stays_here_says_so_and_goes_the_next_night(self):
        ubu1 = Ubu1()
        first = ubu1.advance("first")
        self.hook(ubu1, "#!/bin/sh\necho no pushes tonight >&2\nexit 1\n")
        status, _, err = ubu1.night(ubu1.rounds(1.0))
        self.assertEqual(status, 1, err)
        self.assertEqual(ubu1.main(), first)
        failed = (ubu1.state / "push-failed.txt").read_text()
        local = ubu1.git(ubu1.clone, "rev-parse", "HEAD")
        self.assertIn(f"the nightly's push of {local} to origin's main failed: ", failed)
        self.assertIn("no pushes tonight", failed)
        self.assertIn("its results stay in the nightly's clone", failed)
        self.assertEqual(ubu1.log().count("failed: "), 3)

        ubu1.git(ubu1.clone, "config", "--unset", "core.hooksPath")
        second = ubu1.advance("second")
        status, _, err = ubu1.night(ubu1.rounds(1.0))
        self.assertEqual(status, 0, err)
        first_night, second_night = ubu1.nights()
        self.assertEqual((first_night["head"], second_night["base"], second_night["head"]),
                         (first, first, second))
        self.assertEqual(ubu1.git(ubu1.origin, "log", "--format=%s", "-3", "main").splitlines()[1:],
                         [f"nightly {first_night['date'][:10]}: {short(first_night['base'])}.."
                          f"{short(first)}, nothing moved", "a commit"])
        self.assertEqual(len(second_night["push"]), 1)
        self.assertIn(f"the nightly's push of {local} to origin's main failed", second_night["push"][0])
        self.assertIn("(pushed ", second_night["push"][0])
        self.assertFalse((ubu1.state / "push-failed.txt").exists())
        self.assertFalse((ubu1.state / "push-recovered.txt").exists())


class Install(unittest.TestCase):
    def test_the_cron_entry_is_one_at_two_and_replaced_not_added(self):
        ubu1 = Ubu1()
        bin_ = ubu1.dir / "bin"
        crontab = ubu1.dir / "crontab.txt"
        crontab.write_text("5 * * * * other job\n")
        (bin_ / "crontab").write_text(
            f'#!/bin/sh\nif [ "$1" = -l ]; then cat {crontab}; else cat > {crontab}.new && '
            f'mv {crontab}.new {crontab}; fi\n')
        for tool in ("fxjob", "cargo"):
            if not (bin_ / tool).exists():
                (bin_ / tool).write_text("#!/bin/sh\nexit 0\n")
                (bin_ / tool).chmod(0o755)
        (bin_ / "crontab").chmod(0o755)
        environment = {**ubu1.environment, "CLADEFS_NIGHTLY_CRONTAB": str(bin_ / "crontab"),
                       "CLADEFS_NIGHTLY_REPO": str(ubu1.dir / "installed")}
        for _ in range(2):
            done = subprocess.run(["/bin/bash", str(SCRIPTS / "nightly-install.sh"),
                                   str(ubu1.origin)], capture_output=True, text=True,
                                  env=environment)
            self.assertEqual(done.returncode, 0, done.stderr)
        state = ubu1.home / ".local" / "state" / "cladefs" / "nightly"
        self.assertEqual(crontab.read_text().splitlines(), [
            "5 * * * * other job",
            f"0 2 * * * PATH={bin_}:/usr/local/bin:/usr/bin:/bin "
            f"{ubu1.dir / 'installed'}/scripts/nightly-host.sh >> {state}/cron.log 2>&1 "
            "# cladefs nightly (scripts/nightly-install.sh)"])
        self.assertEqual(ubu1.git(ubu1.dir / "installed", "config", "user.name"), "cladefs nightly")

    def test_what_the_nightly_needs_is_checked_first(self):
        ubu1 = Ubu1()
        # A PATH of only what the script must find, git and hostname: /usr/bin holds flock on
        # Linux, not on macOS (TASK-441.4).
        path = ubu1.dir / "path"
        path.mkdir()
        for tool in ("git", "hostname"):
            (path / tool).symlink_to(shutil.which(tool))
        done = subprocess.run(["/bin/bash", str(SCRIPTS / "nightly-install.sh"), str(ubu1.origin)],
                              capture_output=True, text=True,
                              env={**ubu1.environment, "PATH": str(path)})
        self.assertEqual((done.returncode, done.stderr),
                         (1, "The nightly can't run here: missing fxjob flock backlog cargo\n"))


if __name__ == "__main__":
    unittest.main()
