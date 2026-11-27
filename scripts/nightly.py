#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""The nightly measurement's planning and reporting (TASK-441.1, TASK-441.2), which
scripts/nightly-host.sh runs on the nightly host from cron, in the nightly's own clone, ROOT.

Each night's results are a directory of docs/measurements, HOST-nightly-<time>-<head>, holding
what scripts/bench-ab-host.sh fetched, `night.json`, what the night found, and `nightly.md`, the
same for a reader. They are its state too: the newest night that measured is the last word on
main, and its head is tonight's baseline.

  plan
      `skip WHY` if the newest night that measured (outcome `measured`) measured main's code
      already, else `run BASE HEAD`. HEAD is the newest commit of origin's main that changed more
      than what the nightly writes (docs/measurements), so that a night's own commit is no change to measure. BASE is the newest
      measured night's head; with none, the first night's base; with no night at all, HEAD's
      first parent. A night that could not run, or whose rounds were all left out, measured
      nothing: the next night measures the same commits again, or main's newer head against the
      same base (TASK-489).
  report --results DIR --base SHA --head SHA --status N
      judges the night (scripts/ab-rounds.py), writes night.json and nightly.md in DIR,
      regenerates docs/measurements/nightly-history.md, and files a task on the board for each metric
      that moved, or notes it on the open task filed for it before; and one when the last three
      nights had no result.
  history
      regenerates the history page alone.
  sync
      takes origin's main: fetches, rebases what the nightly committed and could not push on it
      (or sets it aside on a branch, nightly-unpushed-<time>, if it conflicts) and pushes it.
  publish --results DIR
      commits the night's results and the history page, and pushes them to
      origin's main, fetching and rebasing on a main that moved meanwhile, never forcing. A push
      that fails leaves the commit here and says so in STATE/push-failed.txt, which the next
      night's report repeats, and its sync pushes.

