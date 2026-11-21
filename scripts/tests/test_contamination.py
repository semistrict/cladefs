# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/contamination.py (TASK-375): the disks it finds under a round's paths, on
hosts built by fake_sysfs.py; others' I/O and the verdict from samples of a disk and a cgroup; a
round run under it; and that the round drivers run their rounds under it."""

import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import fake_sysfs
from fake_sysfs import contamination
from script import SCRIPTS

Bounds, Layout, Sample = contamination.Bounds, contamination.Layout, contamination.Sample
GB = 1_000_000_000
MB = 1_000_000


def layout_of(tree, *paths):
    found = contamination.find(tree.host(), paths)
    return found.devices, found.pools, found.beneath, found.said, found.unchecked


class Disks(unittest.TestCase):
    """The disk under a round's path, by /proc/self/mountinfo and /sys."""

    def setUp(self):
        self.tree = fake_sysfs.Tree()

    def test_a_partitions_disk_is_the_whole_disk(self):
        self.tree.disk("nvme0n1", "259:0")
        self.tree.partition("nvme0n1", "nvme0n1p2", "259:2")
        self.tree.mount("/", "259:2", "ext4", "/dev/nvme0n1p2")
        self.assertEqual(layout_of(self.tree, "/var/results/r1"),
                         ({"nvme0n1": "259:0"}, {}, {}, [], []))

    def test_the_deepest_mount_is_the_paths_whatever_the_mounts_order(self):
        self.tree.disk("sda", "8:0")
        self.tree.disk("sdb", "8:16")
        self.tree.disk("sdc", "8:32")
        self.tree.mount("/mnt/scratch", "8:16", "ext4", "/dev/sdb")
        self.tree.mount("/", "8:0", "ext4", "/dev/sda")
        self.tree.mount("/mnt/scratchy", "8:0", "ext4", "/dev/sda")
        self.assertEqual(layout_of(self.tree, "/mnt/scratch/run/r1"),
                         ({"sdb": "8:16"}, {}, {}, [], []))
        self.assertEqual(layout_of(self.tree, "/mnt/scratch"), ({"sdb": "8:16"}, {}, {}, [], []))
        self.assertEqual(layout_of(self.tree, "/mnt"), ({"sda": "8:0"}, {}, {}, [], []))
        # A mount over another at the same place hides it.
        self.tree.mount("/mnt/scratch", "8:32", "ext4", "/dev/sdc")
        self.assertEqual(layout_of(self.tree, "/mnt/scratch/run/r1"),
                         ({"sdc": "8:32"}, {}, {}, [], []))

    def test_a_zvols_pool_and_the_pools_disk_are_found(self):
        """host1: the scratch disk a zvol on the pool /home is on, the pool on the one NVMe."""
        self.tree.zvol("zd0", "230:0", "tank/vol/scratch")
        self.tree.disk("nvme0n1", "259:0")
        self.tree.partition("nvme0n1", "nvme0n1p3", "259:3")
        self.tree.mount("/mnt/fxdax-scratch", "230:0", "ext4", "/dev/zd0")
        self.tree.dataset("0x36", "tank/vol/scratch", 0, 0)
        self.tree.dataset("0x90", "tank/home", 0, 0)
        self.tree.pool_disks("tank", "/dev/nvme0n1p3")
        self.assertEqual(layout_of(self.tree, "/mnt/fxdax-scratch/fxdax-host/run-1/results"),
                         ({"zd0": "230:0"}, {"tank": {"tank/vol/scratch"}}, {"nvme0n1": "tank"},
                          [], []))

    def test_a_zvol_whose_pool_says_nothing_is_unchecked(self):
        self.tree.zvol("zd16", "230:16", "tank/scratch")
        self.tree.mount("/mnt/scratch", "230:16", "ext4", "/dev/zd16")
        self.assertEqual(layout_of(self.tree, "/mnt/scratch"), (
            {"zd16": "230:16"}, {"tank": {"tank/scratch"}}, {},
            ["tank: its disks not found (zpool list -vHPL tank): nothing recorded beneath it"],
            ["tank: no counts of its datasets (/proc/spl/kstat/zfs/tank/objset-*): others' "
             "writes to the pool can't be told"]))

    def test_a_zvol_no_dataset_is_named_for_is_unchecked(self):
        self.tree.disk("zd0", "230:0")
        self.tree.mount("/mnt/scratch", "230:0", "ext4", "/dev/zd0")
        (self.tree.root / "dev/zvol").mkdir()
        self.assertEqual(layout_of(self.tree, "/mnt/scratch"), (
            {"zd0": "230:0"}, {}, {}, [],
            ["zd0: a zvol no link in /dev/zvol names the dataset of: others' writes to its pool "
             "can't be told"]))

    def test_tmpfs_has_no_device_and_says_so(self):
        self.tree.mount("/dev/shm", "0:26", "tmpfs", "tmpfs")
        self.assertEqual(layout_of(self.tree, "/dev/shm/bucket"), (
            {}, {}, {}, ["/dev/shm/bucket: tmpfs, no device: nothing to record"], []))

    def test_a_loop_devices_disk_is_its_backing_files(self):
        self.tree.disk("sda", "8:0")
        self.tree.partition("sda", "sda1", "8:1")
        self.tree.mount("/", "8:1", "ext4", "/dev/sda1")
        self.tree.loop("loop3", "7:3", "/var/images/cell.img")
        self.tree.mount("/mnt/cell", "7:3", "ext4", "/dev/loop3")
        self.assertEqual(layout_of(self.tree, "/mnt/cell/r1"), ({"sda": "8:0"}, {}, {}, [], []))

    def test_a_loop_device_on_tmpfs_has_no_device(self):
        self.tree.mount("/dev/shm", "0:26", "tmpfs", "tmpfs")
        self.tree.loop("loop0", "7:0", "/dev/shm/cell.img (deleted)")
        self.tree.mount("/mnt/cell", "7:0", "ext4", "/dev/loop0")
        self.assertEqual(layout_of(self.tree, "/mnt/cell"), (
            {}, {}, {}, ["/dev/shm/cell.img: tmpfs, no device: nothing to record"], []))

    def test_a_mapped_devices_disks_are_those_under_it(self):
        self.tree.disk("nvme0n1", "259:0")
        self.tree.partition("nvme0n1", "nvme0n1p3", "259:3")
        self.tree.disk("sdb", "8:16")
        self.tree.over("dm-0", "253:0", "nvme0n1p3", "sdb")
        self.tree.mount("/", "253:0", "ext4", "/dev/mapper/vg-root")
        self.assertEqual(layout_of(self.tree, "/results"),
                         ({"nvme0n1": "259:0", "sdb": "8:16"}, {}, {}, [], []))

    def test_a_zfs_dataset_as_the_rounds_disk_is_unchecked(self):
        """ZFS writes for no cgroup: the round's writes to a dataset others write to can't be
        told from theirs. The pool's other datasets' writes are still others'."""
        self.tree.mount("/home", "0:45", "zfs", "tank/home")
        self.tree.dataset("0x90", "tank/home", 0, 0)
        self.tree.pool_disks("tank", "/dev/nvme0n1p3")
        self.tree.disk("nvme0n1", "259:0")
        self.tree.partition("nvme0n1", "nvme0n1p3", "259:3")
        self.assertEqual(layout_of(self.tree, "/home/me/results"), (
            {}, {"tank": {"tank/home"}}, {"nvme0n1": "tank"}, [],
            ["/home/me/results: on the ZFS dataset tank/home, whose I/O is no cgroup's: this "
             "round's writes to it can't be told from others'"]))

    def test_a_filesystem_on_no_block_device_is_unchecked(self):
        self.tree.mount("/", "0:52", "overlay", "overlay")
        self.assertEqual(layout_of(self.tree, "/results"), (
            {}, {}, {}, [], ["/results: on overlay (overlay), no block device found"]))

    def test_a_host_with_no_mounts_to_read_is_unchecked(self):
        """macOS, for the scripts' local parts."""
        self.assertEqual(layout_of(self.tree, "/Users/me/results"), (
            {}, {}, {}, [],
            ["/Users/me/results: no mount of it in /proc/self/mountinfo, as off Linux"]))

    def test_two_paths_on_one_disk_are_one_disk(self):
        self.tree.disk("sda", "8:0")
        self.tree.mount("/", "8:0", "ext4", "/dev/sda")
        self.tree.mount("/dev/shm", "0:26", "tmpfs", "tmpfs")
        self.assertEqual(layout_of(self.tree, "/results", "/tmp/job", "/dev/shm"), (
            {"sda": "8:0"}, {}, {}, ["/dev/shm: tmpfs, no device: nothing to record"], []))


