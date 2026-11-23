#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Says when others wrote to a measurement round's disk (TASK-375). `hotbed run --bench` gives a
measurement cores, cache, memory bandwidth and memory of its own, and caps other hotbed work's I/O
on one disk at most: the disk it does not isolate. The test host had one NVMe, its
scratch disk a zvol on the pool /home is on, and a round of TASK-330.4's A/B ran wholly beside
another agent's 186 MB/s loader with nothing in its results saying so. A round driver runs its
round under this (scripts/lib/bench-ab-linux-host.sh, scripts/lib/bench-linux-host.sh, and
scripts/bench-rounds.sh through scripts/test-e2e-job.sh, which names the round's disk, its job's
scratch directory, the results being on another; a scripts/bench.sh or scripts/qualify.sh run by
hand is wrapped the same way):

    contamination.py run RESULTS [DISK...] -- COMMAND...   runs COMMAND, a round, and exits with its
                                                           status, RESULTS/contamination.json written
    contamination.py said RESULTS                          the record, in a line

DISK is a path on each disk the round keeps its cells on (default: RESULTS, and TMPDIR if set).
The record says, over the round:

- **others' I/O**, by disk. The disk under each path is found from /proc/self/mountinfo and /sys:
  a partition's whole disk; a loop device's backing file's disk; a device mapper's or md's disks;
  a zvol; tmpfs, which has no device and nothing to record, said. Of each, what it read and wrote
  (/sys/block/DEV/stat) less what the round's cgroup did (its io.stat for the device) is others'.
  The round's cgroup is the process's own, or the nearest above it with I/O accounting, as long
  as nothing else runs in that one: under hotbed, the measurement's slice, which holds one
  measurement at a time.
- **under a zvol**, the pool's other datasets' writes, by ZFS's own counts of each
  (/proc/spl/kstat/zfs/POOL/objset-*, nwritten: what was written to the dataset's files, as it
  was written, a mapped file's pages not among them): others' too, as a loader in /home is. ZFS
  writes for no cgroup, so the round's own writes can't be followed to the pool's disks (zpool
  list): their totals are recorded (`beneath`), and not judged.
- **pressure**: how long some task of the round's cgroup stalled on CPU, memory and I/O (PSI);
  the host's where the cgroup has none.
- **the verdict**: `contaminated`, with why (which resource, how much, its bound); `clean`; or
  `unchecked`, with why, wherever others' I/O could not be told from the round's own: no cgroup
  (macOS, cgroup v1, the root cgroup, as in a container), a cgroup shared with others, a disk not
  found (an overlay, a network filesystem), a ZFS dataset as the round's own disk. Never clean
  where it could not check: the disk's whole writes, the round's own among them, are then said in
  the why, and judged by no bound.

What the round runs outside its cgroup is others' to this: scripts/test-e2e.sh's
CLADEFSD_MEMORY_MAX runs the extent daemon in a systemd scope of its own, whose writes would
mark the round. No measurement's driver sets it.

The bounds, on others' writes to the round's disks together, in MB (10^6 bytes) a second:
CLADEFS_CONTAMINATION_WRITE_MB_S over the round (default 10) and
CLADEFS_CONTAMINATION_MINUTE_WRITE_MB_S in any one minute (default 50), TASK-375's proposal. What
the test host has shown since: beside clean rounds, what the scratch zvol wrote that was not the
measurement's (ext4's journal thread, which is no cgroup's, among it) was 0.5 to 6.3 MB/s in 30 s
samples (TASK-330.5, TASK-322); ordinary work beside a measurement, capped by fxjob then, 25 to 27 MB/s
(TASK-374); the loader that went unseen, 186 MB/s. Reads are recorded and not judged: nothing
measured says what others' reads cost a round. Nor is pressure, unless
CLADEFS_CONTAMINATION_STALL_PCT names bounds, RESOURCE=PERCENT words (`io=20 cpu=5`), the share of
the round some task of it stalled: a round's own tasks stall on each other (the sandboxes' vCPUs
are overcommitted on purpose), so no bound holds for every round.

