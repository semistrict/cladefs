# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Stopping a running machine: timeouts, stop handles, Ctrl-C."""

import os
import signal
import threading
import time

import pytest

from guest import machine_with
from libmachine import Machine

pytestmark = pytest.mark.skipif(
    not os.access("/dev/kvm", os.R_OK | os.W_OK), reason="/dev/kvm is not accessible"
)

# A guest that never stops: it counts at 0x500 forever.
FOREVER = """
top:
    inc dword [0x500]
    jmp top
"""


def counter(m: Machine) -> int:
    return int.from_bytes(m.read_memory(0x500, 4), "little")


def test_timeout():
    m = machine_with(FOREVER)
    start = time.monotonic()
    assert m.run(timeout=0.2).kind == "timed_out"
    assert 0.2 <= time.monotonic() - start < 2
    first = counter(m)
    assert first > 0
    assert m.run(timeout=0.05).kind == "timed_out"
    assert counter(m) > first, "the machine resumed where it stopped"


def test_zero_timeout_does_not_run_the_guest():
    m = machine_with(FOREVER)
    assert m.run(timeout=0).kind == "timed_out"
    assert counter(m) == 0


@pytest.mark.parametrize("bad", [-1, float("nan"), float("inf")])
def test_bad_timeout(bad):
    with pytest.raises(ValueError, match="timeout"):
        machine_with(FOREVER).run(timeout=bad)


def test_stop_from_another_thread():
    m = machine_with(FOREVER)
    handle = m.stop_handle()
    threading.Timer(0.1, handle.stop).start()
    start = time.monotonic()
    assert m.run().kind == "stopped"
    assert time.monotonic() - start < 2


def test_stop_before_run_is_kept():
    m = machine_with(FOREVER)
    m.stop_handle().stop()
    assert m.run().kind == "stopped"
    assert counter(m) == 0


def test_a_long_run_does_not_stop_the_vcpus_to_check_signals():
    m = machine_with(FOREVER)
    assert m.run(timeout=1.0).kind == "timed_out"
    # One kick, the timeout's. Checking for signals by running in 50 ms
    # slices kicked and restarted the vCPU 20 times.
    assert m.exit_counts()["interrupted"] == 1


def test_ctrl_c_stops_the_guest_and_raises():
    m = machine_with(FOREVER)
    sent = []

    def ctrl_c():
        sent.append(time.monotonic())
        os.kill(os.getpid(), signal.SIGINT)

    threading.Timer(0.3, ctrl_c).start()
    with pytest.raises(KeyboardInterrupt):
        m.run()
    latency = time.monotonic() - sent[0]
    assert latency < 0.25, f"KeyboardInterrupt {latency * 1e3:.0f} ms after Ctrl-C"
    # The vCPU ran undisturbed until then: the stop's kick, and perhaps the
    # SIGINT itself if it landed on the vCPU's thread.
    assert m.exit_counts()["interrupted"] <= 2
    # The machine is intact and runs again.
    before = counter(m)
    assert before > 0
    assert m.run(timeout=0.05).kind == "timed_out"
    assert counter(m) > before