class Cgroups(unittest.TestCase):
    """The cgroup whose io.stat is the round's own I/O."""

    def setUp(self):
        self.tree = fake_sysfs.Tree()
        self.top = self.tree.root / "sys/fs/cgroup"

    def found(self):
        group = contamination.cgroup(self.tree.host())
        return group.own, group.io, group.name, group.why or group.shared()

    def test_the_processs_own_if_it_accounts_io(self):
        self.tree.cgroup("/job.scope", io={})
        self.tree.runs_in("0::/job.scope\n")
        self.assertEqual(self.found(),
                         (self.top / "job.scope", self.top / "job.scope", "/job.scope", ""))

    def test_the_nearest_above_that_accounts_io(self):
        """Under fxjob --bench: the job's scope has no io.stat, the measurements' slice has, and
        holds one measurement at a time."""
        self.tree.cgroup("/fxdax.slice", io={})
        self.tree.cgroup("/fxdax.slice/fxdax-bench.slice", io={})
        self.tree.cgroup("/fxdax.slice/fxdax-work.slice", io={})
        self.tree.cgroup("/fxdax.slice/fxdax-bench.slice/fxjob-r1-7.scope")
        self.tree.cgroup("/fxdax.slice/fxdax-bench.slice/fxjob-r0-3.scope", populated=False)
        self.tree.runs_in("0::/fxdax.slice/fxdax-bench.slice/fxjob-r1-7.scope\n")
        bench = self.top / "fxdax.slice/fxdax-bench.slice"
        self.assertEqual(self.found(), (bench / "fxjob-r1-7.scope", bench,
                                        "/fxdax.slice/fxdax-bench.slice", ""))

    def test_one_shared_with_others_work_is_not_the_rounds(self):
        self.tree.cgroup("/user.slice", io={})
        self.tree.cgroup("/user.slice/session-4.scope")
        self.tree.cgroup("/user.slice/session-9.scope")
        self.tree.runs_in("0::/user.slice/session-4.scope\n")
        self.assertEqual(self.found(), (
            self.top / "user.slice/session-4.scope", self.top / "user.slice", "/user.slice",
            "/user.slice has the nearest I/O accounting to the round's cgroup, and holds others' "
            "work too (session-9.scope)"))

    def test_none_that_accounts_io(self):
        self.tree.cgroup("/user.slice/session-4.scope")
        self.tree.runs_in("0::/user.slice/session-4.scope\n")
        self.assertEqual(self.found(), (
            self.top / "user.slice/session-4.scope", None, None,
            "no cgroup of its has I/O accounting (io.stat): /user.slice/session-4.scope"))

    def test_the_root_cgroup_cgroup_v1_and_no_cgroups_at_all(self):
        self.assertEqual(self.found(), (None, None, None, "no /proc/self/cgroup: not Linux"))
        self.tree.runs_in("0::/\n")
        self.assertEqual(self.found(), (None, None, None,
                                        "it runs in the root cgroup, with everything else"))
        self.tree.runs_in("12:blkio:/user.slice\n11:memory:/user.slice\n")
        self.assertEqual(self.found(), (None, None, None,
                                        "cgroup v1: no cgroup's io.stat is the round's own"))


