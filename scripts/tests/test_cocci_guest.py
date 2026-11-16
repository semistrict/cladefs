# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/cocci-guest.py, the semantic patches of the guest's bugs' shapes: the guest's
C passes them, each finds the mutants it names (TASK-444.6), and the lists of argued matches and
unruled mutants hold the check to them."""

import io
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import script

cocci = script.load("cocci-guest")
DAX = script.SCRIPTS.parent / "guest" / "linux" / "fs" / "fxfs" / "dax.c"


# fxfs_dax_region_open with TASK-99's race: as it was before 7caa43578, its region's lock taken out.
TASK_99_REGION_OPEN = """\
static void fxfs_dax_region_open(struct cladefs_dax *dax, struct cladefs_dax_region *region)
{
\tfxfs_device_assert_in_gate(dax->dev);
\tfxfs_dax_delay();
\tspin_lock(&dax->regions_lock);
\tif (!region->writable && !region->mapped) {
\t\tregion->writable = true;
\t\tlist_add_tail(&region->writable_list, &dax->writable_regions);
\t}
\tspin_unlock(&dax->regions_lock);
}
"""


class RegionLock(unittest.TestCase):
    RULE = cocci.RULES / "region-lock.cocci"

    def test_the_guests_dax_code_reads_regions_under_their_locks(self):
        self.assertEqual(cocci.matches(self.RULE, DAX), "")

    def test_task_99s_unlocked_read_is_flagged(self):
        """TASK-44.17: fxfs_dax_region_open looking at a region's `mapped` under the regions lock
        alone, as before TASK-99's fix. Its code as it was: since 7caa43578 it opens only regions
        nothing has mapped, and takes no region's lock for a mutant to take out."""
        with tempfile.TemporaryDirectory() as scratch:
            path = Path(scratch) / "dax.c"
            path.write_text(TASK_99_REGION_OPEN)
            found = cocci.matches(self.RULE, path)
        flagged = [line for line in found.splitlines() if line.startswith("-\t")]
        self.assertEqual(flagged, ["-\tif (!region->writable && !region->mapped) {"])
        self.assertIn("fxfs_dax_region_open", found)


class TheGuestsC(unittest.TestCase):
    def run_check(self, *args):
        ran = subprocess.run([script.SCRIPTS / "cocci-guest.py", *args], capture_output=True,
                             text=True)
        return ran.returncode, ran.stdout

    def test_it_passes_every_rule_its_argued_matches_aside(self):
        self.assertEqual(self.run_check(), (0, ""))

    def test_each_rule_finds_the_mutants_it_names_and_each_mutant_has_a_rule_or_a_why(self):
        self.assertEqual(self.run_check("--mutants"), (0, ""))


# Two functions bumping a count a wait reads, one waking its waiters.
WAITS = """\
static long waits(struct d *d, long seen)
{
\treturn wait_event_killable_timeout(d->wait, atomic_long_read(&d->count) != seen, 10);
}

static void wakes(struct d *d)
{
\tatomic_long_inc(&d->count);
\twake_up_all(&d->wait);
}

static void wakes_none(struct d *d)
{
\tatomic_long_inc(&d->count);
}
"""


