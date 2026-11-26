# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""AsyncSandbox from asyncio: the test guest boots with libmachine_action=agent;
each test runs its own event loop with asyncio.run (no pytest plugin)."""

import asyncio
import hashlib
import os
import subprocess
import sys
import textwrap
import time
from pathlib import Path

import pytest

from libmachine import AsyncProcess, AsyncSandbox, EgressPolicy, ExecResult, GuestBusyError, MachineError, Snapshot

GUEST = Path(os.environ.get("LIBMACHINE_GUEST_DIR", Path(__file__).parents[3] / "target" / "guest"))

pytestmark = [
    pytest.mark.skipif(not os.access("/dev/kvm", os.R_OK | os.W_OK), reason="/dev/kvm is not accessible"),
    pytest.mark.skipif(
        not (GUEST / "vmlinux").is_file(),
        reason=f"no test guest in {GUEST} (run crates/libmachine/scripts/build-test-guest.sh)",
    ),
]

# Generous bound for "promptly", so that a loaded host does not flake.
PROMPT = 2.0


def create(cmdline="quiet libmachine_action=agent", **kwargs):
    return AsyncSandbox.create(
        GUEST / "vmlinux", GUEST / "initramfs.cpio", cmdline, mem_size=256 << 20, ready_timeout=60, **kwargs
    )


async def alive(sb, pid):
    return (await sb.exec(["sh", "-c", f"kill -0 {pid} 2>/dev/null"])).exit_code == 0


async def until_dead(sb, pid):
    deadline = time.monotonic() + PROMPT
    while await alive(sb, pid):
        assert time.monotonic() < deadline, f"command {pid} still runs"


def test_many_sandboxes_at_once(tmp_path):
    async def one(i):
        async with await create() as sb:
            r = await sb.exec(["sh", "-c", f"echo {i}"])
            assert isinstance(r, ExecResult)
            assert (r.stdout, r.exit_code) == (f"{i}\n".encode(), 0)
            data = bytes(range(256)) * 400 + bytes([i])
            await sb.write_file("/tmp/data", data, 0o600)
            assert await sb.read_file("/tmp/data") == data
            (tmp_path / f"to{i}").write_text(f"sandbox {i}")
            await sb.copy_to_guest(tmp_path / f"to{i}", "/tmp/copied")
            await sb.copy_from_guest("/tmp/copied", tmp_path / f"back{i}")
            assert (tmp_path / f"back{i}").read_text() == f"sandbox {i}"
            assert sb.ended is None
        assert sb.ended.kind == "closed"
        return i

    async def main():
        return await asyncio.gather(*(one(i) for i in range(8)))

    assert asyncio.run(main()) == list(range(8))


def test_streaming_and_cancelled_reads():
    async def main():
        async with await create() as sb:
            await streaming(sb)

    async def streaming(sb):
        p = await sb.spawn(["cat"])
        assert isinstance(p, AsyncProcess) and p.pid > 1
        await p.write_stdin(b"hello\n")
        assert await p.read() == ("stdout", b"hello\n")
        await p.close_stdin()
        assert await p.read() == ("exit", (0, None))
        assert await p.read() is None

        # Reads cancelled by a timeout lose nothing.
        p = await sb.spawn(["sh", "-c", "for i in 1 2 3 4 5; do echo $i; sleep 0.05; done"])
        out, cancelled = b"", 0
        while True:
            try:
                event = await asyncio.wait_for(p.read(), 0.005)
            except TimeoutError:
                cancelled += 1
                continue
            if event[0] == "exit":
                assert event[1] == (0, None)
                break
            out += event[1]
        assert out == b"1\n2\n3\n4\n5\n"
        assert cancelled > 10

        r = await sb.exec(["wc", "-c"], stdin=b"x" * 300000)
        assert r.stdout.strip() == b"300000"
        with pytest.raises(FileNotFoundError):
            await sb.exec(["/no/such/program"])
        assert (await sb.close()).kind == "closed"

    asyncio.run(main())


def test_cancelling_exec_kills_the_command():
    async def main():
        async with await create() as sb:
            with pytest.raises(TimeoutError):
                await asyncio.wait_for(sb.exec(["sh", "-c", "echo $$ > /tmp/pid; exec sleep 1000"]), 0.5)
            pid = (await sb.read_file("/tmp/pid")).decode().strip()
            await until_dead(sb, pid)

            p = await sb.spawn(["sleep", "1000"])
            pid = p.pid
            assert await alive(sb, pid)
            del p
            await until_dead(sb, pid)

    asyncio.run(main())


def test_closing_while_a_command_runs():
    async def main():
        async with await create() as sb:
            running = asyncio.create_task(sb.exec(["sleep", "1000"]))
            await asyncio.sleep(0.2)
            start = time.monotonic()
            assert (await sb.close()).kind == "closed"
            with pytest.raises(ConnectionResetError):
                await running
            with pytest.raises(ConnectionResetError):
                await sb.read_file("/etc/passwd")
            assert time.monotonic() - start < PROMPT
            assert (await sb.wait()).kind == "closed"

    asyncio.run(main())


def test_a_guest_that_powers_off_fails_waiting_calls():
    async def main():
        async with await create() as sb:
            p = await sb.spawn(["sleep", "1000"])
            waiting = asyncio.create_task(p.wait())
            try:
                await sb.exec(["poweroff", "-f"])
            except ConnectionResetError:
                pass
            assert (await sb.wait()).kind == "poweroff"
            with pytest.raises(ConnectionResetError):
                await waiting
            with pytest.raises(ConnectionResetError):
                await sb.exec(["true"])
            assert (await sb.close()).kind == "poweroff"

    asyncio.run(main())


def test_a_failed_boot_fails_create_at_once():
    async def main():
        start = time.monotonic()
        with pytest.raises(ConnectionResetError, match="Poweroff"):
            await create("libmachine_action=poweroff")
        assert time.monotonic() - start < 10
        with pytest.raises(ValueError):
            await create(vcpus=0)

    asyncio.run(main())


def run_script(script):
    """Runs `script` in a Python process of its own (it may leave sandboxes
    and calls behind as it exits); its exit status, stdout and stderr."""
    r = subprocess.run(
        [sys.executable, "-c", textwrap.dedent(script)],
        capture_output=True,
        text=True,
        timeout=120,
        env={**os.environ, "LIBMACHINE_GUEST_DIR": str(GUEST)},
    )
    return r.returncode, r.stdout, r.stderr


SCRIPT_HEAD = """
    import asyncio, os
    from pathlib import Path
    from libmachine import AsyncSandbox
    from libmachine import _libmachine
    G = Path(os.environ["LIBMACHINE_GUEST_DIR"])
    def create():
        return AsyncSandbox.create(G / "vmlinux", G / "initramfs.cpio", "quiet libmachine_action=agent",
                                   mem_size=256 << 20, ready_timeout=60)