def disk(*samples, own=True):
    """Samples of one disk, nvme0n1 (259:0), each (at, read, written, own read, own written)."""
    return [Sample(at, {"nvme0n1": (read, written)},
                   {"259:0": (own_read, own_written)} if own else None,
                   "" if own else "cgroup v1: no cgroup's io.stat is the round's own")
            for at, read, written, own_read, own_written in samples]


ONE_DISK = Layout(devices={"nvme0n1": "259:0"})
BOUNDS = {"write_mb_s": 10.0, "minute_write_mb_s": 50.0, "stall_pct": {}}


class Accounting(unittest.TestCase):
    """Others' I/O is the disk's less the round's cgroup's; the verdict against the bounds."""

    def test_a_round_alone_is_clean_and_its_record_whole(self):
        samples = disk((100.0, 5 * GB, 7 * GB, 1 * GB, 2 * GB),
                       (700.0, 8 * GB, 13 * GB, 3 * GB, 7400 * MB))
        self.assertEqual(contamination.judge(samples, ONE_DISK, Bounds()), {
            "verdict": "clean",
            "why": [],
            "seconds": 600.0,
            "others": {"read_bytes": 1 * GB, "written_bytes": 600 * MB, "write_mb_s": 1.0,
                       "worst_minute_write_mb_s": 1.0, "written_by": {"nvme0n1": 600 * MB}},
            "devices": {"nvme0n1": {"read_bytes": 3 * GB, "written_bytes": 6 * GB,
                                    "own_read_bytes": 2 * GB, "own_written_bytes": 5400 * MB}},
            "pressure": {"of": None},
            "cgroup": None,
            "said": [],
            "bounds": BOUNDS,
        })

    def test_others_writes_past_the_rounds_bound_contaminate_it(self):
        """TASK-330.4's round: a loader's 186 MB/s beside it."""
        samples = disk((0.0, 0, 0, 0, 0), (600.0, 0, 120 * GB, 0, 8400 * MB))
        record = contamination.judge(samples, ONE_DISK, Bounds())
        self.assertEqual((record["verdict"], record["why"]), ("contaminated", [
            "others wrote 186.0 MB/s to its disk over the round, bound 10 (nvme0n1 186.0)",
            "others wrote 186.0 MB/s to its disk in the minute 0 s in, bound 50"]))
        self.assertEqual(record["others"]["written_bytes"], 111_600 * MB)

    def test_the_bound_itself_is_within_it(self):
        samples = disk((0.0, 0, 0, 0, 0), (600.0, 0, 9 * GB, 0, 3 * GB))
        record = contamination.judge(samples, ONE_DISK, Bounds())
        self.assertEqual((record["verdict"], record["others"]["write_mb_s"]), ("clean", 10.0))
        samples = disk((0.0, 0, 0, 0, 0), (600.0, 0, 9 * GB + 600 * MB, 0, 3 * GB))
        record = contamination.judge(samples, ONE_DISK, Bounds())
        self.assertEqual((record["verdict"], record["others"]["write_mb_s"]),
                         ("contaminated", 11.0))

    def test_one_minute_of_others_writes_contaminates_a_round_within_its_bound(self):
        """4.2 GB of others' in the third of ten minutes: 7 MB/s over the round, 70 that minute."""
        samples = disk(*((60.0 * minute, 0, 600 * MB * minute + (4200 * MB if minute > 2 else 0),
                          0, 600 * MB * minute) for minute in range(11)))
        record = contamination.judge(samples, ONE_DISK, Bounds())
        self.assertEqual((record["verdict"], record["why"]), ("contaminated", [
            "others wrote 70.0 MB/s to its disk in the minute 120 s in, bound 50"]))
        self.assertEqual((record["others"]["write_mb_s"],
                          record["others"]["worst_minute_write_mb_s"]), (7.0, 70.0))

    def test_a_rounds_last_seconds_are_no_minute_of_their_own(self):
        """600 MB of others' in the 5 s a round ran past its last sample is 120 MB/s of those 5 s,
        and 9.2 MB/s of the minute they end."""
        samples = disk((0.0, 0, 0, 0, 0), (60.0, 0, 0, 0, 0), (120.0, 0, 0, 0, 0),
                       (125.0, 0, 600 * MB, 0, 0))
        record = contamination.judge(samples, ONE_DISK, Bounds())
        self.assertEqual((record["verdict"], record["others"]["write_mb_s"],
                          record["others"]["worst_minute_write_mb_s"]), ("clean", 4.8, 9.231))
        # Half a minute and more is a window of its own.
        samples = disk((0.0, 0, 0, 0, 0), (60.0, 0, 0, 0, 0), (90.0, 0, 1800 * MB, 0, 0))
        record = contamination.judge(samples, ONE_DISK, Bounds(write=100))
        self.assertEqual((record["verdict"], record["why"]), ("contaminated", [
            "others wrote 60.0 MB/s to its disk in the minute 60 s in, bound 50"]))

    def test_the_rounds_own_io_is_never_others(self):
        """What the cgroup counts past the disk's own count (a device above it) is no one's."""
        samples = disk((0.0, 0, 0, 0, 0), (100.0, 1 * GB, 1 * GB, 2 * GB, 3 * GB))
        record = contamination.judge(samples, ONE_DISK, Bounds())
        self.assertEqual((record["verdict"], record["others"]["read_bytes"],
                          record["others"]["written_bytes"]), ("clean", 0, 0))

    def test_a_disk_the_cgroup_never_touched_is_all_others(self):
        samples = [Sample(0.0, {"nvme0n1": (0, 0)}, {}),
                   Sample(100.0, {"nvme0n1": (0, 2 * GB)}, {"8:0": (0, 2 * GB)})]
        record = contamination.judge(samples, ONE_DISK, Bounds())
        self.assertEqual((record["verdict"], record["why"]), ("contaminated", [
            "others wrote 20.0 MB/s to its disk over the round, bound 10 (nvme0n1 20.0)"]))

    def test_the_pools_other_datasets_writes_are_others(self):
        """host1: a loader in /home, on the pool the scratch zvol is on, never passes the zvol's
        own device; ZFS counts it for /home's dataset."""
        layout = Layout(devices={"zd0": "230:0"}, pools={"tank": {"tank/scratch"}},
                        beneath={"nvme0n1": "tank"})

        def at(time, zvol, own, scratch, home, var, nvme):
            return Sample(time, {"zd0": (0, zvol), "nvme0n1": (0, nvme)}, {"230:0": (0, own)}, "",
                          {"tank": {"tank/scratch": (0, scratch), "tank/home": (0, home),
                                    "tank/var": (0, var)}})

        samples = [at(0.0, 1 * GB, 1 * GB, 1 * GB, 50 * GB, 3 * GB, 70 * GB),
                   at(300.0, 10 * GB, 9700 * MB, 10 * GB, 105_800 * MB, 3030 * MB, 140 * GB)]
        record = contamination.judge(samples, layout, Bounds())
        self.assertEqual((record["verdict"], record["why"]), ("contaminated", [
            "others wrote 187.1 MB/s to its disk over the round, bound 10 (tank/home 186.0, "
            "zd0 1.0, tank/var 0.1)",
            "others wrote 187.1 MB/s to its disk in the minute 0 s in, bound 50"]))
        self.assertEqual(record["others"]["written_by"],
                         {"zd0": 300 * MB, "tank/home": 55_800 * MB, "tank/var": 30 * MB})
        self.assertEqual(record["devices"], {
            "zd0": {"read_bytes": 0, "written_bytes": 9 * GB, "own_read_bytes": 0,
                    "own_written_bytes": 8700 * MB},
            "nvme0n1": {"read_bytes": 0, "written_bytes": 70 * GB, "beneath": "tank"}})
        # Without the loader, the same round is clean.
        samples[1] = at(300.0, 10 * GB, 9700 * MB, 10 * GB, 50 * GB, 3030 * MB, 80 * GB)
        record = contamination.judge(samples, layout, Bounds())
        self.assertEqual((record["verdict"], record["others"]["write_mb_s"]), ("clean", 1.1))

    def test_a_dataset_mounted_anew_counts_from_nothing(self):
        layout = Layout(devices={"zd0": "230:0"}, pools={"tank": {"tank/scratch"}})
        samples = [
            Sample(0.0, {"zd0": (0, 0)}, {}, "", {"tank": {"tank/home": (0, 9 * GB)}}),
            Sample(100.0, {"zd0": (0, 0)}, {}, "",
                   {"tank": {"tank/home": (0, 500 * MB), "tank/new": (0, 300 * MB)}})]
        record = contamination.judge(samples, layout, Bounds())
        self.assertEqual((record["verdict"], record["others"]["written_by"]),
                         ("clean", {"zd0": 0, "tank/home": 500 * MB, "tank/new": 300 * MB}))

    def test_without_the_rounds_own_io_it_is_never_clean(self):
        """cgroup v1, a GitHub runner's container, macOS: the disk's writes are whoever's."""
        samples = disk((0.0, 0, 0, 0, 0), (600.0, 0, 120 * MB, 0, 0), own=False)
        record = contamination.judge(samples, ONE_DISK, Bounds())
        self.assertEqual((record["verdict"], record["why"]), ("unchecked", [
            "cgroup v1: no cgroup's io.stat is the round's own: others' writes can't be told "
            "from its own; its disk's whole writes were 0.2 MB/s over the round, 0.2 in its "
            "worst minute"]))
        self.assertEqual(record["devices"],
                         {"nvme0n1": {"read_bytes": 0, "written_bytes": 120 * MB}})

    def test_one_sample_without_the_rounds_own_io_and_none_holds(self):
        """Another job started in the round's cgroup's slice midway."""
        samples = disk((0.0, 0, 0, 0, 0), (60.0, 0, 6 * GB, 0, 6 * GB), (120.0, 0, 12 * GB, 0, 12 * GB))
        samples[1] = Sample(60.0, {"nvme0n1": (0, 6 * GB)}, None, "/s holds others' work too (j)")
        record = contamination.judge(samples, ONE_DISK, Bounds())
        self.assertEqual((record["verdict"], record["why"]), ("unchecked", [
            "/s holds others' work too (j): others' writes can't be told from its own; its "
            "disk's whole writes were 100.0 MB/s over the round, 100.0 in its worst minute"]))

    def test_a_disk_not_found_is_unchecked_whatever_was_measured(self):
        layout = Layout(devices={"nvme0n1": "259:0"}, unchecked=["/home/r: on the ZFS dataset"])
        samples = disk((0.0, 0, 0, 0, 0), (600.0, 0, 6 * GB, 0, 6 * GB))
        record = contamination.judge(samples, layout, Bounds())
        self.assertEqual((record["verdict"], record["why"]),
                         ("unchecked", ["/home/r: on the ZFS dataset"]))
        # What was measured past a bound still contaminates it.
        samples = disk((0.0, 0, 0, 0, 0), (600.0, 0, 18 * GB, 0, 6 * GB))
        record = contamination.judge(samples, layout, Bounds())
        self.assertEqual((record["verdict"], record["why"]), ("contaminated", [
            "others wrote 20.0 MB/s to its disk over the round, bound 10 (nvme0n1 20.0)"]))

    def test_no_disk_under_the_round_needs_no_cgroup(self):
        """All of it on tmpfs: nothing others could write to."""
        layout = Layout(said=["/dev/shm/r: tmpfs, no device: nothing to record"])
        samples = [Sample(0.0, {}, None, "cgroup v1"), Sample(60.0, {}, None, "cgroup v1")]
        record = contamination.judge(samples, layout, Bounds())
        self.assertEqual((record["verdict"], record["why"], record["said"], record["devices"]),
                         ("clean", [], ["/dev/shm/r: tmpfs, no device: nothing to record"], {}))

    def test_a_round_of_no_time_is_unchecked(self):
        samples = disk((5.0, 0, 0, 0, 0), (5.0, 0, 0, 0, 0))
        record = contamination.judge(samples, ONE_DISK, Bounds())
        self.assertEqual((record["verdict"], record["why"]),
                         ("unchecked", ["the round took no time"]))

    def test_pressure_is_recorded_and_judged_only_against_a_bound_named(self):
        def at(time, cpu, memory, io_):
            return Sample(time, {"nvme0n1": (0, 0)}, {}, "", {},
                          {"cpu": cpu, "memory": memory, "io": io_}, "cgroup")

        samples = [at(0.0, 1_000_000, 0, 500_000), at(100.0, 31_000_000, 0, 25_500_999)]
        record = contamination.judge(samples, ONE_DISK, Bounds())
        self.assertEqual((record["verdict"], record["pressure"]), (
            "clean", {"of": "cgroup", "cpu_ms": 30_000, "memory_ms": 0, "io_ms": 25_000}))
        bounds = Bounds(stalls=(("io", 20.0), ("cpu", 30.0)))
        record = contamination.judge(samples, ONE_DISK, bounds)
        self.assertEqual((record["verdict"], record["why"], record["bounds"]["stall_pct"]), (
            "contaminated",
            ["its tasks stalled on io 25.0% of the round (the cgroup's pressure), bound 20%"],
            {"io": 20.0, "cpu": 30.0}))

    def test_what_a_load_beside_a_round_skews(self):
        self.assertEqual([unit for unit in ("s", "ms", "IOPS", "MiB/s", "tps", "ops/s", "MiB",
                                            "GiB", "bytes", "x", "count", "furlongs")
                          if contamination.skewed(unit)],
                         ["s", "ms", "IOPS", "MiB/s", "tps", "ops/s", "furlongs"])


