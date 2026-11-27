#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""pjdfstest (BSD-2-Clause) on FUSE mounts of new cells, as root (TASK-149.38): POSIX's
permissions, owners, setgid and sticky directories, times and errnos, as the host's Node layer,
fxfs-posix under it and cladefs::fuse's server over it keep them.

Two runs, each on a new cell of a daemon's of its own, which `cladefsctl mount create` makes and
the daemon's FUSE mount shows (`CLADEFSD_MOUNT`, TASK-475.2): the daemon owns its host's cells,
nothing else opening them (board 14305; `cladefs-mount` mounted a cell of its own before):

- in a directory made in the mount, as the guest's pjdfstest case runs in its Fxfs;
- from the mount's root itself, chowned and chmodded first: a new volume's root has no POSIX
  attributes of its own, and a chown of it alone once left it mode 000 (TASK-222).

pjdfstest is built at the commit scripts/lib/pjdfstest.sh pins, in CLADEFS_PJDFSTEST_WORK (default:
target/pjdfstest), once. A run's failures are its failed tests, by file and number, and its files
that failed whole (a bad plan, an exit status); scripts/pjdfstest-fuse-known lists those of open
bugs, each with its task. The script fails if any other fails, or if one listed fails in neither
run, so that the list stays true. The runs' logs are left in the work directory.

Usage: sudo scripts/pjdfstest-fuse.py BIN_DIR [DIR...]

BIN_DIR holds the cladefsd and cladefsctl binaries; DIRs name pjdfstest's directories of tests
(such as chmod), all of them otherwise.
"""

import os
import re
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PINNED = ROOT / "scripts" / "lib" / "pjdfstest.sh"
KNOWN = ROOT / "scripts" / "pjdfstest-fuse-known"
# The cells' size: room for what pjdfstest makes, which is little.
CAPACITY = 1 << 30


def pinned_version():
    """The repository and commit scripts/lib/pjdfstest.sh pins, unless the environment says."""
    text = PINNED.read_text()
    found = {name: re.search(rf"{name}=\$\{{{name}:-([^}}]+)\}}", text).group(1)
             for name in ("PJDFSTEST_REPO", "PJDFSTEST_COMMIT")}
    return tuple(os.environ.get(name, value) for name, value in found.items())


def build(work):
    """pjdfstest built at its pinned version in `work`, unless it is already: its directory."""
    repo, commit = pinned_version()
    source = work / "pjdfstest"
    built = work / "pjdfstest.commit"
    if built.exists() and built.read_text() == commit and (source / "pjdfstest").exists():
        return source
    work.mkdir(parents=True, exist_ok=True)
    subprocess.run(["rm", "-rf", str(source)], check=True)
    run = lambda *args: subprocess.run(args, cwd=source, check=True)
    source.mkdir()
    run("git", "init", "-q")
    run("git", "fetch", "-q", "--depth", "1", repo, commit)
    run("git", "checkout", "-q", "FETCH_HEAD")
    run("autoreconf", "-ifs")
    run("./configure")
    run("make", "pjdfstest")
    built.write_text(commit)
    return source


def failures(prove):
    """The failures prove's summary says: `FILE:N` for each test failed, `FILE` for a file that
    failed whole, FILE relative to pjdfstest's tests directory."""
    found = set()
    lines = prove.splitlines()
    try:
        start = lines.index("Test Summary Report")
    except ValueError:
        return found
    current, listing = None, False
    for line in lines[start + 2:]:
        header = re.match(r"^\S*/tests/(\S+\.t)\s+\(Wstat:", line)
        if header:
            current, listing = header.group(1), False
            continue
        if current is None or not line.startswith(" "):
            current = None
            continue
        label = re.match(r"^\s+([A-Za-z -]+):\s*(.*)$", line)
        if label:
            listing = label.group(1) == "Failed tests"
            if listing:
                found |= numbers(current, label.group(2))
            elif label.group(1) in ("Parse errors", "Non-zero exit status", "Non-zero wait status"):
                found.add(current)
        elif listing:
            # The failed tests' list goes on.
            found |= numbers(current, line)
    return found