ROOT is the tree this script is in unless --root names another; STATE, ~/.local/state/cladefs/
nightly unless CLADEFS_NIGHTLY_STATE names another, keeps nightly.log, every step's line.
"""

import argparse
import datetime
import importlib.util
import json
import os
import statistics
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent

spec = importlib.util.spec_from_file_location("ab_rounds", HERE / "ab-rounds.py")
ab_rounds = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = ab_rounds
spec.loader.exec_module(ab_rounds)

NIGHT = "night.json"
HISTORY = "nightly-history.md"
# What a night that measured nothing is: one that could not run, or whose every timing had no
# result (its rounds all left out).
BAD = ("could not run", "no result")
# How many bad nights in a row file a task.
BAD_NIGHTS = 3
# The board's project, the labels of what the nightly files, and the profile it writes as.
PROJECT = "cladefs"
LABELS = ["priority=high", "tag.perf=yes", "tag.nightly=yes"]
PROFILE = "nightly"
PUSH_TRIES = 3
A, B = "base", "head"


def now():
    """The time, UTC, to the microsecond: nights are ordered by it."""
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def state_dir(environ=os.environ):
    directory = Path(environ.get("CLADEFS_NIGHTLY_STATE")
                     or Path.home() / ".local" / "state" / "cladefs" / "nightly")
    directory.mkdir(parents=True, exist_ok=True)
    return directory


def log(state, text):
    """Says `text` on standard error and in STATE/nightly.log."""
    line = f"{now()} {text}"
    print(line, file=sys.stderr)
    with open(state / "nightly.log", "a") as out:
        out.write(line + "\n")


def nights(measurements):
    """Every night's record under `measurements`, oldest first, each with its directory's name."""
    found = []
    for path in Path(measurements).glob(f"*-nightly-*/{NIGHT}"):
        record = json.loads(path.read_text())
        record["directory"] = path.parent.name
        found.append(record)
    return sorted(found, key=lambda record: (record["date"], record["directory"]))


# What the nightly writes, which is no change of main's code to measure.
NOT_CODE = (":(exclude)docs/measurements",)


def plan(root, measurements):
    """What tonight does: ("skip", why) or ("run", base, head)."""
    head = git(root, "log", "-1", "--format=%H", "origin/main", "--", ".", *NOT_CODE).stdout.strip()
    found = nights(measurements)
    measured = [record for record in found if record["outcome"] == "measured"]
    if measured and measured[-1]["head"] == head:
        return "skip", (f"main's code is {head[:12]}'s, which the night of "
                        f"{measured[-1]['date']} measured")
    if measured:
        return "run", measured[-1]["head"], head
    if found:
        return "run", found[0]["base"], head
    parent = git(root, "rev-parse", "--verify", "--quiet", f"{head}^", check=False).stdout.strip()
    if not parent:
        sys.exit(f"{head} has no parent, and no night measured before it: nothing to compare")
    return "run", parent, head


def short(sha):
    return sha[:8] if sha else "-"


def key(row):
    """A moved metric's stable key, which its task's title carries."""
    return f"{row['case']} {row['name']} {row['verdict']}"


def change(row):
    """How much `row` moved, a phrase: a timing's median ratio across its rounds, a count's
    values."""
    if row["kind"] == "timing":
        ratios = [each["ratio"] for each in row["rounds"] if "ratio" in each]
        factor = statistics.median(ratios)
        return f"{abs(factor - 1) * 100:.0f}% {row['verdict']}"
    first = row["rounds"][0]
    return f"{row['verdict']}, {ab_rounds.number(first['a'])} → {ab_rounds.number(first['b'])}"


def replay(row, record, environ):
    """The commands that replay `row`'s measurement on the nightly host, from a checkout of
    main."""
    base, head = record["base"], record["head"]
    if row["kind"] == "count" and row["case"] in ab_rounds.COUNT_BENCHES:
        return (f"for c in {base} {head}; do git worktree add /tmp/counts-$c $c; "
                f"scripts/counts.sh /tmp/counts-$c /tmp/counts-$c/out /tmp/counts-$c/work; done  "
                f"# then compare out/ of each (scripts/ab-rounds.py's {row['case']})")
    cases = environ.get("CLADEFS_BENCH_CASES", "")
    groups = environ.get("CLADEFS_BENCH_GROUPS", "")
    rounds = environ.get("CLADEFS_AB_ROUNDS", "3")
    return (f"CLADEFS_AB_A=base={base} CLADEFS_AB_B=head={head} CLADEFS_AB_ROUNDS={rounds} "
            f"CLADEFS_AB_RERUN=1 CLADEFS_BENCH_CASES='{cases}' CLADEFS_BENCH_GROUPS='{groups}' "
            f"scripts/bench-ab-host.sh RESULTS && scripts/ab-rounds.py compare RESULTS base head")


def outcome(results, comparison, status):
    """What the night came to: (outcome, why), outcome measured, no result or could not run."""
    failed = results / "build-failed"
    if failed.exists():
        side = failed.read_text().strip()
        name = A if side == "a" else B
        return "could not run", (f"{name} failed to build (see build-{side}.log and "
                                 f"guest-kernel-{side}.log)")
    if comparison is None or not comparison["rounds"]:
        return "could not run", (f"no round ran (the A/B ended {status}; see remote.log and "
                                 f"driver.log)")
    timings = [row for row in comparison["metrics"] if row["kind"] == "timing"]
    if not any(each["kept"] for each in comparison["rounds"].values()) or (
            timings and all(row["verdict"] == "no result" for row in timings)):
        return "no result", "every round was left out (contaminated, or its control moved)"
    return "measured", None


def judge(results, base, head, status, environ=os.environ):
    """The night's record, from its results."""
    results = Path(results)
    comparison = None
    if ab_rounds.rounds(results, A) or ab_rounds.rounds(results, B):
        comparison = ab_rounds.compare(results, A, B, environ.get("CLADEFS_AB_CONTROL",
                                                                  ab_rounds.CONTROL) or None,
                                       ab_rounds.Bounds.of(environ))
    said, why = outcome(results, comparison, status)
    record = {
        "date": now(),
        "base": base,
        "head": head,
        "status": status,
        "outcome": said,
        "why": why,
        "head_built": (results / "artifacts-b.txt").exists() and not (
            results / "build-failed").exists(),
        "moved": [],
        "broken": [],
        "no_result": [],
        "tasks": [],
    }
    if comparison is not None:
        (results / "rounds.json").write_text(json.dumps(comparison, indent=1) + "\n")
        record["moved"] = [{**row, "key": key(row), "change": change(row),
                            "replay": replay(row, record, environ)}
                           for row in ab_rounds.moved(comparison)]
        record["broken"] = [{**each, "key": each["what"],
                             "replay": replay({"kind": "timing"}, record, environ)}
                            for each in comparison["broken"]]
        record["no_result"] = [ab_rounds.metric(row) for row in ab_rounds.no_result(comparison)]
    return record, comparison