class Settings(unittest.TestCase):
    def test_the_bounds_are_the_tasks_unless_the_environment_names_others(self):
        self.assertEqual(Bounds.of({}), Bounds(10.0, 50.0, ()))
        self.assertEqual(
            Bounds.of({"CLADEFS_CONTAMINATION_WRITE_MB_S": "2.5",
                       "CLADEFS_CONTAMINATION_MINUTE_WRITE_MB_S": "32",
                       "CLADEFS_CONTAMINATION_STALL_PCT": "io=20 cpu=7.5"}),
            Bounds(2.5, 32.0, (("io", 20.0), ("cpu", 7.5))))

    def test_a_bound_that_is_no_number_is_refused(self):
        with self.assertRaises(SystemExit) as refused:
            Bounds.of({"CLADEFS_CONTAMINATION_WRITE_MB_S": "10MB"})
        self.assertEqual(str(refused.exception),
                         "CLADEFS_CONTAMINATION_WRITE_MB_S is MB/s, a number: 10MB")
        with self.assertRaises(SystemExit) as refused:
            Bounds.of({"CLADEFS_CONTAMINATION_STALL_PCT": "disk=20"})
        self.assertEqual(str(refused.exception),
                         "CLADEFS_CONTAMINATION_STALL_PCT is RESOURCE=PERCENT words, RESOURCE cpu, "
                         "memory or io: disk=20")