scripts/bench-ab.py leaves a contaminated round's timings out of its comparison (`skewed`: every
unit but a count's or a size's, which a load beside the round doesn't move), and says which rounds
were unchecked.
"""

import json
import os
import re
import signal
import subprocess
import sys
import time
from dataclasses import dataclass, field, replace
from pathlib import Path

RECORD = "contamination.json"
MB = 1_000_000
# The units of what a load beside a round doesn't move: sizes, ratios and counts. Any other unit
# is a time or a rate, one not named here too.
COUNTS = frozenset({"MiB", "GiB", "bytes", "x", "count"})
NO_DEVICE = frozenset({"tmpfs", "ramfs", "devtmpfs"})
ZVOL = re.compile(r"zd\d+")
RESOURCES = ("cpu", "memory", "io")


def skewed(unit):
    """Whether a load beside a round moves what is measured in `unit`: a time or a rate."""
    return unit not in COUNTS


@dataclass(frozen=True)
class Bounds:
    """Past these a round is contaminated: others' writes over the round and in any one minute,
    MB/s, and, if named, the percentage of the round its tasks stalled on each resource."""

    write: float = 10.0
    minute: float = 50.0
    stalls: tuple = ()

    @classmethod
    def of(cls, environ):
        """The bounds `environ` names, CLADEFS_CONTAMINATION_*, the defaults otherwise."""

        def rate(name, default):
            try:
                return float(environ.get(name) or default)
            except ValueError:
                sys.exit(f"{name} is MB/s, a number: {environ[name]}")

        stalls = []
        for word in environ.get("CLADEFS_CONTAMINATION_STALL_PCT", "").split():
            resource, _, percent = word.partition("=")
            if resource not in RESOURCES or not re.fullmatch(r"\d+(\.\d+)?", percent):
                sys.exit("CLADEFS_CONTAMINATION_STALL_PCT is RESOURCE=PERCENT words, RESOURCE cpu, "
                         f"memory or io: {word}")
            stalls.append((resource, float(percent)))
        return cls(rate("CLADEFS_CONTAMINATION_WRITE_MB_S", cls.write),
                   rate("CLADEFS_CONTAMINATION_MINUTE_WRITE_MB_S", cls.minute), tuple(stalls))


def zpool_list(pool):
    """`zpool list -vHPL POOL`'s output, the pool's disks by their devices' paths among it; None
    if it can't be had."""
    try:
        done = subprocess.run(["zpool", "list", "-vHPL", pool], capture_output=True, text=True)
    except OSError:
        return None
    return done.stdout if done.returncode == 0 else None


class Host:
    """The host a round runs on, as its /proc, /sys and /dev say it, under `root` (the tests'
    is a tree of their own), and its zpool."""

    def __init__(self, root="/", zpool=zpool_list):
        self.root = Path(root)
        self.zpool = zpool

    def path(self, absolute):
        return self.root / absolute.lstrip("/")

    def text(self, absolute):
        """The file's text, empty if there is none to read."""
        try:
            return self.path(absolute).read_text()
        except OSError:
            return ""

    def real(self, path):
        """`path` as the mounts name it, its links followed."""
        return os.path.realpath(path) if self.root == Path("/") else os.path.normpath(path)


@dataclass
class Layout:
    """What a round's paths are on: `devices`, the disks whose I/O less the round's own is
    others', each with its number, as io.stat names it; `pools`, the ZFS pools under them, each
    with the round's own datasets, the others' writes others'; `beneath`, the pools' disks, each
    with its pool; `said`, what there was nothing to record of; and `unchecked`, why others' I/O
    can't be told for a path."""

    devices: dict = field(default_factory=dict)
    pools: dict = field(default_factory=dict)
    beneath: dict = field(default_factory=dict)
    said: list = field(default_factory=list)
    unchecked: list = field(default_factory=list)


def find(host, paths):
    """The layout of `paths` on `host`."""
    layout = Layout()
    for path in paths:
        place(host, host.real(str(path)), layout)
    return layout


def note(notes, text):
    if text not in notes:
        notes.append(text)


