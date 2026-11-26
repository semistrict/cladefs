# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Snapshots from Python: a booted guest with its agent is snapshotted, and
machines restored from the file run commands on their own copies of it.
Snapshot files go under target/ and are removed after."""

import os
import shutil
import threading
from pathlib import Path

import pytest

from libmachine import SNAPSHOT_FORMAT_VERSION, Machine, MachineError, Snapshot

ROOT = Path(__file__).parents[3]
GUEST = Path(os.environ.get("LIBMACHINE_GUEST_DIR", ROOT / "target" / "guest"))

pytestmark = [
    pytest.mark.skipif(not os.access("/dev/kvm", os.R_OK | os.W_OK), reason="/dev/kvm is not accessible"),
    pytest.mark.skipif(
        not (GUEST / "vmlinux").is_file(),
        reason=f"no test guest in {GUEST} (run crates/libmachine/scripts/build-test-guest.sh)",
    ),
]


@pytest.fixture
def workdir():
    d = ROOT / "target" / "snapshot-tests" / "python"
    shutil.rmtree(d, ignore_errors=True)
    d.mkdir(parents=True)
    yield d
    shutil.rmtree(d, ignore_errors=True)


class Running:
    """A machine running on a thread of its own."""

    def __init__(self, machine):
        self.machine = machine
        self.agent = machine.agent()
        self.stop_handle = machine.stop_handle()
        self.exits = []
        self.thread = threading.Thread(target=lambda: self.exits.append(machine.run(timeout=300)))
        self.thread.start()

    def stop(self):
        self.stop_handle.stop()
        self.thread.join()
        return self.machine

    def sh(self, script):
        r = self.agent.exec(["sh", "-c", script])
        assert r.exit_code == 0, r
        return r.stdout.decode()


def test_snapshot_and_restore_clones(workdir):
    template = Running(
        Machine.boot_linux(
            GUEST / "vmlinux", GUEST / "initramfs.cpio", "quiet libmachine_action=agent", mem_size=256 << 20
        )
    )
    template.agent.wait_ready(60)
    template.sh("echo from-python > /tmp/marker")
    machine = template.stop()

    snapshot = Snapshot.take(machine, workdir / "template")
    assert (snapshot.mem_size, snapshot.vcpus, snapshot.cpu_model) == (256 << 20, 1, "host")
    assert snapshot.cpu_fingerprint == machine.cpu_fingerprint
    assert SNAPSHOT_FORMAT_VERSION >= 1
    reopened = Snapshot(workdir / "template")
    assert Path(reopened.path) == workdir / "template"

    clones = [Running(reopened.restore()) for _ in range(2)]
    try:
        for i, c in enumerate(clones):
            assert c.sh("cat /tmp/marker") == "from-python\n"
            c.sh(f"echo clone{i} > /tmp/marker")
        randoms = [c.sh("head -c 16 /dev/urandom | od -An -tx1") for c in clones]
        assert randoms[0] != randoms[1]
        for i, c in enumerate(clones):
            assert c.sh("cat /tmp/marker") == f"clone{i}\n"
    finally:
        for c in clones:
            c.stop()

    with pytest.raises(MachineError, match="vCPUs"):
        reopened.restore(vcpus=2)
    with pytest.raises(MachineError, match="memory map"):
        reopened.restore(mem_size=128 << 20)
    with pytest.raises(ValueError):
        reopened.restore(cpu_model="pentium")


def test_files_that_are_no_snapshot_are_refused(workdir):
    bogus = workdir / "bogus"
    bogus.write_bytes(b"not a snapshot" * 10)
    with pytest.raises(MachineError, match="not a usable snapshot"):
        Snapshot(bogus)
    with pytest.raises(FileNotFoundError):
        Snapshot(workdir / "missing")
    bare = Machine(mem_size=1 << 20)
    with pytest.raises(MachineError, match="PC machines"):
        Snapshot.take(bare, workdir / "bare")