def clock(*times):
    """A clock that says `times`, one a call."""
    said = iter(times)
    return lambda: next(said)


class Run(unittest.TestCase):
    """A round run under it, on a host of fake_sysfs's: sampled as it starts and as it ends."""

    def setUp(self):
        self.tree = fake_sysfs.Tree()
        self.tree.disk("nvme0n1", "259:0")
        self.tree.partition("nvme0n1", "nvme0n1p2", "259:2")
        self.tree.mount("/", "259:2", "ext4", "/dev/nvme0n1p2")
        self.tree.io("nvme0n1", 4 * GB, 4 * GB)
        self.group = self.tree.cgroup("/bench.slice", io={"259:0": (1 * GB, 1 * GB)})
        self.tree.cgroup("/bench.slice/job.scope", stalled={"cpu": 0, "memory": 0, "io": 0})
        self.tree.runs_in("0::/bench.slice/job.scope\n")
        self.results = Path(tempfile.mkdtemp()) / "r1"

    def run_round(self, *shell):
        """Runs a round, 600 s by the clock, whose command is `shell`; its status, what it said,
        and its record."""
        said = io.StringIO()
        with contextlib.redirect_stderr(said):
            status = contamination.run(self.results, ["/results/r1"], ["/bin/sh", "-c", *shell],
                                       self.tree.host(), Bounds(), every=3600.0,
                                       clock=clock(0.0, 0.0, 0.0, 600.0))
        return status, said.getvalue(), json.loads((self.results / "contamination.json").read_text())

    def others_write(self, written, own):
        """A command after which the disk has written `written` bytes more, `own` of them the
        round's cgroup's."""
        stat = self.tree.root / "sys/class/block/nvme0n1/stat"
        return (f"echo '91 4 {4 * GB // 512} 120 77 13 {(4 * GB + written) // 512} 340' > {stat}; "
                f"echo '259:0 rbytes={GB} wbytes={GB + own}' > {self.group / 'io.stat'}")

    def test_a_round_beside_a_loader_is_contaminated_and_keeps_its_status(self):
        status, said, record = self.run_round(self.others_write(120 * GB, 8400 * MB) + "; exit 3")
        why = ["others wrote 186.0 MB/s to its disk over the round, bound 10 (nvme0n1 186.0)",
               "others wrote 186.0 MB/s to its disk in the minute 0 s in, bound 50"]
        self.assertEqual((status, record["verdict"], record["why"], record["cgroup"]),
                         (3, "contaminated", why, "/bench.slice"))
        self.assertEqual(said, f"contamination: CONTAMINATED: {why[0]}; {why[1]}\n")
        self.assertEqual(contamination.load(self.results), record)

    def test_a_round_alone_is_clean(self):
        status, said, record = self.run_round(self.others_write(9 * GB, 8400 * MB))
        self.assertEqual((status, record["verdict"], record["why"]), (0, "clean", []))
        self.assertEqual(record["pressure"], {"of": "cgroup", "cpu_ms": 0, "memory_ms": 0, "io_ms": 0})
        self.assertEqual(said, "contamination: clean: others wrote 1.0 MB/s to its disk over "
                               "600 s, 1.0 in its worst minute (bounds 10 and 50)\n")

    def test_a_round_whose_cgroup_others_share_is_unchecked(self):
        self.tree.cgroup("/bench.slice/other.scope")
        status, said, record = self.run_round(self.others_write(9 * GB, 8400 * MB))
        why = ("/bench.slice has the nearest I/O accounting to the round's cgroup, and holds "
               "others' work too (other.scope): others' writes can't be told from its own; its "
               "disk's whole writes were 15.0 MB/s over the round, 15.0 in its worst minute")
        self.assertEqual((status, record["verdict"], record["why"], record["cgroup"]),
                         (0, "unchecked", [why], None))
        self.assertEqual(said, f"contamination: UNCHECKED: {why}\n")

    def test_the_hosts_pressure_where_the_cgroup_has_none(self):
        self.tree.cgroup("/plain.scope", io={})
        self.tree.runs_in("0::/plain.scope\n")
        self.tree.host_stalled(cpu=7_000_000, memory=0, io=2_000_000)
        _, _, record = self.run_round(":")
        self.assertEqual((record["verdict"], record["pressure"]),
                         ("clean", {"of": "host", "cpu_ms": 0, "memory_ms": 0, "io_ms": 0}))

    def test_a_disk_gone_under_the_round_is_unchecked(self):
        stat = self.tree.root / "sys/class/block/nvme0n1/stat"
        status, said, record = self.run_round(f"rm {stat}; exit 4")
        self.assertEqual(status, 4)
        self.assertEqual(record, {"verdict": "unchecked", "why": [
            f"recording failed: FileNotFoundError: [Errno 2] No such file or directory: '{stat}'"]})
        self.assertEqual(said, "contamination: UNCHECKED: " + record["why"][0] + "\n")

    def test_a_round_that_left_no_record_is_unchecked(self):
        self.assertEqual(contamination.load(self.results), {"verdict": "unchecked", "why": [
            "no record of others' I/O beside it (contamination.json)"]})

    def test_it_is_sampled_as_the_round_runs(self):
        """A minute in, others have written 6 GB: 100 MB/s of that minute, which the round's
        125 s alone, 48 MB/s, would not say. The round says when it has written, and ends when
        told, each through a pipe."""
        wrote, end = self.results.parent / "wrote", self.results.parent / "end"
        os.mkfifo(wrote)
        os.mkfifo(end)

        def written():
            wrote.read_text()
            return 100.0

        def ended():
            end.write_text("end\n")
            return 60.0

        # As it starts, 0 s; the first minute's end, 60 s; past it, once the round has written:
        # the sample of 60 s; the round told to end before its second minute's; its end, 125 s.
        times = iter([lambda: 0.0, lambda: 0.0, written, lambda: 60.0, ended, lambda: 125.0])
        said = io.StringIO()
        with contextlib.redirect_stderr(said):
            status = contamination.run(
                self.results, ["/results/r1"],
                ["/bin/sh", "-c", f"{self.others_write(6 * GB, 0)}; echo > {wrote}; read x < {end}"],
                self.tree.host(), Bounds(write=100.0), every=60.0, clock=lambda: next(times)())
        record = json.loads((self.results / "contamination.json").read_text())
        self.assertEqual((status, record["verdict"], record["why"], record["seconds"]), (
            0, "contaminated",
            ["others wrote 100.0 MB/s to its disk in the minute 0 s in, bound 50"], 125.0))
        self.assertEqual((record["others"]["write_mb_s"],
                          record["others"]["worst_minute_write_mb_s"]), (48.0, 100.0))


