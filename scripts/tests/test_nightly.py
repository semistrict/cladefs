# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of the nightly (TASK-441.1, TASK-441.2), end to end: scripts/nightly-host.sh as cron runs
it, in the nightly's clone of an origin, its runs and board calls in fake_hotbed.py
(scripts/lib/host.sh), with flock (fake_flock.py) and a host script in bench-ab-linux-host.sh's
place (FAKE_AB) whose rounds are fixtures shaped as the test host's (test_ab_rounds.write_round).
And scripts/nightly-install.sh's cron entry."""

import datetime
import json
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest
import weakref
from pathlib import Path

from script import SCRIPTS
from test_ab_rounds import CONTROL, write_round

# The host script of the A/B, as the nightly's tests have it: says each job of it, its phase, where
# it ran (work, or a measurement) and the sides' revisions, a line of RESULTS/jobs.txt; builds the
# sides, or fails the one CLADEFS_FAKE_NIGHTLY=fail-build-SIDE names, as the real one does
# (RESULTS/build-failed); measures by copying the rounds CLADEFS_FAKE_ROUNDS holds.
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
    exit 0
fi
cp -R "$CLADEFS_FAKE_ROUNDS/." "$results/"
"""

# The scripts the nightly runs, linked into the origin; the host script of the A/B is FAKE_AB.
LINKED = ["nightly-host.sh", "nightly.py", "ab-rounds.py", "bench-ab.py", "contamination.py",
          "bench-ab-host.sh", "build-commit.sh", "lib/host.sh", "lib/host-workspace.sh",
          "lib/measurement.sh"]
IDENTITY = {"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t",
            "GIT_COMMITTER_EMAIL": "t@t", "GIT_CONFIG_COUNT": "1",
            "GIT_CONFIG_KEY_0": "protocol.file.allow", "GIT_CONFIG_VALUE_0": "always"}
BASE_IOPS = [6077.151, 6100.0]


class NightlyHost:
    """This machine as the nightly host, in a temporary directory: a bare origin of cladefs whose scripts are
    the nightly's and FAKE_AB; a home whose src/cladefs-nightly is the nightly's clone of it; and
    the host's tools."""

    def __init__(self):
        self.dir = Path(tempfile.mkdtemp()).resolve()
        weakref.finalize(self, shutil.rmtree, self.dir, True)
        self.home = self.dir / "home"
        self.home.mkdir()
        bin_ = self.dir / "bin"
        bin_.mkdir()
        for name, script in [("flock", "fake_flock.py"), ("hotbed", "fake_hotbed.py")]:
            (bin_ / name).write_text(f'#!/bin/sh\nexec {shlex.quote(sys.executable)} '
                                     f'{shlex.quote(str(Path(__file__).with_name(script)))} "$@"\n')
            (bin_ / name).chmod(0o755)
        self.hotbed = self.dir / "hotbed"
        self.hotbed.mkdir()
        (self.hotbed / "calls").write_text("")
        self.state = self.home / ".local" / "state" / "cladefs" / "nightly"
        self.board = self.dir / "board.json"
        self.environment = {
            "PATH": f"{bin_}:/usr/bin:/bin:/usr/sbin:/sbin",
            "HOME": str(self.home),
            "FAKE_HOTBED": str(self.hotbed),
            "CLADEFS_HOTBED_WORK": str(self.hotbed / "work"),
            "BOARD_STATE": str(self.board),
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

    def measured(self):
        """The nights' measurements so far: hotbed's commands run as one (--bench)."""
        return [call for call in (self.hotbed / "calls").read_text().splitlines()
                if "--bench" in call]

    def tasks(self):
        return json.loads(self.board.read_text()) if self.board.exists() else []

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
        host1 = NightlyHost()
        host1.advance()
        status, _, err = host1.night(host1.rounds(1.0))
        self.assertEqual(status, 0, err)
        before = host1.main()
        calls = host1.measured()
        status, _, err = host1.night(host1.rounds(1.0))
        self.assertEqual(status, 0, err)
        self.assertEqual(host1.main(), before)
        self.assertEqual(host1.measured(), calls)
        [record] = host1.nights()
        self.assertEqual(host1.log().splitlines()[-1].split(" ", 1)[1],
                         f"nothing to measure: main's code is {record['head'][:12]}'s, which the "
                         f"night of {record['date']} measured")

    def test_a_quiet_night_builds_as_work_measures_alone_and_files_nothing(self):
        host1 = NightlyHost()
        parent = host1.main()
        head = host1.advance()
        status, _, err = host1.night(host1.rounds(1.0))
        self.assertEqual(status, 0, err)
        [record] = host1.nights()
        self.assertEqual((record["base"], record["head"], record["outcome"], record["moved"],
                          record["tasks"]), (parent, head, "measured", [], []))
        self.assertEqual(host1.jobs(record), [f"build work {parent} {head}",
                                             f"measure bench {parent} {head}"])
        self.assertEqual(host1.tasks(), [])
        self.assertEqual(host1.git(host1.origin, "log", "-1", "--format=%s", "main"),
                         f"nightly {record['date'][:10]}: {parent[:8]}..{head[:8]}, nothing moved")

    def test_a_metric_that_moved_is_one_task_and_on_later_nights_a_note(self):
        host1 = NightlyHost()
        parent = host1.main()
        first = host1.advance("first")
        status, _, err = host1.night(host1.rounds(0.8))
        self.assertEqual(status, 0, err)
        [task] = host1.tasks()
        marker = "[nightly:fxfs-guest-dax fsync-append slower]"
        self.assertEqual(task["text"].split("\n\n## Description\n\n")[0],
                         f"# Nightly: fxfs-guest-dax fsync-append 25% slower on main, "
                         f"{parent[:8]}..{first[:8]} {marker}")
        self.assertEqual((task["labels"], task["as"], task["replies"]),
                         ({"priority": "high", "tag.perf": "yes", "tag.nightly": "yes"}, "nightly", []))
        [record] = host1.nights()
        where = f"docs/measurements/{record_directory(host1, record)}/nightly.md"
        self.assertIn(f"- Base, the commit the night before measured: {parent}\n"
                      f"- Head, main: {first}\n"
                      "- Each round, base against head: r1: base 6,089, head 4,871 (1.250×); "
                      "r2: base 6,089, head 4,871 (1.250×); r3: base 6,089, head 4,871 (1.250×)\n"
                      f"- Results: {where}\n", task["text"])
        self.assertIn(f"    CLADEFS_AB_A=base={parent} CLADEFS_AB_B=head={first} "
                      "CLADEFS_AB_ROUNDS=3 CLADEFS_AB_RERUN=1 CLADEFS_BENCH_CASES='bench-fs-ext4-"
                      "guest-kernel bench-fs-fxfs-guest-dax bench-fork-fxfs-guest-dax-cold "
                      "bench-fork-fxfs-guest-dax-warm' CLADEFS_BENCH_GROUPS='io,files,fsync,sync' "
                      "scripts/bench-ab-host.sh RESULTS && scripts/ab-rounds.py compare RESULTS "
                      "base head\n", task["text"])
        # The commit holds the results and the history page; the task is on the board.
        changed = host1.git(host1.origin, "show", "--name-only", "--format=", "main").splitlines()
        self.assertIn("docs/measurements/nightly-history.md", changed)
        self.assertIn(f"docs/measurements/{record_directory(host1, record)}/night.json", changed)
        self.assertEqual(record["tasks"], [{"key": "fxfs-guest-dax fsync-append slower",
                                            "id": "board 900", "action": "filed"}])

        second = host1.advance("second")
        status, _, err = host1.night(host1.rounds(0.8))
        self.assertEqual(status, 0, err)
        [task] = host1.tasks()
        _, record = host1.nights()
        self.assertEqual((record["base"], record["head"]), (first, second))
        self.assertEqual(task["replies"], [{"as": "nightly", "text":
            f"{record['date'][:10]}: again on {first[:8]}..{second[:8]}: r1: base 6,089, head 4,871 "
            "(1.250×); r2: base 6,089, head 4,871 (1.250×); r3: base 6,089, head 4,871 (1.250×) "
            f"(docs/measurements/{record_directory(host1, record)}/nightly.md)"}])
        self.assertEqual(record["tasks"], [{"key": "fxfs-guest-dax fsync-append slower",
                                            "id": "board 900", "action": "noted"}])
        self.assertIn("| fxfs-guest-dax fsync-append 25% slower | board 900 (noted) |",
                      host1.history())

    def test_a_done_task_is_filed_again(self):
        host1 = NightlyHost()
        host1.advance("first")
        host1.night(host1.rounds(0.8))
        tasks = host1.tasks()
        tasks[0]["state"] = "completed"
        host1.board.write_text(json.dumps(tasks))
        host1.advance("second")
        status, _, err = host1.night(host1.rounds(0.8))
        self.assertEqual(status, 0, err)
        self.assertEqual([task["id"] for task in host1.tasks()], ["900", "901"])

    def test_three_nights_in_a_row_without_a_result_file_a_task(self):
        host1 = NightlyHost()
        parent = host1.main()
        heads = []
        for night in range(3):
            heads.append(host1.advance(f"n{night}"))
            status, _, err = host1.night(host1.rounds(1.0), CLADEFS_FAKE_NIGHTLY="fail-build-b")
            self.assertEqual(status, 0, err)
            self.assertEqual(len(host1.tasks()), 0 if night < 2 else 1)
        records = host1.nights()
        self.assertEqual([(r["base"], r["head"], r["outcome"], r["why"]) for r in records],
                         [(parent, head, "could not run",
                           "head failed to build (see build-b.log and guest-kernel-b.log)")
                          for head in heads])
        [task] = host1.tasks()
        self.assertEqual(task["text"].split("\n", 1)[0], "# Nightly: 3 nights in a row measured nothing "
                                                          "[nightly:no-result]")
        self.assertIn("**The last 3 nights measured nothing** (a task is filed).", host1.history())
        # A night that measured ends the streak.
        host1.advance("fixed")
        status, _, err = host1.night(host1.rounds(1.0))
        self.assertEqual(status, 0, err)
        self.assertEqual(host1.nights()[-1]["outcome"], "measured")
        self.assertNotIn("measured nothing**", host1.history())

    def test_a_night_whose_rounds_were_all_left_out_has_no_result(self):
        host1 = NightlyHost()
        host1.advance()
        status, _, err = host1.night(host1.rounds(0.8, verdicts={1: "contaminated", 2: "contaminated",
                                                               3: "contaminated"}))
        self.assertEqual(status, 0, err)
        [record] = host1.nights()
        self.assertEqual((record["outcome"], record["why"], record["moved"]),
                         ("no result", "every round was left out (contaminated, or its control "
                                       "moved)", []))
        self.assertIn("**The last night measured nothing**: a task is filed at 3.",
                      host1.history())
        self.assertEqual(host1.tasks(), [])

    def test_a_night_that_measured_nothing_is_measured_again(self):
        """TASK-489: the first night built both sides and measured nothing; main did not
        move, and the next night did nothing, as the night had measured main's code."""
        host1 = NightlyHost()
        parent = host1.main()
        first = host1.advance("first")
        left_out = {1: "contaminated", 2: "contaminated", 3: "contaminated"}
        for rounds, environment in [(None, {}), (host1.rounds(1.0, left_out), {}),
                                    (host1.rounds(1.0), {"CLADEFS_FAKE_NIGHTLY": "fail-build-b"}),
                                    (host1.rounds(1.0), {})]:
            status, _, err = host1.night(rounds, **environment)
            self.assertEqual(status, 0, err)
        self.assertEqual([(r["base"], r["head"], r["outcome"]) for r in host1.nights()],
                         [(parent, first, "could not run"), (parent, first, "no result"),
                          (parent, first, "could not run"), (parent, first, "measured")])
        # Measured, it is not measured again; main's next head is, against it.
        status, _, err = host1.night(host1.rounds(1.0))
        self.assertEqual((status, len(host1.nights())), (0, 4), err)
        second = host1.advance("second")
        host1.night(host1.rounds(1.0, left_out))
        third = host1.advance("third")
        host1.night(host1.rounds(1.0))
        self.assertEqual([(r["base"], r["head"], r["outcome"]) for r in host1.nights()[4:]],
                         [(first, second, "no result"), (first, third, "measured")])

    def test_a_night_of_the_second_and_head_of_another_has_results_of_its_own(self):
        """TASK-647: nights a second apart of one head, as this test's nights are on CI's runners,
        named their results alike: the second's mkdir failed, `File exists`. Every name of the
        next two minutes taken, the night takes the first count after its own."""
        host1 = NightlyHost()
        host1.main()
        head = host1.advance("first")
        host = subprocess.run(["hostname", "-s"], capture_output=True, text=True,
                              check=True).stdout.strip()
        measurements = host1.clone / "docs" / "measurements"
        now = datetime.datetime.now(datetime.timezone.utc)
        for second in range(120):
            at = now + datetime.timedelta(seconds=second)
            (measurements / f"{host}-nightly-{at:%Y-%m-%dT%H%M%SZ}-{head[:8]}").mkdir(parents=True)
        status, _, err = host1.night(host1.rounds(1.0))
        self.assertEqual(status, 0, err)
        listed = host1.git(host1.origin, "ls-tree", "-r", "--name-only", "main", "docs/measurements")
        nights = [path.split("/")[2] for path in listed.splitlines() if path.endswith("/night.json")]
        self.assertEqual(len(nights), 1, listed)
        self.assertRegex(nights[0], rf"^{host}-nightly-\d{{4}}-\d\d-\d\dT\d{{6}}Z-{head[:8]}-2$")




def short(sha):
    return sha[:8]


def record_directory(host1, record):
    listed = host1.git(host1.origin, "ls-tree", "-r", "--name-only", "main", "docs/measurements")
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
    def hook(self, host1, text):
        hooks = host1.dir / "hooks"
        hooks.mkdir(exist_ok=True)
        (hooks / "pre-push").write_text(text)
        (hooks / "pre-push").chmod(0o755)
        host1.git(host1.clone, "config", "core.hooksPath", str(hooks))

    def test_a_push_refused_as_main_moved_is_rebased_and_pushed_again(self):
        host1 = NightlyHost()
        host1.advance()
        self.hook(host1, MOVES_MAIN.format(marker=host1.dir / "raced", pusher=host1.pusher))
        status, _, err = host1.night(host1.rounds(1.0))
        self.assertEqual(status, 0, err)
        subjects = host1.git(host1.origin, "log", "--format=%s", "-2", "main").splitlines()
        self.assertEqual(subjects[1], "raced")
        self.assertRegex(subjects[0], r"^nightly \d{4}-\d{2}-\d{2}: [0-9a-f]{8}\.\.[0-9a-f]{8}, "
                                      r"nothing moved$")
        lines = [line.split(" ", 1)[1] for line in host1.log().splitlines()]
        self.assertTrue(lines[-2].startswith("push 1 failed: "), lines)
        self.assertEqual(lines[-1], "pushed to origin's main (try 2)")
        self.assertFalse((host1.state / "push-failed.txt").exists())

    def test_a_failed_push_stays_here_says_so_and_goes_the_next_night(self):
        host1 = NightlyHost()
        first = host1.advance("first")
        self.hook(host1, "#!/bin/sh\necho no pushes tonight >&2\nexit 1\n")
        status, _, err = host1.night(host1.rounds(1.0))
        self.assertEqual(status, 1, err)
        self.assertEqual(host1.main(), first)
        failed = (host1.state / "push-failed.txt").read_text()
        local = host1.git(host1.clone, "rev-parse", "HEAD")
        self.assertIn(f"the nightly's push of {local} to origin's main failed: ", failed)
        self.assertIn("no pushes tonight", failed)
        self.assertIn("its results stay in the nightly's clone", failed)
        self.assertEqual(host1.log().count("failed: "), 3)

        host1.git(host1.clone, "config", "--unset", "core.hooksPath")
        second = host1.advance("second")
        status, _, err = host1.night(host1.rounds(1.0))
        self.assertEqual(status, 0, err)
        first_night, second_night = host1.nights()
        self.assertEqual((first_night["head"], second_night["base"], second_night["head"]),
                         (first, first, second))
        self.assertEqual(host1.git(host1.origin, "log", "--format=%s", "-3", "main").splitlines()[1:],
                         [f"nightly {first_night['date'][:10]}: {short(first_night['base'])}.."
                          f"{short(first)}, nothing moved", "a commit"])
        self.assertEqual(len(second_night["push"]), 1)
        self.assertIn(f"the nightly's push of {local} to origin's main failed", second_night["push"][0])
        self.assertIn("(pushed ", second_night["push"][0])
        self.assertFalse((host1.state / "push-failed.txt").exists())
        self.assertFalse((host1.state / "push-recovered.txt").exists())


class Install(unittest.TestCase):
    def test_the_cron_entry_is_one_at_two_and_replaced_not_added(self):
        host1 = NightlyHost()
        bin_ = host1.dir / "bin"
        crontab = host1.dir / "crontab.txt"
        crontab.write_text("5 * * * * other job\n")
        (bin_ / "crontab").write_text(
            f'#!/bin/sh\nif [ "$1" = -l ]; then cat {crontab}; else cat > {crontab}.new && '
            f'mv {crontab}.new {crontab}; fi\n')
        for tool in ("hotbed",):
            if not (bin_ / tool).exists():
                (bin_ / tool).write_text("#!/bin/sh\nexit 0\n")
                (bin_ / tool).chmod(0o755)
        (bin_ / "crontab").chmod(0o755)
        environment = {**host1.environment, "CLADEFS_NIGHTLY_CRONTAB": str(bin_ / "crontab"),
                       "CLADEFS_NIGHTLY_REPO": str(host1.dir / "installed")}
        environment.pop("HOTBED_CONTROLLER", None)
        for _ in range(2):
            done = subprocess.run(["/bin/bash", str(SCRIPTS / "nightly-install.sh"),
                                   str(host1.origin)], capture_output=True, text=True,
                                  env=environment)
            self.assertEqual(done.returncode, 0, done.stderr)
        state = host1.home / ".local" / "state" / "cladefs" / "nightly"
        self.assertEqual(crontab.read_text().splitlines(), [
            "5 * * * * other job",
            f"0 2 * * * PATH={bin_}:/usr/local/bin:/usr/bin:/bin "
            "HOTBED_CONTROLLER= "
            f"{host1.dir / 'installed'}/scripts/nightly-host.sh >> {state}/cron.log 2>&1 "
            "# cladefs nightly (scripts/nightly-install.sh)"])
        self.assertEqual(host1.git(host1.dir / "installed", "config", "user.name"), "cladefs nightly")

    def test_what_the_nightly_needs_is_checked_first(self):
        host1 = NightlyHost()
        # A PATH of only what the script must find, git and hostname: /usr/bin holds flock on
        # Linux, not on macOS (TASK-441.4).
        path = host1.dir / "path"
        path.mkdir()
        for tool in ("git", "hostname"):
            (path / tool).symlink_to(shutil.which(tool))
        done = subprocess.run(["/bin/bash", str(SCRIPTS / "nightly-install.sh"), str(host1.origin)],
                              capture_output=True, text=True,
                              env={**host1.environment, "PATH": str(path)})
        self.assertEqual((done.returncode, done.stderr),
                         (1, "The nightly can't run here: missing hotbed flock\n"))


if __name__ == "__main__":
    unittest.main()
