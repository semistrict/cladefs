# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""ssh, as scripts/lib/host.sh calls it, for its tests (test_host.py): hosts kept in
$FAKE_SSH/hosts/NAME, each a home, where each call's command runs, in bash, with the call's
standard input, and a PATH whose first directory, $FAKE_SSH/host-bin, holds the host's tools that
this machine lacks or that a test watches: sudo running what it is given; setsid running it in a
session of its own; fxjob (FXJOB) recording its options in the home's fxjob-calls and running its
command in its job's cgroup, a measurement's or work's;
timeout running its command; and flock, fcntl's locks. Each call's host and command, a line, are
appended to $FAKE_SSH/calls.

$FAKE_SSH/state.json says how calls go: "down", the hosts that refuse connections, every call to
one ending 255; and "calls", call by call until they run out, what each does instead of running
alone: {"status": N} runs it and ends N, its output kept, as a connection dropped once it ran
does; {"fail": N} runs nothing and ends N; null runs it. Git on a host runs as this machine's,
copying from local paths (protocol.file.allow).

Usage: python3 fake_ssh.py [-o OPTION]... HOST COMMAND
"""

import json
import os
import subprocess
import sys
from pathlib import Path

# fxjob, which records its options in the home's fxjob-calls and runs its command as in a job's
# cgroup, a scope of fxdax-bench.slice for a measurement, of fxdax-work.slice for work, which
# CLADEFS_SELF_CGROUP names as /proc/self/cgroup does on the host: what scripts/lib/measurement.sh
# reads, and sudo keeps.
FXJOB = r"""#!/bin/bash
echo "$*" >> "$HOME/fxjob-calls"
slice=fxdax-work.slice
while [[ $1 == -* ]]; do
    case $1 in --) shift; break ;; --bench) slice=fxdax-bench.slice; shift ;; *) shift 2 ;; esac
done
export CLADEFS_SELF_CGROUP=$HOME/fxjob-cgroup-$$
echo "0::/user.slice/user-1000.slice/user@1000.service/fxdax.slice/$slice/fxjob-$$.scope" \
    > "$CLADEFS_SELF_CGROUP"
exec "$@"
"""
TOOLS = {
    "sudo": '#!/bin/sh\nexec "$@"\n',
    "setsid": f'#!/bin/sh\nexec {sys.executable} -c '
              '\'import os, sys; os.setsid(); os.execvp(sys.argv[1], sys.argv[1:])\' "$@"\n',
    "fxjob": FXJOB,
    "timeout": '#!/bin/bash\nwhile [[ $1 == -* ]]; do shift; done\nshift\nexec "$@"\n',
    "flock": f'#!/bin/sh\nexec {sys.executable} {Path(__file__).with_name("fake_flock.py")} "$@"\n',
}


def tools(directory=None):
    """The host's tools' directory, made (default $FAKE_SSH/host-bin; a local host's, as the
    nightly's tests run one, another): each tool written whole under a name of this call's, then
    renamed into place, as a job an earlier call launched may run one meanwhile. Written over in
    place, as each call did, a tool the job ran was empty, which sh ran as nothing, ending 0, or,
    on Linux, open for writing (ETXTBSY): the source's host script of a migration never ran, its
    snapshot not left for the target (TASK-385)."""
    directory = Path(directory) if directory else STATE / "host-bin"
    directory.mkdir(exist_ok=True)
    for name, text in TOOLS.items():
        written = directory / f".{name}.{os.getpid()}"
        written.write_text(text)
        written.chmod(0o755)
        written.replace(directory / name)
    return directory


def home(host):
    """The home of `host`, made."""
    directory = STATE / "hosts" / host / "home"
    directory.mkdir(parents=True, exist_ok=True)
    return directory


def main(argv):
    while argv and argv[0] == "-o":
        argv = argv[2:]
    host, command = argv[0], " ".join(argv[1:])
    with open(STATE / "calls", "a") as calls:
        print(f"{host} {command}", file=calls)
    state = json.loads((STATE / "state.json").read_text())
    if host in state.get("down", []):
        print(f"ssh: connect to host {host} port 22: Connection refused", file=sys.stderr)
        return 255
    how = None
    if state.get("calls"):
        how = state["calls"].pop(0)
        (STATE / "state.json").write_text(json.dumps(state))
    if how and "fail" in how:
        return how["fail"]
    environment = {
        "HOME": str(home(host)),
        "PATH": f"{tools()}:/usr/bin:/bin:/usr/sbin:/sbin",
        "GIT_CONFIG_COUNT": "1",
        "GIT_CONFIG_KEY_0": "protocol.file.allow",
        "GIT_CONFIG_VALUE_0": "always",
    }
    done = subprocess.run(["/bin/bash", "-c", command], env=environment, cwd=home(host))
    return how["status"] if how else done.returncode


# The fake hosts' state, read only when this runs: the tests that take FXJOB alone have none.
if __name__ == "__main__":
    STATE = Path(os.environ["FAKE_SSH"])
    sys.exit(main(sys.argv[1:]))