class Lists(unittest.TestCase):
    """The argued matches (scripts/cocci/safe) and unruled mutants (scripts/cocci/unruled), in a
    tree of their own: one rule, unwoken.cocci, and one source."""

    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        (self.root / "rules").mkdir()
        rule = (cocci.RULES / "unwoken.cocci").read_text().splitlines(keepends=True)
        # Without the guest's mutants it names.
        (self.root / "rules/unwoken.cocci").write_text(
            "".join(line for line in rule if not line.startswith("// Mutants:")))
        (self.root / "src").mkdir()
        (self.root / "src/w.c").write_text(WAITS)
        (self.root / "mutants").mkdir()
        for name, value in [("RULES", self.root / "rules"), ("SAFE", self.root / "rules/safe"),
                            ("UNRULED", self.root / "rules/unruled"),
                            ("MUTANTS", self.root / "mutants")]:
            patcher = mock.patch.object(cocci, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def test_a_match_is_named_by_file_function_and_its_first_line(self):
        found = cocci.matches(self.root / "rules/unwoken.cocci", self.root / "src")
        self.assertEqual(cocci.matched_lines(found),
                         [("w.c", "wakes_none", "atomic_long_inc(&d->count);")])

    def test_the_function_is_the_sources_not_the_hunk_headers(self):
        """Linux's spatch names the function a hunk's context starts in, wakes; macOS's the one
        its match is in: the source says which."""
        source = self.root / "src/w.c"
        found = (f"--- {source}\n+++ /tmp/cocci-output-w.c\n"
                 "@@ -10,7 +10,6 @@ static void wakes(struct d *d)\n"
                 " }\n \n static void wakes_none(struct d *d)\n {\n-\tatomic_long_inc(&d->count);\n }\n")
        self.assertEqual(cocci.matched_lines(found),
                         [("w.c", "wakes_none", "atomic_long_inc(&d->count);")])

    def test_an_argued_match_passes_and_an_argued_match_gone_fails(self):
        (self.root / "rules/safe").write_text(
            "# comment\n"
            "unwoken.cocci w.c wakes_none: atomic_long_inc(&d->count); -- nobody waits by then\n")
        self.assertEqual(cocci.safe(), {
            ("unwoken.cocci", "w.c", "wakes_none", "atomic_long_inc(&d->count);"):
                "nobody waits by then"})
        self.assertEqual(self.main([str(self.root / "src")]), (0, ""))
        (self.root / "src/w.c").write_text(
            WAITS.replace("\tatomic_long_inc(&d->count);\n}\n", "}\n"))
        with mock.patch.object(cocci, "SOURCES", [self.root / "src"]):
            self.assertEqual(self.main([]), (1, "scripts/cocci/safe: unwoken.cocci matches no "
                                                "`atomic_long_inc(&d->count);` in w.c's wakes_none\n"))

    def test_an_unargued_match_fails(self):
        (self.root / "rules/safe").write_text("")
        status, said = self.main([str(self.root / "src")])
        self.assertEqual(status, 1)
        self.assertIn("unwoken.cocci matches in", said)
        self.assertIn("-\tatomic_long_inc(&d->count);", said)

    def test_a_mutant_needs_a_rule_or_a_why(self):
        (self.root / "rules/unruled").write_text("# comment\nexplained: a value of its own\n"
                                                 "gone: no such mutant\nbare: \n")
        for name in ["explained", "unexplained", "bare"]:
            (self.root / f"mutants/{name}.patch").write_text("")
        self.assertEqual(cocci.check_mutants(), [
            "unexplained: no rule names it, nor scripts/cocci/unruled",
            "scripts/cocci/unruled: no mutant gone",
            "scripts/cocci/unruled: bare says no why",
        ])

    def test_a_rule_must_match_with_its_mutant_in(self):
        (self.root / "rules/unruled").write_text("")
        rule = self.root / "rules/unwoken.cocci"
        rule.write_text("// Mutants: outside\n" + rule.read_text())
        (self.root / "mutants/outside.patch").write_text(
            "--- a/guest/sim/x.c\n+++ b/guest/sim/x.c\n@@ -1 +1 @@\n-a\n+b\n")
        self.assertEqual(cocci.check_mutants(),
                         ["unwoken.cocci: outside changes guest/sim/x.c, outside the guest's C"])

    def main(self, argv):
        """cocci-guest.py's main with `argv`: its status and what it printed."""
        with mock.patch("sys.argv", ["cocci-guest.py", *argv]), \
                mock.patch("sys.stdout", new_callable=io.StringIO) as out:
            with self.assertRaises(SystemExit) as ended:
                cocci.main()
        return ended.exception.code, out.getvalue()


if __name__ == "__main__":
    unittest.main()
