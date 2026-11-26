#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Runs each Rust libtest case in its own subprocess with Bazel sharding support.

Matches cargo-nextest's per-test process isolation for crates whose unit tests
mutate process-wide state (log::set_logger, panic hooks, global fault settings,
or allocation caps) and advertises Bazel TEST_SHARD_STATUS_FILE support.
"""

from concurrent.futures import ThreadPoolExecutor, as_completed
import os
import re
import subprocess
import sys


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: run_isolated_rust_test.py <test-binary> [libtest-args...]", file=sys.stderr)
        return 2

    binary = sys.argv[1]
    extra_args = sys.argv[2:]

    shard_status = os.environ.get("TEST_SHARD_STATUS_FILE")
    if shard_status:
        with open(shard_status, "wb"):
            pass

    list_cmd = [binary, "--list", "--format", "terse", *extra_args]
    listed = subprocess.run(list_cmd, capture_output=True, text=True, check=False)
    if listed.returncode != 0:
        sys.stderr.write(listed.stderr)
        sys.stdout.write(listed.stdout)
        return listed.returncode

    tests = []
    for line in listed.stdout.splitlines():
        if line.endswith(": test"):
            tests.append(line[: -len(": test")])

    test_filter = os.environ.get("TESTBRIDGE_TEST_ONLY")
    if test_filter:
        pattern = re.compile(test_filter)
        tests = [t for t in tests if pattern.search(t)]

    total_shards = int(os.environ.get("TEST_TOTAL_SHARDS", "1"))
    shard_index = int(os.environ.get("TEST_SHARD_INDEX", "0"))
    selected = [t for i, t in enumerate(tests) if i % total_shards == shard_index]

    workers = max(1, int(os.environ.get("CLADEFS_ISOLATED_JOBS", "4")))
    failures: list[tuple[str, int, str, str]] = []
    passed = 0

    def run_one(name: str) -> tuple[str, int, str, str]:
        proc = subprocess.run(
            [binary, "--exact", "--nocapture", name],
            capture_output=True,
            text=True,
            check=False,
        )
        return name, proc.returncode, proc.stdout, proc.stderr

    with ThreadPoolExecutor(max_workers=workers) as pool:
        futures = [pool.submit(run_one, name) for name in selected]
        for fut in as_completed(futures):
            name, rc, out, err = fut.result()
            if rc == 0:
                passed += 1
            else:
                failures.append((name, rc, out, err))

    for name, rc, out, err in sorted(failures):
        print(f"FAIL {name} (exit {rc}):", file=sys.stderr)
        if out:
            sys.stderr.write(out)
        if err:
            sys.stderr.write(err)

    print(
        f"shard {shard_index + 1}/{total_shards}: "
        f"{passed} passed, {len(failures)} failed (of {len(selected)} tests)"
    )
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