"""


def test_no_call_completes_once_python_exits():
    """A completion is a call into Python from a tokio thread; one after the
    interpreter finalized crashed the process (a call failing because its
    sandbox was collected at exit). Once the atexit hook has run, none
    completes."""
    code, out, err = run_script(
        SCRIPT_HEAD
        + """
    async def main():
        sb = await create()
        task = asyncio.ensure_future(sb.exec(["sh", "-c", "sleep 0.2; echo done"]))
        _libmachine._python_exiting()  # what atexit calls
        await asyncio.sleep(1)
        print("done" if task.done() else "pending", flush=True)
        task.cancel()
    asyncio.run(main())
    """
    )
    assert (code, out) == (0, "pending\n"), err


def test_exiting_with_sandboxes_and_calls_left_behind():
    """Python exits with a sandbox alive, a command running and calls never
    awaited: the sandbox is collected while the interpreter finalizes,
    failing the calls; the process still exits cleanly."""
    for _ in range(3):
        code, out, err = run_script(
            SCRIPT_HEAD
            + """
    import warnings
    warnings.simplefilter("ignore")  # the coroutines never awaited
    left = []
    async def main():
        sb = await create()
        left.append(sb)
        left.append(await sb.spawn(["sleep", "1000"]))
        left.append(sb.exec(["sleep", "1000"]))
        left.append(sb.wait())
        await asyncio.sleep(0.1)
    asyncio.run(main())
    # Collected only by the last collection, while the interpreter finalizes.
    left.append(left)
    print("exiting", flush=True)
    """
        )
        assert (code, out) == (0, "exiting\n"), err
        assert "Traceback" not in err, err


def test_cancelled_reads_lose_nothing_under_pressure():
    # Reads cancelled at random sub-millisecond points, while a command
    # prints fast: a read whose task had already received an event when
    # Python cancelled it must leave that event for the next read.
    import random

    async def main():
        async with await create() as sb:
            # One line every ~2 ms, so most reads wait and many are cancelled.
            p = await sb.spawn(
                ["sh", "-c", "i=0; while [ $i -lt 300 ]; do i=$((i+1)); echo $i; usleep 2000; done"]
            )
            rng = random.Random(1)
            out, cancelled = b"", 0
            while True:
                try:
                    event = await asyncio.wait_for(p.read(), rng.uniform(0, 0.001))
                except TimeoutError:
                    cancelled += 1
                    continue
                if event[0] == "exit":
                    break
                out += event[1]
            assert out.decode().split() == [str(i) for i in range(1, 301)]
            assert cancelled > 100

    asyncio.run(main())


def test_exiting_while_sandbox_calls_are_cancelled():
    """Calls cancelled just before asyncio.run returns, while their sandbox
    goes away at exit: the cancellation reached Python from tokio threads
    after the interpreter finalized and crashed 1 run in 2 (board 13191;
    test_awaitables.py tests the bridge itself, without a guest)."""
    for _ in range(3):
        code, out, err = run_script(
            SCRIPT_HEAD
            + """
    async def main():
        sb = await create()
        tasks = [asyncio.ensure_future(sb.exec(["sh", "-c", "sleep 5"])) for _ in range(64)]
        await asyncio.sleep(0.2)
        for t in tasks:
            t.cancel()
        print("cancelled", flush=True)
    asyncio.run(main())
    """
        )
        assert (code, out) == (0, "cancelled\n"), err


def test_exiting_while_the_decider_is_asked():
    """The guest connects in a loop through an ask rule while Python exits:
    the decider thread enters Python through the same gate as completions,
    and stops being asked once the atexit hook has closed it (board 13191)."""
    for _ in range(3):
        code, out, err = run_script(
            SCRIPT_HEAD
            + """
    import time
    from libmachine import EgressPolicy, NetworkConfig
    asked = []
    def decide(req):
        asked.append(req.port)
        time.sleep(0.001)
        return False
    left = []
    async def main():
        net = NetworkConfig(EgressPolicy().ask("127.0.0.1"), decider=decide, decider_timeout=5.0)
        sb = await AsyncSandbox.create(G / "vmlinux", G / "initramfs.cpio", "quiet libmachine_action=agent",
                                       mem_size=256 << 20, ready_timeout=60, network=net)
        left.append(sb)
        left.append(await sb.spawn(["sh", "-c", "while true; do nc 10.0.2.1 1 </dev/null; done"]))
        while len(asked) < 20:
            await asyncio.sleep(0.01)
    asyncio.run(main())
    print("asked", flush=True)
    """
        )
        assert (code, out) == (0, "asked\n"), err


def test_sandboxes_from_a_snapshot(tmp_path):
    """A template sandbox is snapshotted while it runs on; sandboxes restored
    from the snapshot at once each see its file, keep their own writes and
    draw their own random numbers; restoring is faster than booting."""

    async def main():
        start = time.monotonic()
        async with await create() as template:
            booted = time.monotonic() - start
            await template.write_file("/tmp/marker", b"template\n")
            snapshot = await template.snapshot(tmp_path / "template")
            assert isinstance(snapshot, Snapshot) and snapshot.mem_size == 256 << 20
            # The template runs on after its snapshot.
            assert (await template.exec(["cat", "/tmp/marker"])).stdout == b"template\n"

        async def clone(i):
            start = time.monotonic()
            sb = await AsyncSandbox.restore(snapshot, ready_timeout=60)
            created = time.monotonic() - start
            async with sb:
                assert (await sb.exec(["cat", "/tmp/marker"])).stdout == b"template\n"
                await sb.write_file("/tmp/marker", f"clone {i}\n".encode())
                random = (await sb.exec(["sh", "-c", "head -c 16 /dev/urandom | od -An -tx1"])).stdout
                assert (await sb.exec(["cat", "/tmp/marker"])).stdout == f"clone {i}\n".encode()
            return created, random

        results = await asyncio.gather(*(clone(i) for i in range(4)))
        randoms = [r for _, r in results]
        assert len(set(randoms)) == len(randoms), "two clones drew the same random bytes"
        created = sorted(c for c, _ in results)
        assert created[len(created) // 2] < booted, (created, booted)

        # A networked sandbox cannot be snapshotted, and runs on.

        async with await create(network=EgressPolicy()) as networked:
            with pytest.raises(MachineError, match="network"):
                await networked.snapshot(tmp_path / "net")
            assert (await networked.exec(["true"])).exit_code == 0

    asyncio.run(main())


# Suspended guests' RAM goes to a disk: /tmp may be tmpfs, which is refused.
RAM_DIR = Path(__file__).parents[3] / "target" / "suspend-py"


def test_a_slow_answer_wakes_a_suspended_sandbox(tmp_path):
    """board 13203 from Python: the guest asks a slow server, the sandbox is
    suspended while it waits, and the answer wakes it on the same connection."""
    RAM_DIR.mkdir(parents=True, exist_ok=True)
    body = bytes(i * 7 % 251 for i in range(300_000))

    async def main():
        asked = asyncio.Event()

        async def serve(reader, writer):
            await reader.readuntil(b"\r\n\r\n")
            asked.set()
            await asyncio.sleep(1.5)
            writer.write(b"HTTP/1.0 200 OK\r\nContent-Length: %d\r\n\r\n" % len(body) + body)
            await writer.drain()
            writer.close()

        server = await asyncio.start_server(serve, "127.0.0.1", 0)
        port = server.sockets[0].getsockname()[1]
        async with server, await create(network=EgressPolicy().allow("127.0.0.1", port)) as sb:
            # tmpfs would keep the RAM in memory. (Checked at the first
            # suspension only: later ones write into its file.)
            with pytest.raises(ValueError):
                await sb.suspend("/dev/shm")
            p = await sb.spawn(["sh", "-c", f"wget -q -O - http://10.0.2.1:{port}/ | sha256sum"])
            await asyncio.wait_for(asked.wait(), 30)
            await sb.suspend(RAM_DIR)
            assert sb.suspended
            await asyncio.sleep(0.5)
            assert sb.suspended, "woke before the answer"
            r = await p.wait()
            assert not sb.suspended
            assert r.exit_code == 0, r
            assert r.stdout.split()[0].decode() == hashlib.sha256(body).hexdigest()

            # Nothing to wait for: a call wakes it, and so does resume().
            await sb.suspend(RAM_DIR)
            assert (await sb.exec(["echo", "awake"])).stdout == b"awake\n"
            await sb.suspend(RAM_DIR)
            await sb.resume()
            assert not sb.suspended
            spin = await sb.spawn(["sh", "-c", "while :; do :; done"])
            with pytest.raises(GuestBusyError):
                await sb.suspend(RAM_DIR)
            await spin.kill()

    asyncio.run(main())