def mounted(host, path):
    """The mount `path` is in, by /proc/self/mountinfo, the last of the deepest: its device's
    number, its filesystem and its source. None if no mount is found."""
    best = None
    for line in host.text("/proc/self/mountinfo").splitlines():
        mount, _, filesystem = line.partition(" - ")
        fields = mount.split()
        version = re.sub(r"\\([0-7]{3})", lambda octal: chr(int(octal[1], 8)), fields[4])
        if path == version or path.startswith(version.rstrip("/") + "/"):
            if best is None or len(version) >= len(best[0]):
                best = (version, fields[2], *filesystem.split()[:2])
    return best and best[1:]


def whole(node):
    """The disk of the block device at `node`, in /sys: a partition's, or itself."""
    return node.parent if (node / "partition").exists() else node


def place(host, path, layout):
    """Adds to `layout` what `path` is on."""
    mount = mounted(host, path)
    if mount is None:
        note(layout.unchecked, f"{path}: no mount of it in /proc/self/mountinfo, as off Linux")
        return
    number, filesystem, source = mount
    link = host.path(f"/sys/dev/block/{number}")
    if filesystem in NO_DEVICE:
        note(layout.said, f"{path}: {filesystem}, no device: nothing to record")
    elif filesystem == "zfs":
        pool(host, source, layout)
        note(layout.unchecked, f"{path}: on the ZFS dataset {source}, whose I/O is no cgroup's: "
                               "this round's writes to it can't be told from others'")
    elif not link.exists():
        note(layout.unchecked, f"{path}: on {filesystem} ({source}), no block device found")
    else:
        descend(host, whole(link.resolve()), layout)


def descend(host, node, layout):
    """Adds to `layout` the disks under the block device at `node`, in /sys: a loop device's
    backing file's, a device mapper's or md's, or itself, and under a zvol its pool."""
    backing = node / "loop" / "backing_file"
    slaves = sorted((node / "slaves").iterdir()) if (node / "slaves").is_dir() else []
    if backing.exists():
        place(host, backing.read_text().strip().removesuffix(" (deleted)"), layout)
    elif slaves:
        for slave in slaves:
            descend(host, whole(slave.resolve()), layout)
    else:
        layout.devices[node.name] = (node / "dev").read_text().strip()
        if ZVOL.fullmatch(node.name):
            zvol(host, node.name, layout)


def zvol(host, name, layout):
    """Adds to `layout` the pool of the zvol whose device is `name`, its dataset the one /dev/zvol
    links to it."""
    volumes = host.path("/dev/zvol")
    dataset = next((str(link.relative_to(volumes)) for link in sorted(volumes.rglob("*"))
                    if link.is_symlink() and Path(os.readlink(link)).name == name), None)
    if dataset is None:
        note(layout.unchecked, f"{name}: a zvol no link in /dev/zvol names the dataset of: "
                               "others' writes to its pool can't be told")
    else:
        pool(host, dataset, layout)


def pool(host, dataset, layout):
    """Adds to `layout` the pool of `dataset`, the round's own, and the pool's disks."""
    name = dataset.split("/")[0]
    known = name in layout.pools
    layout.pools.setdefault(name, set()).add(dataset)
    if known:
        return
    if not datasets(host, name):
        note(layout.unchecked, f"{name}: no counts of its datasets "
                               f"(/proc/spl/kstat/zfs/{name}/objset-*): others' writes to the "
                               "pool can't be told")
    for word in (host.zpool(name) or "").split():
        device = host.path(f"/sys/class/block/{Path(word).name}")
        if word.startswith("/dev/") and device.exists():
            layout.beneath[whole(device.resolve()).name] = name
    if name not in layout.beneath.values():
        note(layout.said, f"{name}: its disks not found (zpool list -vHPL {name}): nothing "
                          "recorded beneath it")


def datasets(host, name):
    """What was read from and written to each dataset of the pool `name`, in bytes, by ZFS's
    counts of each."""
    counts = {}
    for path in sorted(host.path(f"/proc/spl/kstat/zfs/{name}").glob("objset-*")):
        values = {fields[0]: fields[2] for fields in map(str.split, path.read_text().splitlines())
                  if len(fields) == 3}
        counts[values["dataset_name"]] = (int(values["nread"]), int(values["nwritten"]))
    return counts