class Board:
    """Hotbed's native task board (`hotbed board`): the open tasks the nightly filed, and filing
    them, as the profile `nightly`. A task is cited as `board ID`."""

    def run(self, *args, text=None):
        done = subprocess.run(["hotbed", "board", *args, "--json", "--as", PROFILE], input=text,
                              capture_output=True, text=True)
        if done.returncode:
            raise RuntimeError(f"hotbed board {' '.join(args[:2])} failed: {done.stderr.strip()}")
        return json.loads(done.stdout)["data"]

    def open_task(self, marker):
        """The open nightly task whose title carries `marker`, its ID; None if none."""
        after = []
        while True:
            page = self.run("task", "ls", "-p", PROJECT, "--label", "tag.nightly=yes",
                            "--limit", "500", *after)
            tasks = page.get("tasks", [])
            for task in tasks:
                if marker in task["message"]["text"].split("\n", 1)[0]:
                    return task["message"]["id"]
            if len(tasks) < 500:
                return None
            after = ["--after", page["cursor"]]

    def file(self, marker, title, description, note):
        """Notes `note` on the open task `marker` names, or files a task: (`board ID`, filed or
        noted)."""
        found = self.open_task(marker)
        if found:
            self.run("reply", found, "-", text=note)
            return f"board {found}", "noted"
        labels = [arg for label in LABELS for arg in ("--label", label)]
        made = self.run("task", "create", "-p", PROJECT, "-", *labels,
                        text=f"# {title} {marker}\n\n## Description\n\n{description}\n")
        return f"board {made['task']['id']}", "filed"


def marker(text):
    return f"[nightly:{text}]"


def file_tasks(board, record, results_name):
    """One task for each metric of `record` that moved and each thing that broke, or a note on the
    open one filed for it before; the tasks, as record['tasks'] lists them."""
    base, head = record["base"], record["head"]
    where = f"docs/measurements/{results_name}/nightly.md"
    tasks = []
    for row in record["moved"]:
        rounds = ab_rounds.rounds_line(row, A, B)
        title = (f"Nightly: {row['case']} {row['name']} {row['change']} on main, "
                 f"{short(base)}..{short(head)}")
        description = (
            f"The nightly (scripts/nightly-host.sh, TASK-441) found **{row['case']} "
            f"{row['name']}** ({row['unit']}) {row['verdict']} past its bound, "
            f"{row['bound'] * 100:g}%, in every round it kept, the control steady.\n\n"
            f"- Base, the commit the night before measured: {base}\n"
            f"- Head, main: {head}\n"
            f"- Each round, {A} against {B}: {rounds}\n"
            f"- Results: {where}\n\n"
            f"Replay on the nightly host, from a checkout of main:\n\n    {row['replay']}\n\n"
            "Find the commit of the range that moved it (git log base..head), and either fix it "
            "or say here why the change is right; a later night that measures it again notes it "
            "here.")
        note = (f"{record['date'][:10]}: again on {short(base)}..{short(head)}: {rounds} "
                f"({where})")
        found, action = board.file(marker(row["key"]), title, description, note)
        tasks.append({"key": row["key"], "id": found, "action": action})
    for each in record["broken"]:
        title = f"Nightly: {each['what']} on main, {short(base)}..{short(head)}"
        description = (
            f"The nightly (scripts/nightly-host.sh, TASK-441): {each['said']}.\n\n"
            f"- Base: {base}\n- Head: {head}\n- Results: {where}\n"
            + "".join(f"- {detail}\n" for detail in each["details"])
            + f"\nReplay on the nightly host, from a checkout of main:\n\n    {each['replay']}\n")
        note = f"{record['date'][:10]}: again on {short(base)}..{short(head)} ({where})"
        found, action = board.file(marker(each["key"]), title, description, note)
        tasks.append({"key": each["key"], "id": found, "action": action})
    return tasks


def bad_streak(found):
    """How many of the newest nights in `found` in a row measured nothing."""
    streak = 0
    for record in reversed(found):
        if record["outcome"] not in BAD:
            break
        streak += 1
    return streak


def file_streak(board, found, results_name):
    """The task for BAD_NIGHTS nights in a row that measured nothing, filed or noted, if the newest
    nights of `found` are such; else nothing."""
    streak = bad_streak(found)
    if streak < BAD_NIGHTS:
        return []
    said = "\n".join(f"- {record['date'][:16]} ({short(record['base'])}..{short(record['head'])}): "
                     f"{record['outcome']}: {record['why']}" for record in found[-streak:])
    title = f"Nightly: {streak} nights in a row measured nothing"
    description = (
        f"The last {streak} nights of the nightly (scripts/nightly-host.sh, TASK-441) measured "
        f"nothing: main went unmeasured for {streak} nights.\n\n{said}\n\n"
        f"The newest: docs/measurements/{results_name}/nightly.md; the history: "
        f"docs/measurements/{HISTORY}. Find why (a build broken, a host whose rounds are all "
        "left out), and fix it.")
    note = f"{found[-1]['date'][:10]}: {streak} nights in a row now. {said.splitlines()[-1]}"
    found_id, action = board.file(marker("no-result"), title, description, note)
    return [{"key": "no-result", "id": found_id, "action": action}]