class CommandLine(unittest.TestCase):
    def run_it(self, *args):
        done = subprocess.run([sys.executable, str(SCRIPTS / "contamination.py"), *args],
                              capture_output=True, text=True)
        return done.returncode, done.stdout, done.stderr

    def test_it_takes_a_rounds_results_and_command(self):
        usage = "usage: contamination.py run RESULTS [DISK...] -- COMMAND... | said RESULTS\n"
        for args in ([], ["run"], ["run", "results"], ["run", "--", "true"],
                     ["run", "results", "--"], ["said"], ["judge", "results", "--", "true"]):
            self.assertEqual(self.run_it(*args), (1, "", usage), args)

    def test_the_rounds_status_is_its_own(self):
        results = Path(tempfile.mkdtemp()) / "r1"
        status, out, _ = self.run_it("run", str(results), "--", "/bin/sh", "-c", "echo ran; exit 7")
        self.assertEqual((status, out), (7, "ran\n"))
        self.assertEqual(sorted(path.name for path in results.iterdir()), ["contamination.json"])

    def test_a_round_that_left_no_record_says_so(self):
        self.assertEqual(self.run_it("said", tempfile.mkdtemp()), (
            0, "UNCHECKED: no record of others' I/O beside it (contamination.json)\n", ""))


class Drivers(unittest.TestCase):
    """Each driver of a measurement's rounds runs them under it, its record in the round's
    results, where scripts/bench-ab.py reads it."""

    def test_each_runs_its_rounds_under_it(self):
        ab = (SCRIPTS / "lib" / "bench-ab-linux-host.sh").read_text()
        self.assertIn('env "${env[@]}" "$tools/scripts/contamination.py" run "$out" -- '
                      '"${command[@]}" \\\n', ab)
        bench = (SCRIPTS / "lib" / "bench-linux-host.sh").read_text()
        self.assertIn('CLADEFS_BENCH_RESULTS="$results" "$repo/scripts/contamination.py" run '
                      '"$results" -- \\\n    "$repo/scripts/bench.sh" "$iterations"', bench)
        # bench-rounds.sh's through the job, which alone can name the disk its cells are on, its
        # scratch directory: the record of that disk, in the round's directory.
        rounds = (SCRIPTS / "bench-rounds.sh").read_text()
        self.assertIn('"CLADEFS_E2E_PREBUILT=$artifacts" "CLADEFS_BENCH_RESULTS=$out" \\\n'
                      '            "$repo/scripts/test-e2e-job.sh" "$out/e2e" "$case"', rounds)
        job = (SCRIPTS / "test-e2e-job.sh").read_text()
        self.assertIn('CLADEFS_E2E_RESULTS="$work" "$root/scripts/contamination.py" run \\\n'
                      '    "${CLADEFS_BENCH_RESULTS:-$results}" "$work" -- '
                      '"$root/scripts/test-e2e.sh" "$@" || status=$?\n', job)

    def test_the_hosts_check_refuses_what_it_cannot_run(self):
        """scripts/contamination-check.sh, the two rounds on a host with fxjob."""
        def check(*args):
            done = subprocess.run(["/bin/bash", str(SCRIPTS / "contamination-check.sh"), *args],
                                  capture_output=True, text=True, env={"PATH": "/usr/bin:/bin"})
            return done.returncode, done.stdout, done.stderr

        self.assertEqual(check(), (2, "", "usage: contamination-check.sh RESULTS\n"))
        self.assertEqual(check(tempfile.mkdtemp()), (
            2, "", "fxjob not found: this runs on a test host with fxjob\n"))


if __name__ == "__main__":
    unittest.main()