@dataclass(frozen=True)
class Cgroup:
    """The round's cgroup: `own`, the process's, whose pressure is the round's; `io`, the one
    whose io.stat is the round's I/O, `own` or the nearest above it that has one; `why`, why
    there is none."""

    own: Path = None
    io: Path = None
    name: str = None
    why: str = ""

    def shared(self):
        """Who else runs in `io`, if it is above `own`: then its I/O is not the round's alone."""
        child = self.own
        while child != self.io:
            for other in sorted(child.parent.iterdir()):
                events = other / "cgroup.events"
                if other != child and events.exists() and "populated 1" in events.read_text():
                    return (f"{self.name} has the nearest I/O accounting to the round's cgroup, "
                            f"and holds others' work too ({other.name})")
            child = child.parent
        return ""


def cgroup(host):
    """The cgroup of this process on `host`, a round's."""
    text = host.text("/proc/self/cgroup")
    if not text:
        return Cgroup(why="no /proc/self/cgroup: not Linux")
    name = next((line[3:] for line in text.splitlines() if line.startswith("0::")), None)
    if name is None:
        return Cgroup(why="cgroup v1: no cgroup's io.stat is the round's own")
    if name == "/":
        return Cgroup(why="it runs in the root cgroup, with everything else")
    top = host.path("/sys/fs/cgroup")
    own = io = host.path("/sys/fs/cgroup" + name)
    while io != top and not (io / "io.stat").exists():
        io = io.parent
    if io == top:
        return Cgroup(own=own, why=f"no cgroup of its has I/O accounting (io.stat): {name}")
    return Cgroup(own, io, "/" + str(io.relative_to(top)))


@dataclass(frozen=True)
class Sample:
    """What a host has counted by the time `at`, in seconds: what each disk read and wrote, in
    bytes (`devices`, by name); of that, the round's cgroup's (`own`, by the device's number),
    None where `unknown` says why it can't be told; each pool's datasets' (`datasets`); and how
    long some task of the round stalled on each resource, in µs (`stalled`, `stalled_of` the
    cgroup's or the host's)."""

    at: float
    devices: dict
    own: dict = None
    unknown: str = ""
    datasets: dict = field(default_factory=dict)
    stalled: dict = field(default_factory=dict)
    stalled_of: str = None


def sample(host, layout, group, at):
    """A sample of `host` at `at`, of `layout`'s disks and pools and of the cgroup `group`."""
    devices = {}
    for name in (*layout.devices, *layout.beneath):
        fields = host.path(f"/sys/class/block/{name}/stat").read_text().split()
        devices[name] = (int(fields[2]) * 512, int(fields[6]) * 512)
    own, unknown = None, group.why or group.shared()
    if not unknown:
        own = {}
        for line in (group.io / "io.stat").read_text().splitlines():
            number, *counts = line.split()
            counts = dict(count.split("=") for count in counts)
            own[number] = (int(counts.get("rbytes", 0)), int(counts.get("wbytes", 0)))
    stalled, stalled_of = {}, None
    for resource in RESOURCES:
        of, text = "cgroup", ""
        if group.own is not None and (group.own / f"{resource}.pressure").exists():
            text = (group.own / f"{resource}.pressure").read_text()
        if not text:
            of, text = "host", host.text(f"/proc/pressure/{resource}")
        some = re.search(r"^some .*total=(\d+)", text, re.MULTILINE)
        if some:
            stalled[resource], stalled_of = int(some[1]), of
    return Sample(at, devices, own, unknown, {name: datasets(host, name) for name in layout.pools},
                  stalled, stalled_of)


