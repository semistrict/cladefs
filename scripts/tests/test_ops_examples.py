# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""The reference files of docs/operating-a-host.md (board 8609) against the daemon they are for:
each metric the alerting rules name is one cladefsd serves, as a rule on a metric renamed or gone
never fires and says nothing (promtool, scripts/check-alerts.sh, checks only that the rules load
and fire on series their tests make up); and the unit's stop waits past the drain it configures,
as systemd's SIGKILL at TimeoutStopSec would cut a drain short, its cells left unuploaded."""

import re
import unittest
from pathlib import Path

from script import SCRIPTS

ROOT = SCRIPTS.parent
EXAMPLES = ROOT / "docs" / "examples"
DAEMON = ROOT / "crates" / "cladefs" / "src" / "cladefsd"


def served():
    """The names of the metrics cladefsd serves, as its sources spell them: whole
    (`"cladefsd_host_disk_free_bytes"`), or without the prefix that metrics.rs's gauge tables and
    series add (`"memory_exhausted_arenas",`)."""
    names = set()
    for path in DAEMON.rglob("*.rs"):
        text = path.read_text()
        names.update(re.findall(r'"(cladefsd_[a-z_]+)[ "{]', text))
        if path.name == "metrics.rs":
            names.update("cladefsd_" + name for name in re.findall(r'^\s+"([a-z_]+)",$', text,
                                                                   re.MULTILINE))
    return names


class Examples(unittest.TestCase):
    def test_every_metric_the_alerts_name_is_served(self):
        rules = (EXAMPLES / "cladefsd-alerts.yml").read_text()
        named = set(re.findall(r"\b(cladefsd_[a-z_]+)\b", rules))
        self.assertGreater(len(named), 10, named)
        self.assertEqual(sorted(named - served()), [])

    def test_the_unit_gives_what_jailed_tenants_need_and_no_more(self):
        """Codex's review (hotbed review 69): run as a user of its own with no capability, the
        daemon refused every grant (chown of the tenant's socket to its jails' user: EPERM) and
        every jailed VM process (not its own to signal, Peer::connected)."""
        unit = (EXAMPLES / "cladefsd.service").read_text()
        caps = {"CAP_CHOWN", "CAP_FOWNER", "CAP_KILL"}
        for key in ["AmbientCapabilities", "CapabilityBoundingSet"]:
            line = re.search(rf"^{key}=(.*)$", unit, re.MULTILINE).group(1)
            self.assertEqual(set(line.split()), caps, key)
        self.assertRegex(unit, r"(?m)^User=cladefsd$")
        # What needs them: the grant's chown, mode and link, and the check of a VM process.
        grants = (DAEMON / "mod.rs").read_text()
        self.assertIn("std::os::unix::fs::chown(&bound, uid, gid)?;", grants)
        self.assertIn("std::fs::hard_link(&bound, socket)?;", grants)
        self.assertIn("process.signal(0)", (DAEMON / "server.rs").read_text())

    def test_the_units_stop_outlasts_its_drain(self):
        unit = (EXAMPLES / "cladefsd.service").read_text()
        drain = int(re.search(r"^Environment=CLADEFSD_DRAIN=(\d+)$", unit, re.MULTILINE).group(1))
        stop = int(re.search(r"^TimeoutStopSec=(\d+)$", unit, re.MULTILINE).group(1))
        self.assertGreater(stop, drain)


if __name__ == "__main__":
    unittest.main()