def numbers(file, listed):
    """`FILE:N` of each test a list of numbers and ranges such as `3, 7-9` names."""
    found = set()
    for part in filter(None, (part.strip() for part in listed.split(","))):
        first, _, last = part.partition("-")
        for number in range(int(first), int(last or first) + 1):
            found.add(f"{file}:{number}")
    return found


def known(text):
    """The failures scripts/pjdfstest-fuse-known lists, each with its task: `TASK-N FAILURE`, a
    FAILURE `FILE:N` or `FILE`, lines starting with `#` comments."""
    listed = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        task, failure = line.split()[:2]
        if not re.fullmatch(r"TASK-[0-9.]+", task):
            raise ValueError(f"{line!r}: not a task first")
        listed[failure] = task
    return listed


def unexpected(found, listed, dirs):
    """The failures `found` that `listed` doesn't list, and those listed, of the directories run,
    that weren't found: (unlisted, gone)."""
    covered = lambda failure: failure in listed or failure.split(":")[0] in listed
    unlisted = sorted(failure for failure in found if not covered(failure))
    ran = lambda failure: not dirs or failure.split("/")[0] in dirs
    gone = sorted(failure for failure in listed if ran(failure)
                  and not any(seen == failure or seen.split(":")[0] == failure for seen in found))
    return unlisted, gone


# cladefs: the master key the mounts' cells are encrypted under, the tests', known to all
# (scripts/lib/test-kms.sh), unless the environment names a KMS (TASK-411.28).
TEST_MASTER_KEY = "7e" * 32


def encrypted_env(dir):
    """The environment of a daemon in `dir`: every cell is encrypted, and the daemon refuses to
    start with no KMS (TASK-411.28). Unless the environment names one, a host's: its
    bucket a directory in `dir`, placement 1, and the bucket KMS of the tests' master key."""
    env = dict(os.environ)
    if not any(env.get(name) for name in ("CLADEFS_KMS", "CLADEFS_MASTER_KEY", "CLADEFS_MASTER_KEY_FILE")):
        bucket = Path(dir) / "bucket"
        bucket.mkdir(exist_ok=True)
        env.setdefault("CLADEFSD_BUCKET", f"file://{bucket}/")
        env.setdefault("CLADEFSD_TOKEN", "1")
        env["CLADEFS_MASTER_KEY"] = TEST_MASTER_KEY
    return env


# The daemon's memory, fixed: what pjdfstest's small files need, many times over.
MEMORY_BYTES = 1 << 30