def push_said(state):
    """What STATE says of an earlier night's push: that it failed, and whether it went since."""
    said = []
    for name in ("push-failed.txt", "push-recovered.txt"):
        path = state / name
        if path.exists():
            said.append(path.read_text().strip())
    return said


def night_markdown(record, comparison, push):
    lines = [f"# The nightly of {record['date'][:16].replace('T', ' ')} UTC", "",
             f"Base {record['base']}, head (main) {record['head']}: {record['outcome']}"
             + (f": {record['why']}" if record["why"] else "") + ".", ""]
    if push:
        lines += ["## An earlier night's push", ""] + [f"- {line}" for line in push] + [""]
    if record["tasks"]:
        lines += ["## Tasks", ""] + [f"- {task['id']} ({task['action']}): {task['key']}"
                                     for task in record["tasks"]] + [""]
    if comparison is not None:
        lines.append(ab_rounds.markdown(comparison))
    return "\n".join(lines).rstrip("\n") + "\n"


def history(measurements):
    """Writes the history page from every night's record, newest first."""
    found = nights(measurements)
    streak = bad_streak(found)
    lines = [
        "# The nightly's history",
        "",
        "Each night on the nightly host (scripts/nightly-host.sh, TASK-441), main's head measured "
        "against the commit the night before measured, in interleaved rounds of the same night, "
        "with the control case and exact counts (scripts/ab-rounds.py). A night main did not move "
        "ran nothing and is not here. A metric that moved is a task on the board, labelled "
        "tag.nightly. "
        "Written by scripts/nightly.py; do not edit.",
        "",
    ]
    if streak:
        lines += [("**The last night measured nothing**" if streak == 1 else
                   f"**The last {streak} nights measured nothing**")
                  + (f": a task is filed at {BAD_NIGHTS}." if streak < BAD_NIGHTS else
                     " (a task is filed).") + "", ""]
    lines += ["| Night (UTC) | Base | Head | Outcome | Moved | Tasks | Results |",
              "|---|---|---|---|---|---|---|"]
    for record in reversed(found):
        outcome_ = record["outcome"] + (f": {record['why']}" if record["why"] else "")
        moved = ", ".join(f"{row['case']} {row['name']} {row['change']}"
                          for row in record["moved"])
        moved += "".join(f"{'; ' if moved else ''}{each['what']}" for each in record["broken"])
        tasks = ", ".join(f"{task['id']} ({task['action']})" for task in record["tasks"])
        lines.append(f"| {record['date'][:16].replace('T', ' ')} | {short(record['base'])} | "
                     f"{short(record['head'])} | {outcome_} | {moved or '-'} | {tasks or '-'} | "
                     f"[{record['directory']}]({record['directory']}/nightly.md) |")
    (Path(measurements) / HISTORY).write_text("\n".join(lines) + "\n")


def report(root, results, base, head, status, state, environ=os.environ):
    results = Path(results)
    measurements = root / "docs" / "measurements"
    record, comparison = judge(results, base, head, status, environ)
    push = push_said(state)
    record["push"] = push
    board = Board()
    record["tasks"] = file_tasks(board, record, results.name)
    (results / NIGHT).write_text(json.dumps(record, indent=1) + "\n")
    found = nights(measurements)
    record["tasks"] += file_streak(board, found, results.name)
    (results / NIGHT).write_text(json.dumps(record, indent=1) + "\n")
    (results / "nightly.md").write_text(night_markdown(record, comparison, push))
    history(measurements)
    recovered = state / "push-recovered.txt"
    if recovered.exists():
        recovered.unlink()
    log(state, f"night {short(base)}..{short(head)}: {record['outcome']}"
               f"{': ' + record['why'] if record['why'] else ''}; {len(record['moved'])} moved, "
               f"{len(record['broken'])} broke; tasks "
               f"{', '.join(t['id'] + ' ' + t['action'] for t in record['tasks']) or 'none'}")
    return record


def git(root, *args, check=True):
    done = subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True)
    if check and done.returncode:
        raise RuntimeError(f"git {' '.join(args)} failed: {done.stderr.strip()}")
    return done


def ahead(root):
    """How many commits here origin's main lacks."""
    return int(git(root, "rev-list", "--count", "origin/main..HEAD").stdout.strip())


