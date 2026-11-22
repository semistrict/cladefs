#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""The findings a network-isolated reviewer's report carries for hotbed (scripts/review-change.sh).

Such a reviewer can't reach hotbed, so it ends its report with a ```json hotbed-findings block
holding {"findings": [...]}, as `hotbed review findings` takes them. This writes that payload to
FINDINGS, unless the list is empty (hotbed refuses an empty one: the review's summary says so), and
prints a one-line summary for `hotbed review finish`. A report without the block, or
with one that isn't that shape, fails: a reviewer that forgot it hasn't said it found nothing.

    scripts/review-findings.py REPORT FINDINGS
"""

import json
import re
import sys
from pathlib import Path

BLOCK = re.compile(r"^```json[ \t]+hotbed-findings[ \t]*\n(.*?)\n```[ \t]*$", re.S | re.M)


def findings(report):
    """The findings list REPORT's last hotbed-findings block holds; ValueError if there is none."""
    blocks = BLOCK.findall(report)
    if not blocks:
        raise ValueError("the report has no ```json hotbed-findings block")
    payload = json.loads(blocks[-1])
    if not isinstance(payload, dict) or not isinstance(payload.get("findings"), list):
        raise ValueError('the hotbed-findings block isn\'t {"findings": [...]}')
    return payload["findings"]


def summary(report, count):
    """The report's first line of prose, with how many findings it reported."""
    prose = (line.strip() for line in report.splitlines())
    head = next((line for line in prose if line and not line.startswith("```")), "")
    if len(head) > 240:
        head = head[:237] + "..."
    return f"Codex review, {count} finding(s): {head}"


def main(argv):
    if len(argv) != 3:
        print("usage: review-findings.py REPORT FINDINGS", file=sys.stderr)
        return 2
    report = Path(argv[1]).read_text(encoding="utf-8")
    try:
        found = findings(report)
    except ValueError as err:  # json.JSONDecodeError is one too
        print(f"review-findings.py: {err}", file=sys.stderr)
        return 1
    if found:
        Path(argv[2]).write_text(json.dumps({"findings": found}), encoding="utf-8")
    print(summary(report, len(found)))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
