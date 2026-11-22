# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/cells-per-host.py, which says how many cells a host held, of the cells-per-host
cases' logs in a test-host.sh results directory (board 5586)."""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import script

cells = script.load("cells-per-host")

# A case's log as the case says its steps: two held, then one past fsync's budget twice, and its
# end; the memory line and the settings' among them, which are no steps.
LOG = """\
cells-per-host: Settings { vm_mib: 256 }, dax false
memory: vm=0 phase=base host_used=1
cells-per-host: step=1 vms=1 added=1 room_mib=19000 per_vm_mib=110.5 spawn_s=0.1 ready_s=1.2 boot_p99_ms=900 samples=10 tick_p99_ms=1 tick_max_ms=2 echo_p99_ms=3 echo_max_ms=4 cat_p99_ms=2 cat_max_ms=2 fsync_p99_ms=9 fsync_max_ms=12
cells-per-host: step=2 vms=17 added=16 room_mib=17200 per_vm_mib=105.0 spawn_s=1.0 ready_s=3.4 boot_p99_ms=2100 samples=170 tick_p99_ms=4 tick_max_ms=9 echo_p99_ms=6 echo_max_ms=8 cat_p99_ms=3 cat_max_ms=5 fsync_p99_ms=40 fsync_max_ms=80 guest_reserved_mib=37.0 guest_used_mib=45.0 guest_slab_mib=8.0 guest_stack_mib=1.0 guest_pt_mib=0.5 guest_cached_mib=20.0 guest_memmap_mib=4.0 vmm_pss_mib=70.0 vmm_anon_mib=60.0 vmm_shmem_mib=10.0 vmm_pt_mib=1.0 kvm_pt_mib=3.0 daemon_anon_mib=1.5 daemon_cache_mib=25.0
cells-per-host: step=3 vms=33 added=16 room_mib=15500 per_vm_mib=104.0 spawn_s=1.0 ready_s=3.9 boot_p99_ms=2600 samples=330 tick_p99_ms=7 tick_max_ms=20 echo_p99_ms=9 echo_max_ms=30 cat_p99_ms=5 cat_max_ms=9 fsync_p99_ms=300 fsync_max_ms=900 past=fsync
cells-per-host: step=4 vms=33 added=0 room_mib=15400 per_vm_mib=104.5 spawn_s=0.0 ready_s=0.0 boot_p99_ms=0 samples=330 tick_p99_ms=7 tick_max_ms=19 echo_p99_ms=9 echo_max_ms=31 cat_p99_ms=5 cat_max_ms=8 fsync_p99_ms=310 fsync_max_ms=950 past=fsync
cells-per-host: held=17 limit=latency:fsync
"""


def results_with(logs):
    """A results directory whose cases' logs are `logs`, by case, each its single repeat's."""
    root = Path(tempfile.mkdtemp())
    for case, text in logs.items():
        log = root / "e2e" / case / case / "log.txt"
        log.parent.mkdir(parents=True)
        log.write_text(text)
    return root


class CellsPerHost(unittest.TestCase):
    def test_a_case_held_the_vms_of_its_last_step_within_every_budget(self):
        steps, end = cells.parse(LOG.splitlines())
        self.assertEqual([step["vms"] for step in steps], [1, 17, 33, 33])
        self.assertEqual(steps[2]["past"], "fsync")
        self.assertEqual(end, {"held": 17, "limit": "latency:fsync"})
        self.assertEqual(cells.summary(steps, end), {
            "held": 17, "limit": "latency:fsync", "room_mib": 17200, "per_vm_mib": 105.0,
            "ready_s": 3.4, "boot_p99_ms": 2100, "tick_p99_ms": 4, "echo_p99_ms": 6,
            "cat_p99_ms": 3, "fsync_p99_ms": 40, "guest_reserved_mib": 37.0, "guest_used_mib": 45.0,
            "guest_slab_mib": 8.0, "guest_stack_mib": 1.0, "guest_pt_mib": 0.5,
            "guest_cached_mib": 20.0, "guest_memmap_mib": 4.0, "vmm_pss_mib": 70.0,
            "vmm_anon_mib": 60.0, "vmm_shmem_mib": 10.0, "vmm_pt_mib": 1.0, "kvm_pt_mib": 3.0,
            "daemon_anon_mib": 1.5, "daemon_cache_mib": 25.0,
        })

    def test_a_case_that_said_no_end_has_no_summary(self):
        steps, end = cells.parse(LOG.splitlines()[:-1])
        self.assertEqual(len(steps), 4)
        self.assertIsNone(end)
        self.assertIsNone(cells.summary(steps, end))

    def test_a_run_fails_on_a_case_below_its_floor_or_without_an_end(self):
        results = results_with({"cells-per-host": LOG, "cells-per-host-dax": LOG.replace(
            "held=17 limit=latency:fsync", "held=33 limit=memory")})
        out = results / "cells.json"
        ran = subprocess.run(
            [sys.executable, script.SCRIPTS / "cells-per-host.py", "--json", out,
             "--floor", "cells-per-host=20", "--floor", "cells-per-host-dax=30", results],
            capture_output=True, text=True)
        self.assertEqual(ran.returncode, 1, ran.stderr)
        self.assertEqual(ran.stderr,
                         "cells-per-host: cells-per-host: held 17 VMs, fewer than its floor of 20\n")
        self.assertIn("| cells-per-host | 17 | latency:fsync | 105.0 | 2100 | 4 | 6 | 3 | 40 |",
                      ran.stdout)
        # Each step's memory per VM, in a table of its own: none said by the steps that say none.
        self.assertIn("| 2 | 17 | 37.0 | 45.0 | 8.0 | 1.0 | 0.5 | 20.0 | 4.0 | 70.0 | 60.0 | 10.0 "
                      "| 1.0 | 3.0 | 1.5 | 25.0 |", ran.stdout)
        self.assertIn("| 1 | 1 |  |  |  |  |  |  |  |  |  |  |  |  |  |  |", ran.stdout)
        # The DAX case held 33, its last step's not past a budget: none is, so no memory per VM.
        self.assertIn("| cells-per-host-dax | 33 | memory |  |  |  |  |  |  |", ran.stdout)
        written = json.loads(out.read_text())
        self.assertEqual(written["floors"], {"cells-per-host": 20, "cells-per-host-dax": 30})
        self.assertEqual(written["cases"]["cells-per-host"]["summary"]["held"], 17)

        lost = results_with({"cells-per-host": LOG})
        ran = subprocess.run([sys.executable, script.SCRIPTS / "cells-per-host.py", lost],
                             capture_output=True, text=True)
        self.assertEqual(ran.returncode, 1)
        self.assertEqual(ran.stderr, "cells-per-host: cells-per-host-dax: said no end: it failed "
                                     "or never ran\n")

    def test_a_floor_names_a_case(self):
        ran = subprocess.run([sys.executable, script.SCRIPTS / "cells-per-host.py", "--floor",
                              "nope=3", tempfile.mkdtemp()], capture_output=True, text=True)
        self.assertEqual(ran.returncode, 2)
        self.assertIn("--floor is CASE=N, CASE one of cells-per-host, cells-per-host-dax: not nope=3",
                      ran.stderr)


if __name__ == "__main__":
    unittest.main()