def rebase(root, state):
    """Rebases what is here on origin's main; on a conflict, sets it aside on a branch of its own
    and takes origin's main: whether it rebased."""
    if git(root, "rebase", "-q", "origin/main", check=False).returncode == 0:
        return True
    git(root, "rebase", "--abort", check=False)
    branch = f"nightly-unpushed-{datetime.datetime.now(datetime.timezone.utc):%Y%m%dT%H%M%SZ}"
    git(root, "branch", branch)
    git(root, "reset", "-q", "--hard", "origin/main")
    log(state, f"what the nightly could not push conflicts with origin's main: set aside on "
               f"branch {branch} here, main taken from origin")
    (state / "push-failed.txt").write_text(
        f"{now()}: the nightly's unpushed commits conflicted with origin's main; they are on "
        f"branch {branch} of the nightly's clone on {os.uname().nodename}, not on main\n")
    return False


def push(root, state):
    """Pushes what is here to origin's main, PUSH_TRIES times at most, fetching and rebasing on a
    main that moved between; never forcing. Whether it went."""
    said = ""
    for attempt in range(1, PUSH_TRIES + 1):
        done = git(root, "push", "-q", "origin", "HEAD:main", check=False)
        if done.returncode == 0:
            log(state, f"pushed to origin's main (try {attempt})")
            return True
        said = " ".join(done.stderr.split())
        log(state, f"push {attempt} failed: {said}")
        if git(root, "fetch", "-q", "origin", check=False).returncode:
            break
        if not rebase(root, state):
            return False
    head = git(root, "rev-parse", "HEAD").stdout.strip()
    (state / "push-failed.txt").write_text(
        f"{now()}: the nightly's push of {head} to origin's main failed: {said}; its results stay "
        f"in the nightly's clone on {os.uname().nodename}, which the next night pushes\n")
    return False


def sync(root, state):
    """Takes origin's main, what the nightly committed and could not push rebased on it and
    pushed. Whether all of it is on origin's main now."""
    git(root, "fetch", "-q", "origin")
    pending = ahead(root)
    if not rebase(root, state):
        return False
    if not pending:
        return True
    log(state, f"pushing {pending} commit(s) an earlier night could not")
    if not push(root, state):
        return False
    failed = state / "push-failed.txt"
    if failed.exists():
        (state / "push-recovered.txt").write_text(
            failed.read_text().strip() + f" (pushed {now()})\n")
        failed.unlink()
    return True


def publish(root, results, state):
    """Commits the night, and pushes it (`push`). Whether it went."""
    results = Path(results)
    record = json.loads((results / NIGHT).read_text())
    measurements = Path("docs") / "measurements"
    # Of the night's directory, its report and its record alone: the rounds' raw output stays here,
    # on the nightly host, untracked (.gitignore).
    night = measurements / results.name
    git(root, "add", "--", str(night / "nightly.md"), str(night / NIGHT),
        str(measurements / HISTORY))
    summary = record["outcome"]
    if record["moved"] or record["broken"]:
        summary = ", ".join([f"{row['case']} {row['name']} {row['change']}"
                             for row in record["moved"]] +
                            [each["what"] for each in record["broken"]])
    elif record["outcome"] == "measured":
        summary = "nothing moved"
    elif record["why"]:
        summary += f": {record['why']}"
    git(root, "commit", "-q", "-m",
        f"nightly {record['date'][:10]}: {short(record['base'])}..{short(record['head'])}, "
        f"{summary}")
    if push(root, state):
        failed = state / "push-failed.txt"
        if failed.exists():
            failed.unlink()
        return True
    return False


def main(argv, environ=os.environ):
    parser = argparse.ArgumentParser(description=__doc__.strip().splitlines()[0])
    parser.add_argument("--root", default=str(HERE.parent))
    sub = parser.add_subparsers(dest="action", required=True)
    sub.add_parser("plan")
    reported = sub.add_parser("report")
    reported.add_argument("--results", required=True)
    reported.add_argument("--base", required=True)
    reported.add_argument("--head", required=True)
    reported.add_argument("--status", type=int, required=True)
    sub.add_parser("history")
    sub.add_parser("sync")
    published = sub.add_parser("publish")
    published.add_argument("--results", required=True)
    args = parser.parse_args(argv)
    root = Path(args.root).resolve()
    measurements = root / "docs" / "measurements"
    state = state_dir(environ)
    if args.action == "plan":
        print(" ".join(plan(root, measurements)))
    elif args.action == "report":
        report(root, args.results, args.base, args.head, args.status, state, environ)
    elif args.action == "history":
        history(measurements)
    elif args.action == "sync":
        return 0 if sync(root, state) else 1
    elif args.action == "publish":
        return 0 if publish(root, args.results, state) else 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
