# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""`cargo nextest run`, as scripts/madsim-seeds.py runs it, for its tests (test_madsim_seeds.py).

$FAKE_NEXTEST/runs.json says each seed's run, by its MADSIM_TEST_SEED: a list of its tests'
outcomes, `[BINARY, TEST, FAILURE]` (FAILURE null for a pass), which it writes to
$FAKE_NEXTEST_JUNIT as nextest 0.9's JUnit report, exiting 100 if any failed; or a status, which it
exits with, writing nothing, as nextest does when it fails before its tests run. A seed it doesn't
name passes. Each call's seed and arguments are appended to $FAKE_NEXTEST/calls, a line of JSON
each.

Usage: python3 fake_nextest.py nextest run ARG...
"""

import json
import os
import sys
from pathlib import Path
from xml.sax.saxutils import escape, quoteattr

TESTS_FAILED = 100


def report(cases):
    """A JUnit report as nextest writes one with its profile madsim, of `cases`: (binary, test,
    failure text or None); a failure's text that of a panic, first, then the test's output."""
    body = []
    for binary, test, text in cases:
        failure = ""
        if text is not None:
            failure = (f"<failure message={quoteattr(text.splitlines()[0])} "
                       f'type="test failure with exit code 101">{escape(text)}</failure>'
                       f"<system-out>running 1 test</system-out>"
                       f"<system-err> WARN a log line\n{escape(text)}</system-err>")
        body.append(f"<testcase name={quoteattr(test)} classname={quoteattr(binary)}>"
                    f"{failure}</testcase>")
    return ('<?xml version="1.0" encoding="UTF-8"?>\n<testsuites name="nextest-run">'
            f'<testsuite name="cladefs::madsim">{"".join(body)}</testsuite></testsuites>\n')


def main(argv):
    if argv[:2] != ["nextest", "run"]:
        sys.exit(f"fake cargo: no such call: {' '.join(argv)}")
    state = Path(os.environ["FAKE_NEXTEST"])
    seed = os.environ["MADSIM_TEST_SEED"]
    with open(state / "calls", "a") as calls:
        print(json.dumps({"seed": seed, "args": argv[2:]}), file=calls)
    run = json.loads((state / "runs.json").read_text()).get(seed, [])
    if isinstance(run, int):
        sys.exit(run)
    if any(text is not None for _, _, text in run):
        Path(os.environ["FAKE_NEXTEST_JUNIT"]).write_text(report(run))
        sys.exit(TESTS_FAILED)


if __name__ == "__main__":
    main(sys.argv[1:])
