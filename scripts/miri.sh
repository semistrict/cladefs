#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Runs cladefs's own crates' tests under Miri, which finds undefined behaviour in unsafe code as it
# runs (TASK-444.4): use after free, out-of-bounds and misaligned accesses, data races, aliasing
# broken (Stacked Borrows), a value invalid for its type. On Linux, with the nightly the guest's Rust
# pins and its Miri (the test host's setup installs them on a host of one's own).
#
#   miri.sh [CRATE...]            the tests of scripts/miri-tests, those Miri runs, of the crates
#   miri.sh --probe [CRATE...]    every test of the crates (the own crates with unsafe code by
#                                 default), sorted by how it ends, to write scripts/miri-tests from
#
# Each test runs under nextest's miri profile (.config/nextest.toml: ended after two minutes), with
# the host's files and clock (-Zmiri-disable-isolation), as the tests use temporary directories.
# --probe writes each crate's log to $CLADEFS_MIRI_LOGS (default: a new directory, which it names)
# and prints a line a test: `ub`, Miri found undefined behaviour, a finding; `unsupported`, it
# reached what Miri can't run (a foreign function, a system call Miri doesn't model, inline
# assembly); `passed`; `timeout`; `failed`, otherwise. It fails if a test found undefined behaviour.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
toolchain=$(sed -n 's/^channel = "\(.*\)"$/\1/p' guest/rust/rust-toolchain.toml)
export MIRIFLAGS="${MIRIFLAGS:--Zmiri-disable-isolation}"

probe=""
if [[ ${1:-} == --probe ]]; then
    probe=1
    shift
fi
crates=("$@")

if [[ -z $probe ]]; then
    # scripts/miri-tests: a crate and a test of its a line, `#` comments; each crate's run at once.
    status=0
    for crate in $(awk '!/^#/ && NF {print $1}' scripts/miri-tests | sort -u); do
        (( ${#crates[@]} )) && [[ " ${crates[*]} " != *" $crate "* ]] && continue
        filter=$(awk -v crate="$crate" '$1 == crate {printf "%stest(=%s)", sep, $2; sep = " | "}' \
            scripts/miri-tests)
        cargo "+$toolchain" miri nextest run -p "$crate" --profile miri -E "$filter" || status=1
    done
    exit $status
fi

if (( ${#crates[@]} == 0 )); then
    crates=(virtio_extent block_device extent_frame extent_store extent_remote extent_keys
            fxfs-posix sqlite_cladefs fxfs-sim-diff cladefs-examples cladefs)
fi
logs=${CLADEFS_MIRI_LOGS:-$(mktemp -d)}
mkdir -p "$logs"
echo "logs: $logs" >&2
ub=0
for crate in "${crates[@]}"; do
    log=$logs/$crate.log
    cargo "+$toolchain" miri nextest run -p "$crate" --profile miri --no-fail-fast \
        > "$log" 2>&1 || true
    # Each test's end as nextest says it, then why a failure failed, from its output.
    python3 - "$crate" "$log" <<'PY' || ub=1
import re, sys
crate, log = sys.argv[1], sys.argv[2]
END = re.compile(r"^\s+(PASS|FAIL|TIMEOUT|SIGSEGV|SIGABRT|SIGKILL)\s+\[[^\]]*\]\s+"
                 r"(?:\([^)]*\)\s+)?\S+\s+(\S+)$")
# Each test's end, and the lines nextest prints after it, a failure's output, up to the next end;
# the summary after the run, which says each failure again, aside.
tests, found = [], False
for line in open(log, errors="replace").read().splitlines():
    if line.lstrip().startswith("Summary ["):
        break
    ended = END.match(line)
    if ended:
        tests.append([ended[1], ended[2], []])
    elif tests:
        tests[-1][2].append(line)
for end, test, output in tests:
    output = "\n".join(output) if end != "PASS" else ""
    if end == "PASS":
        fate = "passed"
    elif end == "TIMEOUT":
        fate = "timeout"
    elif "Undefined Behavior" in output:
        fate, found = "ub", True
    elif "unsupported operation" in output or "can't call foreign function" in output:
        fate = "unsupported"
    else:
        fate = "failed"
    print(f"{fate:12} {crate} {test}")
if not tests:
    print(f"{'failed':12} {crate} (no test ran: see {log})")
sys.exit(1 if found else 0)
PY
done
exit $ub