def others(before, after, layout):
    """What others did to `layout`'s disks between two samples: the bytes they read, and the
    bytes they wrote by disk and dataset. A disk's I/O less the round's own; where its own is
    unknown, the disk's whole. A pool's datasets' writes but the round's own datasets', a dataset
    mounted since counted from nothing."""
    known = before.own is not None and after.own is not None
    read, written = 0, {}
    for name, number in layout.devices.items():
        did = [now - then for now, then in zip(after.devices[name], before.devices[name])]
        if known:
            own = zip(after.own.get(number, (0, 0)), before.own.get(number, (0, 0)))
            did = [max(0, all_ - (now - then)) for all_, (now, then) in zip(did, own)]
        read += did[0]
        written[name] = did[1]
    for name, mine in layout.pools.items():
        for dataset, (_, now) in after.datasets[name].items():
            if dataset not in mine:
                then = before.datasets[name].get(dataset, (0, 0))[1]
                written[dataset] = now - then if now >= then else now
    return read, written


def minutes(samples):
    """The windows between `samples`, a minute each as `run` takes them, each (before, after);
    the last, cut short by the round's end, joined to the one before if under half its length: a
    burst in a few seconds is no minute's rate."""
    windows = list(zip(samples, samples[1:]))
    if len(windows) > 1:
        (before, middle), (_, after) = windows[-2:]
        if after.at - middle.at < (middle.at - before.at) / 2:
            windows[-2:] = [(before, after)]
    return windows


def judge(samples, layout, bounds, group=Cgroup()):
    """The record of a round whose samples are `samples`, the first as it started, the last as
    it ended."""
    # One sample without the round's own I/O, and no difference of it holds; with no disk under
    # the round, there is none to tell.
    unknown = next((each.unknown for each in samples if each.unknown), "") if layout.devices else ""
    if unknown:
        samples = [replace(each, own=None) for each in samples]
    first, last = samples[0], samples[-1]
    seconds = last.at - first.at
    read, written = others(first, last, layout)
    contaminated, unchecked = [], list(layout.unchecked)
    rate = worst = worst_at = 0.0
    if seconds <= 0:
        unchecked.append("the round took no time")
    else:
        rate = sum(written.values()) / seconds / MB
        for before, after in minutes(samples):
            window = sum(others(before, after, layout)[1].values()) / (after.at - before.at) / MB
            if window > worst:
                worst, worst_at = window, before.at - first.at
        if unknown:
            unchecked.append(f"{unknown}: others' writes can't be told from its own; its disk's "
                             f"whole writes were {rate:.1f} MB/s over the round, {worst:.1f} in "
                             "its worst minute")
        else:
            most = sorted(written.items(), key=lambda item: -item[1])[:3]
            by = ", ".join(f"{name} {count / seconds / MB:.1f}" for name, count in most)
            if rate > bounds.write:
                contaminated.append(f"others wrote {rate:.1f} MB/s to its disk over the round, "
                                    f"bound {bounds.write:g} ({by})")
            if worst > bounds.minute:
                contaminated.append(f"others wrote {worst:.1f} MB/s to its disk in the minute "
                                    f"{worst_at:.0f} s in, bound {bounds.minute:g}")
        for resource, percent in bounds.stalls:
            if resource in first.stalled and resource in last.stalled:
                share = (last.stalled[resource] - first.stalled[resource]) / 1e4 / seconds
                if share > percent:
                    contaminated.append(f"its tasks stalled on {resource} {share:.1f}% of the "
                                        f"round (the {last.stalled_of}'s pressure), bound "
                                        f"{percent:g}%")
    if contaminated:
        verdict, why = "contaminated", contaminated
    elif unchecked:
        verdict, why = "unchecked", unchecked
    else:
        verdict, why = "clean", []
    devices = {}
    for name in (*layout.devices, *layout.beneath):
        did = [now - then for now, then in zip(last.devices[name], first.devices[name])]
        devices[name] = {"read_bytes": did[0], "written_bytes": did[1]}
        if name in layout.beneath:
            devices[name]["beneath"] = layout.beneath[name]
        elif not unknown:
            number = layout.devices[name]
            own = zip(last.own.get(number, (0, 0)), first.own.get(number, (0, 0)))
            devices[name]["own_read_bytes"], devices[name]["own_written_bytes"] = (
                now - then for now, then in own)
    pressure = {"of": last.stalled_of}
    for resource in RESOURCES:
        if resource in first.stalled and resource in last.stalled:
            pressure[f"{resource}_ms"] = (last.stalled[resource] - first.stalled[resource]) // 1000
    return {
        "verdict": verdict,
        "why": why,
        "seconds": round(seconds, 1),
        "others": {"read_bytes": read, "written_bytes": sum(written.values()),
                   "write_mb_s": round(rate, 3), "worst_minute_write_mb_s": round(worst, 3),
                   "written_by": written},
        "devices": devices,
        "pressure": pressure,
        "cgroup": None if unknown or not layout.devices else group.name,
        "said": layout.said,
        "bounds": {"write_mb_s": bounds.write, "minute_write_mb_s": bounds.minute,
                   "stall_pct": dict(bounds.stalls)},
    }


