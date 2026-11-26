# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""The guest agent over virtio-vsock: the test guest boots with
libmachine_action=agent, a thread runs the machine, and the tests use the
agent from the main thread."""

import os
import threading
from pathlib import Path

import pytest

from libmachine import ExecResult, Machine

GUEST = Path(os.environ.get("LIBMACHINE_GUEST_DIR", Path(__file__).parents[3] / "target" / "guest"))

pytestmark = [
    pytest.mark.skipif(not os.access("/dev/kvm", os.R_OK | os.W_OK), reason="/dev/kvm is not accessible"),
    pytest.mark.skipif(
        not (GUEST / "vmlinux").is_file(),
        reason=f"no test guest in {GUEST} (run crates/libmachine/scripts/build-test-guest.sh)",
    ),
]


@pytest.fixture(scope="module")
def guest():
    m = Machine.boot_linux(
        GUEST / "vmlinux", GUEST / "initramfs.cpio", "quiet libmachine_action=agent", mem_size=256 << 20
    )
    agent, vsock, stop = m.agent(), m.vsock(), m.stop_handle()
    exits = []
    runner = threading.Thread(target=lambda: exits.append(m.run(timeout=300)))
    runner.start()
    try:
        agent.wait_ready(60)
        yield agent, vsock
    finally:
        stop.stop()
        runner.join()
    assert exits[0].kind == "stopped", m.serial_output().decode(errors="replace")


def test_exec_output_and_exit_code(guest):
    agent, _ = guest
    r = agent.exec(["sh", "-c", "echo out; echo err >&2; exit 3"])
    assert isinstance(r, ExecResult)
    assert (r.stdout, r.stderr, r.exit_code, r.signal) == (b"out\n", b"err\n", 3, None)
    r = agent.exec(["sh", "-c", 'echo "$FOO $(pwd)"'], env={"FOO": "bar", "PATH": "/bin"}, cwd="/tmp")
    assert r.stdout == b"bar /tmp\n"
    assert agent.exec(["wc", "-c"], stdin=b"x" * 100000).stdout.strip() == b"100000"
    assert agent.exec(["sh", "-c", "kill -9 $$"]).signal == 9


def test_missing_program_is_file_not_found(guest):
    agent, _ = guest
    with pytest.raises(FileNotFoundError) as e:
        agent.exec(["/no/such/program"])
    assert "/no/such/program" in e.value.filename


def test_streaming_stdin(guest):
    agent, _ = guest
    p = agent.spawn(["cat"])
    assert p.pid > 1
    p.write_stdin(b"hello\n")
    assert p.read() == ("stdout", b"hello\n")
    p.close_stdin()
    assert p.read() == ("exit", (0, None))
    assert p.read() is None
    p = agent.spawn(["sleep", "1000"])
    p.kill()
    assert p.wait().signal == 9


def test_files_both_ways(guest):
    agent, _ = guest
    data = os.urandom(1 << 20)
    agent.write_file("/tmp/py-blob", data, 0o700)
    assert agent.read_file("/tmp/py-blob") == data
    assert agent.exec(["stat", "-c", "%a", "/tmp/py-blob"]).stdout == b"700\n"
    with pytest.raises(FileNotFoundError):
        agent.read_file("/no/such/file")
    with pytest.raises(IsADirectoryError):
        agent.read_file("/tmp")


def test_a_command_survives_a_reset(guest):
    agent, vsock = guest
    assert vsock.guest_cid == 3
    p = agent.spawn(["sh", "-c", "sleep 0.3; echo done"])
    vsock.reset_connections()
    r = p.wait()
    assert (r.stdout, r.exit_code) == (b"done\n", 0)
    # Execs are sent again after resets, and run once.
    for i in range(20):
        vsock.reset_connections()
        assert agent.exec(["sh", "-c", f"echo {i} >> /tmp/py-once"]).exit_code == 0
    assert agent.read_file("/tmp/py-once") == "".join(f"{i}\n" for i in range(20)).encode()
    agent.ping()


def test_a_guest_that_powers_off_fails_what_waits_on_it():
    m = Machine.boot_linux(
        GUEST / "vmlinux", GUEST / "initramfs.cpio", "quiet libmachine_action=agent", mem_size=256 << 20
    )
    agent, stop = m.agent(), m.stop_handle()
    exits = []
    runner = threading.Thread(target=lambda: exits.append(m.run(timeout=300)))
    runner.start()
    try:
        agent.wait_ready(60)
        p = agent.spawn(["sleep", "1000"])
        errors = []

        def wait():
            try:
                p.wait()
            except ConnectionResetError as e:
                errors.append(e)

        waiter = threading.Thread(target=wait)
        waiter.start()
        try:
            agent.exec(["poweroff", "-f"])
        except ConnectionResetError:
            pass
        runner.join()
        waiter.join(timeout=5)
        assert not waiter.is_alive()
        assert exits[0].kind == "poweroff"
        assert len(errors) == 1 and "gone" in str(errors[0])
        with pytest.raises(ConnectionResetError):
            agent.ping()
    finally:
        stop.stop()
        runner.join()


def test_bare_machine_has_no_agent():
    with pytest.raises(ValueError, match="no vsock"):
        Machine().agent()
