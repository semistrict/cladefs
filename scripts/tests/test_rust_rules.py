# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests of scripts/rust-rules.py, the rules from the host crates' past bugs (TASK-444.6): the
crates pass them, each finds the mutants it names, every mutant has a rule or a why, and what each
rule takes for its shape."""

import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path
from unittest import mock

import script

rules = script.load("rust-rules")


def matches(rule, source):
    """`rule`'s matches in Rust `source`: (line, rule) each."""
    return [(match.line, match.rule) for match in rule(Path("x.rs"), textwrap.dedent(source))]


class TheCrates(unittest.TestCase):
    def run_rules(self, *args):
        ran = subprocess.run([script.SCRIPTS / "rust-rules.py", *args], capture_output=True,
                             text=True)
        return ran.returncode, ran.stdout

    def test_they_pass_every_rule(self):
        self.assertEqual(self.run_rules(), (0, ""))

    def test_each_rule_finds_the_mutants_it_names_and_each_mutant_has_a_rule_or_a_why(self):
        self.assertEqual(self.run_rules("--mutants"), (0, ""))


class OutsideTheSeam(unittest.TestCase):
    """cladefs: the daemon's threads, runtimes and blocking waits go through its seam (TASK-685.3)."""

    def matches(self, source, path="crates/cladefs/src/cladefsd/x.rs"):
        return [(match.line, match.rule)
                for match in rules.outside_the_seam(Path(path), textwrap.dedent(source))]

    def test_a_thread_of_the_daemons_is_flagged(self):
        self.assertEqual(self.matches("""\
            fn start() {
                std::thread::Builder::new().name("x".into()).spawn(work).unwrap();
                futures::executor::block_on(waiting);
            }
            """), [(2, "outside-the-seam"), (3, "outside-the-seam")])

    def test_one_saying_why_it_is_outside_the_simulation_passes(self):
        self.assertEqual(self.matches("""\
            fn start() {
                // cladefs: outside the simulation: the VM socket.
                std::thread::Builder::new().name("x".into()).spawn(work).unwrap();
            }
            """), [])

    def test_the_seam_its_tests_and_other_crates_pass(self):
        source = "fn f() { tokio::task::spawn_blocking(work); }\n"
        self.assertEqual(self.matches(source, "crates/cladefs/src/cladefsd/run.rs"), [])
        self.assertEqual(self.matches(source, "crates/cladefs/src/cladefsd/tests.rs"), [])
        self.assertEqual(self.matches(source, "crates/cladefs/src/fuse/x.rs"), [])
        self.assertEqual(self.matches("""\
            fn f() {}
            #[cfg(test)]
            mod tests {
                fn t() { std::thread::spawn(work); }
            }
            """), [])


class MixedOrderings(unittest.TestCase):
    def test_a_counter_all_relaxed_passes(self):
        self.assertEqual(matches(rules.mixed_orderings, """\
            fn count(&self) { self.hits.fetch_add(1, Ordering::Relaxed); }
            fn hits(&self) -> u64 { self.hits.load(Ordering::Relaxed) }
            """), [])

    def test_a_synchronizers_relaxed_operation_is_flagged(self):
        self.assertEqual(matches(rules.mixed_orderings, """\
            fn publish(&self) { self.ready.store(true, Ordering::Release); }
            fn ready(&self) -> bool { self.ready.load(Ordering::Acquire) }
            fn peek(&self) -> bool {
                self.ready.load(Ordering::Relaxed)
            }
            """), [(4, "mixed-orderings")])

    def test_a_comment_saying_why_lets_it_be(self):
        self.assertEqual(matches(rules.mixed_orderings, """\
            fn publish(&self) { self.ready.store(true, Ordering::Release); }
            /// Whether it is ready. Relaxed, as it publishes nothing beside itself.
            fn peek(&self) -> bool {
                self.ready.load(Ordering::Relaxed)
            }
            """), [])

    def test_an_operation_over_lines_is_read_whole(self):
        self.assertEqual(matches(rules.mixed_orderings, """\
            fn publish(&self) { self.ready.store(true, Ordering::Release); }
            fn take(&self) -> bool {
                self.ready
                    .compare_exchange(true, false, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            }
            """), [(3, "mixed-orderings")])


class Patterns(unittest.TestCase):
    def test_an_unbounded_semaphore_is_flagged_and_a_comment_naming_it_is_not(self):
        rule = next(rule for rule, _ in rules.RULES if rule.__name__ == "unbounded-semaphore")
        self.assertEqual(matches(rule, """\
            // Semaphore::MAX_PERMITS is no bound.
            let page_ins = Semaphore::new(Semaphore::MAX_PERMITS);
            """), [(2, "unbounded-semaphore")])


class Mutants(unittest.TestCase):
    """--mutants, in a tree of its own: one crate's file, its mutants, the unruled list."""

    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        (self.root / "crates/c/src").mkdir(parents=True)
        (self.root / "crates/c/mutants").mkdir()
        (self.root / "crates/c/src/lib.rs").write_text(
            "fn f(&self) {\n    self.ready.store(true, Ordering::Release);\n}\n"
            "fn g(&self) -> bool {\n    self.ready.load(Ordering::Acquire)\n}\n")
        (self.root / "unruled").write_text("")
        for name, value in [("ROOT", self.root), ("UNRULED", self.root / "unruled")]:
            patcher = mock.patch.object(rules, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def mutant(self, name, old, new):
        (self.root / f"crates/c/mutants/{name}.patch").write_text(
            "--- a/crates/c/src/lib.rs\n+++ b/crates/c/src/lib.rs\n@@ -4,3 +4,3 @@\n"
            f" fn g(&self) -> bool {{\n-{old}\n+{new}\n }}\n")

    def test_a_rule_must_match_with_its_mutant_in(self):
        self.mutant("relaxed", "    self.ready.load(Ordering::Acquire)",
                    "    self.ready.load(Ordering::Relaxed)")
        self.mutant("same", "    self.ready.load(Ordering::Acquire)",
                    "    self.ready.load(Ordering::SeqCst)")
        with mock.patch.object(rules, "RULES", [(rules.mixed_orderings, ["relaxed", "same"])]):
            self.assertEqual(rules.check_mutants(),
                             ["mixed_orderings: matches nothing with same in"])

    def test_a_mutant_needs_a_rule_or_a_why(self):
        self.mutant("explained", "x", "y")
        self.mutant("unexplained", "x", "y")
        (self.root / "unruled").write_text("# comment\nexplained: a value\ngone: none such\n")
        with mock.patch.object(rules, "RULES", []):
            self.assertEqual(rules.check_mutants(), [
                "unexplained: no rule names it, nor scripts/rust-rules/unruled",
                "scripts/rust-rules/unruled: no mutant gone",
            ])


if __name__ == "__main__":
    unittest.main()
