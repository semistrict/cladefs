# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""The bridge every libmachine awaitable goes through, without a guest:
`_libmachine._after(seconds, fail)` completes, or fails, after a delay on
libmachine's tokio runtime, as a sandbox call does."""

import asyncio
import os
import subprocess
import sys
import textwrap

import pytest

from libmachine import _libmachine


def test_results_failures_and_cancellation():
    async def main():
        assert await _libmachine._after(0.01) == 0.01
        with pytest.raises(ValueError, match="failed as asked"):
            await _libmachine._after(0.01, fail=True)
        # A coroutine, so that tasks and task groups take it.
        async with asyncio.TaskGroup() as g:
            tasks = [g.create_task(_libmachine._after(0.001 * i)) for i in range(1, 50)]
        assert [t.result() for t in tasks] == [0.001 * i for i in range(1, 50)]
        with pytest.raises(TimeoutError):
            await asyncio.wait_for(_libmachine._after(10), 0.01)
        # Cancelled calls leave the loop alone: what comes next still runs.
        doomed = [asyncio.ensure_future(_libmachine._after(0.05)) for _ in range(100)]
        await asyncio.sleep(0.01)
        for t in doomed:
            t.cancel()
        assert await _libmachine._after(0.1) == 0.1
        assert all(t.cancelled() for t in doomed)

    asyncio.run(main())


def test_awaiting_needs_a_running_loop():
    with pytest.raises(RuntimeError):
        _libmachine._after(0.01)


EXIT_SCRIPT = """
    import asyncio, random, sys
    from libmachine import _libmachine
    rng = random.Random(int(sys.argv[1]))
    async def main():
        # The failures nobody awaits would each be logged.
        asyncio.get_running_loop().set_exception_handler(lambda loop, context: None)
        calls = [
            asyncio.ensure_future(_libmachine._after(rng.uniform(0, 0.05), fail=rng.random() < 0.3))
            for _ in range(2000)
        ]
        await asyncio.sleep(rng.uniform(0, 0.02))
        for c in calls[::2]:
            c.cancel()
    # asyncio.run cancels what is left and closes the loop; calls still
    # complete, fail and get cancelled after that, as the interpreter
    # finalizes.
    asyncio.run(main())
    print("exiting", flush=True)
"""


def test_exiting_while_calls_complete_fail_and_are_cancelled():
    """Every way a call ends (a result, an exception, a cancellation from
    Python) racing the loop closing and the interpreter finalizing. Any call
    into Python from a tokio thread once the interpreter finalizes crashes
    the process; cancellation was such a path (board 13191)."""
    procs = [
        subprocess.Popen(
            [sys.executable, "-c", textwrap.dedent(EXIT_SCRIPT), str(seed)],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=os.environ,
        )
        for seed in range(40)
    ]
    failures = []
    for seed, p in enumerate(procs):
        out, err = p.communicate(timeout=120)
        if (p.returncode, out) != (0, "exiting\n") or "panicked" in err:
            failures.append(f"seed {seed}: exit {p.returncode}, stdout {out!r}\n{err[-2000:]}")
    assert not failures, "\n".join(failures)