def load(directory):
    """The record of the round whose results are in `directory`: one that left none was not
    checked."""
    path = Path(directory) / RECORD
    if not path.exists():
        return {"verdict": "unchecked", "why": [f"no record of others' I/O beside it ({RECORD})"]}
    return json.loads(path.read_text())


def said(record):
    """`record` in a line."""
    if record["verdict"] != "clean":
        return f"{record['verdict'].upper()}: {'; '.join(record['why'])}"
    of, bounds = record["others"], record["bounds"]
    return (f"clean: others wrote {of['write_mb_s']:.1f} MB/s to its disk over "
            f"{record['seconds']:.0f} s, {of['worst_minute_write_mb_s']:.1f} in its worst minute "
            f"(bounds {bounds['write_mb_s']:g} and {bounds['minute_write_mb_s']:g})")


def run(results, disks, command, host, bounds, every=60.0, clock=time.monotonic):
    """Runs `command`, a round, sampling `host` as it starts, every `every` seconds and as it
    ends, and writes the round's record in `results`; returns the command's status. A record that
    can't be made is an unchecked round, saying why: the round's results are still its results."""
    results = Path(results)
    results.mkdir(parents=True, exist_ok=True)
    failed, samples, layout, group = "", [], Layout(), Cgroup()

    def take():
        nonlocal failed
        if not failed:
            try:
                samples.append(sample(host, layout, group, clock()))
            except (OSError, ValueError, KeyError, IndexError) as error:
                failed = f"recording failed: {type(error).__name__}: {error}"

    try:
        layout, group = find(host, disks), cgroup(host)
    except (OSError, ValueError, KeyError, IndexError) as error:
        failed = f"finding its disks failed: {type(error).__name__}: {error}"
    take()
    try:
        child = subprocess.Popen(command)
    except OSError as error:
        sys.exit(f"contamination.py: {command[0]}: {error.strerror}")
    # A signal for the round is the round's: it ends, and its record is still written.
    for number in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(number, lambda received, _: child.send_signal(received))
    due = clock() + every
    while (status := wait(child, due - clock())) is None:
        take()
        due += every
    take()
    record = {"verdict": "unchecked", "why": [failed]} if failed else judge(
        samples, layout, bounds, group)
    (results / RECORD).write_text(json.dumps(record, indent=1) + "\n")
    print(f"contamination: {said(record)}", file=sys.stderr)
    return status if status >= 0 else 128 - status


def wait(child, seconds):
    """`child`'s status once it ends, None if it still runs after `seconds`."""
    try:
        return child.wait(timeout=max(0.0, seconds))
    except subprocess.TimeoutExpired:
        return None


USAGE = "usage: contamination.py run RESULTS [DISK...] -- COMMAND... | said RESULTS"


def main(argv, environ=os.environ):
    if argv[:1] == ["said"] and len(argv) == 2:
        print(said(load(argv[1])))
        return 0
    if argv[:1] != ["run"] or "--" not in argv or argv.index("--") < 2 or argv[-1] == "--":
        sys.exit(USAGE)
    cut = argv.index("--")
    results, disks, command = argv[1], argv[2:cut], argv[cut + 1:]
    if not disks:
        disks = [results] + ([environ["TMPDIR"]] if environ.get("TMPDIR") else [])
    return run(results, disks, command, Host(), Bounds.of(environ))


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