def mounted(bin_dir, dir):
    """A daemon serving in `dir`, its FUSE mount of cells shown to other users too, which
    pjdfstest's tests become (`-u`, `-g`), and a new cell of it attached there: the daemon's
    process, the cell's directory in the mount, and the daemon's log."""
    mountpoint, cells, control = dir / "mnt", dir / "cells", dir / "control.sock"
    mountpoint.mkdir()
    cells.mkdir()
    env = encrypted_env(dir)
    env.update({"CLADEFSD_GRPC": str(control), "CLADEFSD_MOUNT": str(mountpoint),
                "CLADEFSD_MOUNT_STORES": str(cells), "CLADEFSD_MOUNT_ALLOW_OTHER": "1",
                # The disk the daemon owns, which its cells are kept on (board 14305).
                "CLADEFSD_ROOTS": str(cells)})
    log = open(dir / "cladefsd.log", "w")
    # Its memory fixed: governed, it would require the memory governor's MGLRU
    # and swap of the host, which a CI runner has not (hotbed review 170).
    process = subprocess.Popen([bin_dir / "cladefsd", str(dir / "vms.sock"), str(MEMORY_BYTES)],
                               env=env, stdout=log, stderr=subprocess.STDOUT)
    deadline = time.monotonic() + 60
    while not control.exists():
        if process.poll() is not None or time.monotonic() > deadline:
            sys.exit(f"cladefsd: not serving, {process.poll()}: {(dir / 'cladefsd.log').read_text()}")
        time.sleep(0.05)
    made = subprocess.run([bin_dir / "cladefsctl", "mount", "create", "0", str(CAPACITY)], env=env,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    if made.returncode != 0:
        process.send_signal(signal.SIGTERM)
        process.wait()
        sys.exit(f"cladefsctl mount create: {made.stderr}{(dir / 'cladefsd.log').read_text()}")
    tenants = [entry for entry in mountpoint.iterdir()]
    if len(tenants) != 1:
        sys.exit(f"the mount shows {len(tenants)} tenants, not the one cell's")
    return process, tenants[0] / made.stdout.strip(), log


def run(name, bin_dir, tests, dirs, setup, logs):
    """One run: a new cell mounted, `setup` given its directory and saying where prove runs,
    then pjdfstest; the daemon stopped, draining, which unmounts. Its failures."""
    with tempfile.TemporaryDirectory(prefix=f"pjdfstest-fuse-{name}-") as dir:
        # Searchable by the users pjdfstest's tests become, as the mount is.
        os.chmod(dir, 0o755)
        process, mountpoint, log = mounted(bin_dir, Path(dir))
        try:
            cwd = setup(mountpoint)
            paths = [str(tests / d) for d in dirs] or [str(tests)]
            proved = subprocess.run(["prove", "-r", *paths], cwd=cwd, stdout=subprocess.PIPE,
                                    stderr=subprocess.STDOUT, text=True)
        finally:
            process.send_signal(signal.SIGTERM)
            status = process.wait()
            log.close()
        (logs / f"{name}.log").write_text(proved.stdout)
        print(f"== {name}: prove exit {proved.returncode}; {logs / f'{name}.log'}")
        print("\n".join(proved.stdout.splitlines()[-4:]))
        if status != 0:
            sys.exit(f"cladefsd exited {status}: {(Path(dir) / 'cladefsd.log').read_text()}")
        found = failures(proved.stdout)
        if proved.returncode != 0 and not found:
            sys.exit(f"prove failed, {proved.returncode}, and said no test failed")
        return found


def in_a_directory(mountpoint):
    work = mountpoint / "work"
    work.mkdir()
    return work


def from_the_root(mountpoint):
    """The root chowned to another user and back, its mode kept by each chown, then chmodded."""
    for owner in (65534, 0):
        os.chown(mountpoint, owner, owner)
        mode = os.stat(mountpoint).st_mode & 0o7777
        if mode != 0o755:
            sys.exit(f"a chown of the mount's root to {owner} left it mode {mode:o}")
    os.chmod(mountpoint, 0o755)
    return mountpoint


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__.split("Usage: ")[1].split("\n")[0])
    if os.geteuid() != 0:
        sys.exit("pjdfstest-fuse.py runs as root: pjdfstest chowns, and mounts by mount(2)")
    bin_dir, dirs = Path(sys.argv[1]).resolve(), sys.argv[2:]
    work = Path(os.environ.get("CLADEFS_PJDFSTEST_WORK", ROOT / "target" / "pjdfstest")).resolve()
    tests = build(work) / "tests"
    found = set()
    for name, setup in (("in-a-directory", in_a_directory), ("from-the-root", from_the_root)):
        found |= run(name, bin_dir, tests, dirs, setup, work)
    listed = known(KNOWN.read_text())
    unlisted, gone = unexpected(found, listed, dirs)
    for failure in sorted(found - set(unlisted)):
        print(f"known: {failure} ({listed.get(failure) or listed[failure.split(':')[0]]})")
    for failure in unlisted:
        print(f"FAILED: {failure}")
    for failure in gone:
        print(f"PASSED, listed as failing ({listed[failure]}): {failure}: take it out of {KNOWN}")
    sys.exit(1 if unlisted or gone else 0)


if __name__ == "__main__":
    main()
