# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A Linux host's /proc, /sys and /dev, as scripts/contamination.py reads them, for its tests
(test_contamination.py): a tree of its own under a temporary directory, built a disk, a mount and
a cgroup at a time, laid out as the kernel lays them out (a partition's directory inside its
disk's, /sys/dev/block and /sys/class/block links to them, /dev/zvol's links to the zvols'
devices), so that no test reads the machine's own."""

import tempfile
from pathlib import Path

import script

contamination = script.load("contamination")
SECTOR = 512


class Tree:
    def __init__(self):
        self.root = Path(tempfile.mkdtemp(prefix="sysfs"))
        self.zpools = {}
        for directory in ("proc/self", "sys/dev/block", "sys/class/block", "sys/devices",
                          "sys/fs/cgroup", "dev"):
            (self.root / directory).mkdir(parents=True)

    def host(self):
        """The host this tree is, its zpool what `pool_disks` said."""
        return contamination.Host(self.root, zpool=self.zpools.get)

    def _device(self, directory, name, number):
        directory.mkdir(parents=True)
        (directory / "dev").write_text(f"{number}\n")
        (self.root / "sys/dev/block" / number).symlink_to(directory)
        (self.root / "sys/class/block" / name).symlink_to(directory)
        self.io(name, 0, 0)
        return directory

    def disk(self, name, number):
        """A whole disk NAME, its device's number NUMBER."""
        (self._device(self.root / "sys/devices" / name, name, number) / "slaves").mkdir()

    def partition(self, disk, name, number):
        """The partition NAME of DISK."""
        directory = self._device(self.root / "sys/devices" / disk / name, name, number)
        (directory / "partition").write_text("1\n")

    def loop(self, name, number, backing):
        """A loop device NAME on the file BACKING."""
        self.disk(name, number)
        (self.root / "sys/devices" / name / "loop").mkdir()
        (self.root / "sys/devices" / name / "loop/backing_file").write_text(f"{backing}\n")

    def over(self, name, number, *slaves):
        """A device NAME made of SLAVES, as a device mapper's or md's."""
        self.disk(name, number)
        for slave in slaves:
            (self.root / "sys/devices" / name / "slaves" / slave).symlink_to(
                (self.root / "sys/class/block" / slave).resolve())

    def zvol(self, name, number, dataset):
        """A zvol NAME, the volume DATASET, which /dev/zvol names."""
        self.disk(name, number)
        link = self.root / "dev/zvol" / dataset
        link.parent.mkdir(parents=True, exist_ok=True)
        link.symlink_to("../" * dataset.count("/") + f"../{name}")

    def io(self, name, read, written):
        """What the device NAME has read and written so far, in bytes."""
        (self.root / "sys/class/block" / name / "stat").write_text(
            f"   91    4 {read // SECTOR}  120   77   13 {written // SECTOR}  340 0 500 460\n")

    def mount(self, version, number, filesystem, source):
        """A mount at POINT of SOURCE, a FILESYSTEM, on the device NUMBER."""
        with (self.root / "proc/self/mountinfo").open("a") as mounts:
            mounts.write(f"36 25 {number} / {version} rw,relatime shared:1 - {filesystem} {source} "
                         "rw\n")

    def dataset(self, objset, name, read, written):
        """What ZFS counts of the dataset NAME, its pool's objset OBJSET."""
        directory = self.root / "proc/spl/kstat/zfs" / name.split("/")[0]
        directory.mkdir(parents=True, exist_ok=True)
        (directory / f"objset-{objset}").write_text(
            "31 1 0x01 7 2160 5214416391 73405386\n"
            "name                            type data\n"
            f"dataset_name                    7    {name}\n"
            "writes                          4    409570\n"
            f"nwritten                        4    {written}\n"
            "reads                           4    22108699\n"
            f"nread                           4    {read}\n")

    def pool_disks(self, pool, *devices):
        """What `zpool list -vHPL POOL` says: the pool, then its disks' devices."""
        self.zpools[pool] = f"{pool}\t928G\t512G\t416G\t-\t-\t41%\t55%\t1.00x\tONLINE\t-\n" + "".join(
            f"\t{device}\t928G\t512G\t416G\t-\t-\t41%\t55.2%\t-\tONLINE\n" for device in devices)

    def cgroup(self, name, *, io=None, populated=True, stalled=None):
        """The cgroup NAME: its io.stat, {number: (read, written)}, if IO is given; whether
        anything runs in it; and how long its tasks stalled on each resource, µs, {resource: µs}."""
        directory = self.root / "sys/fs/cgroup" / name.lstrip("/")
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "cgroup.events").write_text(f"populated {int(populated)}\nfrozen 0\n")
        if io is not None:
            (directory / "io.stat").write_text("".join(
                f"{number} rbytes={read} wbytes={written} rios=7 wios=9 dbytes=0 dios=0\n"
                for number, (read, written) in io.items()))
        for resource, micros in (stalled or {}).items():
            (directory / f"{resource}.pressure").write_text(
                f"some avg10=0.00 avg60=0.00 avg300=0.00 total={micros}\n"
                f"full avg10=0.00 avg60=0.00 avg300=0.00 total={micros // 2}\n")
        return directory

    def runs_in(self, text):
        """This process's /proc/self/cgroup."""
        (self.root / "proc/self/cgroup").write_text(text)

    def host_stalled(self, **stalled):
        """How long the host's tasks stalled on each resource, µs."""
        (self.root / "proc/pressure").mkdir(exist_ok=True)
        for resource, micros in stalled.items():
            (self.root / "proc/pressure" / resource).write_text(
                f"some avg10=0.00 avg60=0.00 avg300=0.00 total={micros}\n")
