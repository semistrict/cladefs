# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""hotbed, as scripts/lib/host.sh and the drivers call it, for their tests (test_host.py): one node,
$FAKE_HOTBED/node (default node1), whose workspace is $FAKE_HOTBED/work (the drivers'
CLADEFS_HOTBED_WORK), the checkout cladefs there a clone of the caller's, synced from it.

  session current|start|end [--json]   the one session, $FAKE_HOTBED/session while it lives, each
                                       started with a workspace of its own
  run [FLAG...] -- COMMAND...          COMMAND in the workspace's checkout
  logs -f ID                           a detached command's output, then its status
  stop ID                              ends a detached command

run syncs the caller's checkout into the workspace first, but with --no-sync: its HEAD checked out
there, and every file git doesn't ignore copied over. It runs COMMAND with --env's variables, a
TMPDIR of its own, and CLADEFS_SELF_CGROUP naming a file that says its cgroup, in hotbed's
measurement slice with --bench, in its commands' slice without, as /proc/self/cgroup does
(scripts/lib/measurement.sh); a node of $FAKE_HOTBED/arch (default x86_64), which uname -m says
there, refusing --arch of another. As hotbed does, it refuses --node, which it no longer takes
(board 14457), and a --detach without --mem, the memory hotbed reserves for it, the refusal JSON on
standard output with --json. It says on standard error, as hotbed does, where it runs.
Attached, it runs to its end, its status the command's; with --detach it runs on in a process
group of its own, its output to commands/ID/log and its status to commands/ID/status, and prints
the envelope naming it. Each call is a line of $FAKE_HOTBED/calls: its subcommand and flags, and
COMMAND's first word: the switches, sorted, then --arch's and --mem's values.
"""

import json
import os
import secrets
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

STATE = Path(os.environ["FAKE_HOTBED"])
NODE = (STATE / "node").read_text().strip() if (STATE / "node").exists() else "node1"
ARCH = (STATE / "arch").read_text().strip() if (STATE / "arch").exists() else "x86_64"
WORK = STATE / "work"
CHECKOUT = WORK / "cladefs"
COMMANDS = STATE / "commands"
# uname -m, as the node says its architecture; anything else this machine's.
UNAME = f"""#!/bin/sh
if [ "$*" = -m ]; then echo {ARCH}; else exec /usr/bin/uname "$@"; fi
"""


def envelope(command, data):
    print(json.dumps({"version": "1", "ok": True, "command": command, "data": data}))


def git(*args, cwd=CHECKOUT):
    subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True)


def sync():
    """The caller's checkout into the workspace: its HEAD, and what git doesn't ignore."""
    caller = Path(subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True,
                                 text=True, check=True).stdout.strip())
    if not CHECKOUT.exists():
        WORK.mkdir(parents=True, exist_ok=True)
        git("clone", "-q", str(caller), str(CHECKOUT), cwd=WORK)
    head = subprocess.run(["git", "-C", str(caller), "rev-parse", "HEAD"], capture_output=True,
                          text=True, check=True).stdout.strip()
    git("fetch", "-q", str(caller), head)
    git("checkout", "-q", "-f", "--detach", head)
    git("submodule", "update", "-q", "--init")
    files = subprocess.run(["git", "-C", str(caller), "ls-files", "-co", "--exclude-standard", "-z"],
                           capture_output=True, text=True, check=True).stdout.split("\0")
    for name in filter(None, files):
        source, target = caller / name, CHECKOUT / name
        if source.is_dir() and not source.is_symlink():
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        if target.is_symlink() or target.exists():
            target.unlink()
        shutil.copy2(source, target, follow_symlinks=False)


def run(argv):
    flags, command = argv[:argv.index("--")], argv[argv.index("--") + 1:]
    options = {"--env": []}
    switches = set()
    i = 0
    while i < len(flags):
        flag = flags[i]
        if flag in ("--no-sync", "--detach", "--json", "--bench", "--keep-idle"):
            switches.add(flag)
            i += 1
        elif flag == "--env":
            options["--env"].append(flags[i + 1])
            i += 2
        else:
            options[flag] = flags[i + 1]
            i += 2
    with open(STATE / "calls", "a") as calls:
        said = sorted(switches - {"--json"}) + [f"{k} {options[k]}" for k in ("--arch", "--mem")
                                                if k in options]
        calls.write(" ".join(["run", *said, command[0]]) + "\n")
    if not (STATE / "session").exists() and not os.environ.get("HOTBED_SESSION"):
        sys.exit("error: no session: hotbed session start")
    if "--node" in options:
        sys.exit("error: unknown flag --node")
    if "--detach" in switches and "--mem" not in options:
        refusal = {"code": "INVALID_ARGS", "message": "a detached run needs --mem"}
        if "--json" in switches:
            print(json.dumps({"version": "1", "ok": False, "command": "run", "error": refusal}))
            sys.exit(1)
        sys.exit(f"error: {refusal['message']}")
    wanted = {"amd64": "x86_64", "arm64": "aarch64"}.get(options.get("--arch", ""), ARCH)
    if wanted != ARCH:
        sys.exit(f"error: no node of architecture {options['--arch']}")
    if "--no-sync" not in switches:
        sync()
    ident = secrets.token_hex(5)
    directory = COMMANDS / ident
    (directory / "tmp").mkdir(parents=True)
    (directory / "bin").mkdir()
    (directory / "bin" / "uname").write_text(UNAME)
    (directory / "bin" / "uname").chmod(0o755)
    slice_ = "user-1000-hotbed_bench.slice" if "--bench" in switches else "user-1000-hotbed.slice"
    (directory / "cgroup").write_text(f"0::/user.slice/user-1000.slice/{slice_}/hotbed-{ident}.service\n")
    env = {**os.environ, "TMPDIR": str(directory / "tmp"), "HOTBED_COMMAND": ident,
           "CLADEFS_SELF_CGROUP": str(directory / "cgroup"),
           "PATH": f"{directory / 'bin'}:{os.environ['PATH']}"}
    env.update(pair.split("=", 1) for pair in options["--env"])
    print(f"hotbed: {ident} running in ws1 on {NODE} (hotbed stop {ident})", file=sys.stderr)
    if "--detach" in switches:
        with open(directory / "log", "w") as log:
            process = subprocess.Popen(
                ["/bin/bash", "-c", 'status=0; "$@" || status=$?; echo "$status" > "$0"',
                 str(directory / "status"), *command],
                cwd=CHECKOUT, env=env, stdout=log, stderr=subprocess.STDOUT,
                stdin=subprocess.DEVNULL, start_new_session=True)
        (directory / "pid").write_text(str(process.pid))
        envelope("run", {"id": ident, "state": "COMMAND_STATE_RUNNING"})
        return 0
    return subprocess.run(command, cwd=CHECKOUT, env=env).returncode


def logs(argv):
    ident = argv[-1]
    directory = COMMANDS / ident
    if not directory.exists():
        sys.exit(f"error: no command {ident}")
    while not (directory / "status").exists():
        time.sleep(0.05)
    sys.stdout.write((directory / "log").read_text())
    return int((directory / "status").read_text())


def stop(argv):
    ident = argv[-1]
    directory = COMMANDS / ident
    if not (directory / "status").exists():
        pid = int((directory / "pid").read_text())
        try:
            os.killpg(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        # The group's shell, which would write the status, ended with it: its status is TERM's.
        while True:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                break
            time.sleep(0.05)
        if not (directory / "status").exists():
            (directory / "status").write_text("143\n")
    envelope("stop", {"id": ident, "state": "COMMAND_STATE_CANCELLED"})
    return 0


def session(argv):
    action = argv[0]
    with open(STATE / "calls", "a") as calls:
        calls.write(f"session {action}\n")
    if action == "current":
        if not (STATE / "session").exists():
            sys.exit("error: no session bound here")
        envelope("session current", {"id": (STATE / "session").read_text().strip()})
    elif action == "start":
        # A session of its own, a workspace of its own: the last one's goes.
        shutil.rmtree(WORK, ignore_errors=True)
        (STATE / "session").write_text("fake-session\n")
        envelope("session start", {"id": "fake-session"})
    elif action == "end":
        (STATE / "session").unlink()
        envelope("session end", {"id": "fake-session"})
    return 0


def board(argv):
    state = Path(os.environ["BOARD_STATE"])
    tasks = json.loads(state.read_text()) if state.exists() else []

    def option(name):
        return argv[argv.index(name) + 1]

    def options(name):
        return [argv[at + 1] for at, arg in enumerate(argv) if arg == name]

    assert "--json" in argv, argv
    profile = option("--as")
    if argv[:2] == ["task", "create"] and "-" in argv:
        assert option("-p") == "cladefs", argv
        task = {"id": str(900 + len(tasks)), "text": sys.stdin.read(), "state": "ready",
                "as": profile,
                "labels": dict(label.split("=", 1) for label in options("--label")),
                "replies": []}
        tasks.append(task)
        envelope("board task create", {"task": {"id": task["id"]}, "outcome": "created"})
    elif argv[:2] == ["task", "ls"]:
        assert option("-p") == "cladefs", argv
        key, value = option("--label").split("=", 1)
        listed = [t for t in tasks if t["labels"].get(key) == value and t["state"] != "completed"]
        envelope("board task ls",
                 {"tasks": [{"message": {"id": t["id"], "text": t["text"]}} for t in listed],
                  "cursor": "0"})
    elif argv[0] == "reply" and argv[2] == "-":
        task = next(t for t in tasks if t["id"] == argv[1])
        task["replies"].append({"text": sys.stdin.read(), "as": profile})
        envelope("board reply", {"message": {"id": "1"}, "outcome": "posted"})
    else:
        sys.exit(f"fake_hotbed board: no such call: {argv}")
    state.write_text(json.dumps(tasks))
    return 0


def main(argv):
    command, rest = argv[0], argv[1:]
    if command == "run":
        return run(rest)
    if command == "logs":
        return logs(rest)
    if command == "stop":
        return stop(rest)
    if command == "session":
        return session(rest)
    if command == "board":
        return board(rest)
    sys.exit(f"fake_hotbed: no such command: {command}")


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
