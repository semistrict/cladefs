# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Boots the test guest built by crates/libmachine/scripts/build-test-guest.sh."""

import os
from pathlib import Path

import pytest

from libmachine import ExtentDaemon, ExtentStore, Machine, MachineError

GUEST = Path(os.environ.get("LIBMACHINE_GUEST_DIR", Path(__file__).parents[3] / "target" / "guest"))
MARKER = b"libmachine-init: up "

pytestmark = [
    pytest.mark.skipif(not os.access("/dev/kvm", os.R_OK | os.W_OK), reason="/dev/kvm is not accessible"),
    pytest.mark.skipif(
        not (GUEST / "vmlinux").is_file(),
        reason=f"no test guest in {GUEST} (run crates/libmachine/scripts/build-test-guest.sh)",
    ),
]


@pytest.mark.parametrize("kernel", ["vmlinux", "bzImage"])
def test_boot_to_init_and_power_off(kernel):
    m = Machine.boot_linux(GUEST / kernel, GUEST / "initramfs.cpio", "quiet")
    assert m.run().kind == "poweroff"
    assert MARKER in m.serial_output()


def test_reboot_is_a_reset():
    m = Machine.boot_linux(GUEST / "vmlinux", GUEST / "initramfs.cpio", "libmachine_action=reboot")
    assert m.run().kind == "reset"


def test_missing_kernel_is_file_not_found():
    with pytest.raises(FileNotFoundError) as e:
        Machine.boot_linux(GUEST / "no-such-kernel")
    assert Path(e.value.filename) == GUEST / "no-such-kernel"
    assert e.value.strerror == "No such file or directory"


def test_bad_kernel_is_value_error():
    with pytest.raises(ValueError, match="not an ELF vmlinux or a bzImage"):
        Machine.boot_linux(GUEST / "initramfs.cpio")


def test_ram_past_the_mmio_hole():
    # Past 3 GiB, RAM continues at 4 GiB and the kernel uses it.
    m = Machine.boot_linux(GUEST / "bzImage", GUEST / "initramfs.cpio", mem_size=4 << 30)
    assert m.mem_size == 4 << 30
    assert m.run().kind == "poweroff"
    console = m.serial_output().decode(errors="replace")
    assert "[mem 0x0000000100000000-0x000000013fffffff] usable" in console
    total_kb = next(
        int(line.split()[2]) for line in console.splitlines() if line.startswith("libmachine-init: MemTotal ")
    )
    assert total_kb * 1024 > (4 << 30) * 9 // 10


def online_cpus(console):
    """What the guest's init printed of /sys/devices/system/cpu/online."""
    lines = console.splitlines()
    start = lines.index("libmachine-init: /sys/devices/system/cpu/online:")
    return lines[start + 1].strip()


@pytest.mark.parametrize("vcpus,online", [(1, "0"), (2, "0-1"), (4, "0-3")])
def test_vcpus_come_online(vcpus, online):
    m = Machine.boot_linux(
        GUEST / "vmlinux",
        GUEST / "initramfs.cpio",
        "quiet libmachine_cat=/sys/devices/system/cpu/online",
        vcpus=vcpus,
    )
    assert m.vcpus == vcpus
    assert f"vcpus={vcpus}" in repr(m)
    assert m.run(timeout=30).kind == "poweroff"
    assert online_cpus(m.serial_output().decode(errors="replace")) == online


@pytest.mark.parametrize("vcpus", [0, 255, -1, 1 << 40])
def test_invalid_vcpu_counts_are_refused(vcpus):
    with pytest.raises(ValueError, match="vcpus must be from 1 to 254"):
        Machine.boot_linux(GUEST / "vmlinux", vcpus=vcpus)


def test_timeout_stops_every_vcpu():
    # Every vCPU spins in the guest; the timeout stops them all, and the
    # machine runs on afterwards.
    m = Machine.boot_linux(
        GUEST / "vmlinux", GUEST / "initramfs.cpio", "quiet libmachine_action=spin", vcpus=4
    )
    assert m.run(timeout=2).kind == "timed_out"
    assert b"libmachine-init: spinning on 4 CPUs" in m.serial_output()
    assert m.run(timeout=0.1).kind == "timed_out"


def test_a_long_run_kicks_each_vcpu_once():
    # While the guest spins on every vCPU, a second's run checks for Python
    # signals without stopping the vCPUs: each is kicked once, at the end.
    m = Machine.boot_linux(
        GUEST / "vmlinux", GUEST / "initramfs.cpio", "quiet libmachine_action=spin", vcpus=4
    )
    assert m.run(timeout=2).kind == "timed_out"
    assert b"libmachine-init: spinning on 4 CPUs" in m.serial_output()
    before = m.exit_counts()["interrupted"]
    assert m.run(timeout=1).kind == "timed_out"
    assert m.exit_counts()["interrupted"] - before == 4


def vcpu_threads():
    """The process's threads named vcpu<N>."""
    tasks = Path("/proc/self/task")
    return sum(1 for t in tasks.iterdir() if (t / "comm").read_text().startswith("vcpu"))


def test_a_dropped_machine_takes_its_vcpu_threads_with_it():
    for _ in range(3):
        m = Machine.boot_linux(GUEST / "vmlinux", GUEST / "initramfs.cpio", "quiet", vcpus=4)
        assert vcpu_threads() == 4
        assert m.run(timeout=30).kind == "poweroff"
        del m
        assert vcpu_threads() == 0


def test_extent_stores_need_an_extent_daemon(tmp_path):
    store = ExtentStore("extents", tmp_path / "cell", 64 << 20)
    with pytest.raises(
        ValueError,
        match="^invalid configuration: extent store devices need an extent daemon: extent_daemon is None$",
    ):
        Machine.boot_linux(GUEST / "vmlinux", GUEST / "initramfs.cpio", extent_stores=[store])


def test_an_extent_daemon_not_there_fails_the_machine(tmp_path):
    # The machine's devices connect to the daemon as it is made: one nobody listens at fails it,
    # naming the device and the socket.
    daemon = ExtentDaemon(tmp_path / "no-daemon.sock")
    store = ExtentStore("extents", tmp_path / "cell", 64 << 20, dax_window_size=32 << 20, id="root")
    with pytest.raises(MachineError) as e:
        Machine.boot_linux(
            GUEST / "vmlinux", GUEST / "initramfs.cpio", extent_daemon=daemon, extent_stores=[store]
        )
    assert str(e.value).startswith(
        f"extent store device: root: connecting to the extent daemon: connecting to the extent "
        f"daemon at {tmp_path / 'no-daemon.sock'}: "
    ), str(e.value)


def test_an_extent_store_cache_is_writeback_or_unsafe(tmp_path):
    with pytest.raises(ValueError, match='^cache is "writeback" or "unsafe", not \'always\'$'):
        ExtentStore("extents", tmp_path / "cell", 64 << 20, cache="always")
