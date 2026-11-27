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

    def matches(self, source, path="crates/cladefsd/src/x.rs"):
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
        self.assertEqual(self.matches(source, "crates/cladefsd/src/run.rs"), [])
        self.assertEqual(self.matches(source, "crates/cladefsd/src/tests.rs"), [])
        self.assertEqual(self.matches(source, "crates/cladefs_fuse/src/x.rs"), [])
        self.assertEqual(self.matches("""\
            fn f() {}
            #[cfg(test)]
            mod tests {
                fn t() { std::thread::spawn(work); }
            }
            """), [])


class OffHostFs(unittest.TestCase):
    """cladefs: the daemon's disk goes through host_fs, each simulated host's disk under madsim
    (TASK-685.4)."""

    def matches(self, source, path="crates/local_cell/src/x.rs"):
        return [(match.line, match.rule)
                for match in rules.off_host_fs(Path(path), textwrap.dedent(source))]

    def test_std_fs_its_traits_and_paths_asking_the_disk_are_flagged(self):
        self.assertEqual(self.matches("""\
            use std::os::unix::fs::FileExt as _;
            fn f(path: &Path) -> bool {
                let _ = std::fs::read(path);
                path.exists() && path.canonicalize().is_ok()
            }
            """), [(1, "off-host-fs"), (3, "off-host-fs"), (4, "off-host-fs")])

    def test_host_fs_and_one_saying_why_it_is_outside_the_simulation_pass(self):
        self.assertEqual(self.matches("""\
            fn f(path: &Path) -> bool {
                let _ = host_fs::read(path);
                // cladefs: outside the simulation: procfs.
                let _ = std::fs::read("/proc/self/status");
                host_fs::exists(path)
            }
            """), [])

    def test_files_outside_the_simulation_tests_and_other_crates_pass(self):
        source = "fn f() { let _ = std::fs::read(\"x\"); }\n"
        self.assertEqual(self.matches(source, "crates/cladefs_fuse/src/x.rs"), [])
        self.assertEqual(self.matches(source, "crates/cladefs/src/cladefsd/tests.rs"), [])
        self.assertEqual(self.matches(source, "crates/virtio_extent/src/x.rs"), [])
        self.assertEqual(self.matches("""\
            fn f() {}
            #[cfg(test)]
            mod tests {
                fn t() { let _ = std::fs::read("x"); }
            }
            """), [])
        self.assertEqual(self.matches("""\
            fn f() {}
            #[cfg(test)]
            pub(crate) mod tests {
                fn t() { let _ = std::fs::read("x"); }
            }
            """), [])

    def test_a_module_compiled_only_in_tests_passes_and_one_compiled_beside_them_is_flagged(self):
        """Board 6490: timings.rs's tests are `all(test, target_os = "linux")`."""
        def module(cfg):
            return self.matches(f"""\
                #[cfg({cfg})]
                mod tests {{
                    fn t() {{ let _ = std::fs::read("x"); }}
                }}
                """)

        self.assertEqual(module('all(test, target_os = "linux")'), [])
        self.assertEqual(module("any(test, all(test, madsim))"), [])
        self.assertEqual(module("any(test, madsim)"), [(3, "off-host-fs")])
        self.assertEqual(module("not(test)"), [(3, "off-host-fs")])
        self.assertEqual(module('target_os = "linux"'), [(3, "off-host-fs")])

    def test_a_test_module_in_a_file_of_its_own_leaves_the_lines_after_it(self):
        """Board 6908: `#[cfg(test)] mod sim;` is sim.rs's alone."""
        self.assertEqual(self.matches("""\
            #[cfg(test)]
            mod sim;
            fn g() { let _ = std::fs::read("y"); }
            """), [(3, "off-host-fs")])

    def test_a_test_module_within_a_module_passes_to_its_end_alone(self):
        """Board 6105: migration.rs's tests are a module within one of the file's."""
        self.assertEqual(self.matches("""\
            mod target {
                fn f() {}
                #[cfg(test)]
                mod tests {
                    fn t() {
                        let _ = std::fs::read("x");
                    }
                }
                fn g() { let _ = std::fs::read("y"); }
            }
            """), [(9, "off-host-fs")])


class SilentFallback(unittest.TestCase):
    """cladefs: an error dropped says why, or fails (board 6908)."""

    def matches(self, source, path="/nowhere/crates/cladefs/src/x.rs"):
        return [(match.line, match.rule)
                for match in rules.silent_fallback(Path(path), textwrap.dedent(source))]

    def test_each_shape_of_an_error_dropped_is_flagged(self):
        self.assertEqual(self.matches("""\
            fn f() -> u64 {
                match slots() {
                    Ok(n) => n,
                    Err(_) => 1,
                }
                let n = count().ok();
                let n = count().unwrap_or_else(|_| 0);
                let n = listed.await.unwrap_or_default();
                let n = listed
                    .await
                    .unwrap_or_default();
                let _ = flushed(epoch);
                let _ = task.await;
            }
            """), [(4, "silent-fallback"), (6, "silent-fallback"), (7, "silent-fallback"),
                   (8, "silent-fallback"), (11, "silent-fallback"), (12, "silent-fallback"),
                   (13, "silent-fallback")])

    def test_one_saying_why_passes_on_its_line_or_over_its_statement(self):
        self.assertEqual(self.matches("""\
            fn f() -> u64 {
                match slots() {
                    Ok(n) => n,
                    // Unformatted: the default.
                    Err(_) => 1,
                }
                let n = count().ok(); // Not a number: none.
                // Its waiter gone: nothing to tell.
                let _ = done.send(());
                // A clock gone back: none waited.
                let waited = seen
                    .elapsed()
                    .ok();
                let more = check()
                    && let Some(len) = bytes.u64().and_then(|len| usize::try_from(len).ok());
            }
            """), [(15, "silent-fallback")])

    def test_an_error_given_on_or_a_panic_is_no_fallback(self):
        self.assertEqual(self.matches("""\
            fn f() -> Result<u64, E> {
                match slots() {
                    Ok(n) => Ok(n),
                    Err(_) => return Err(E::Slots),
                    Err(..) => panic!("no slots"),
                }
                let n = within.unwrap_or_else(|_| Err(out_of_reach()));
                let n = task.await.unwrap_or_else(|err| {
                    Err(E::Joined(err))
                });
                match asked {
                    Err(_) => {
                        return Err(E::TimedOut);
                    }
                }
                assert!(matches!(found, Err(_)));
            }
            """), [])

    def test_bindings_and_writes_dropped_are_no_errors(self):
        self.assertEqual(self.matches("""\
            fn rename(&self, from: &str, to: &str) -> io::Result<()> {
                let _ = (from, to);
                let _ = &self.handle;
                let _ = cell;
                let _ = writeln!(out, "{name} {value}");
                let _ =
                    writeln!(out, "{name}");
                Err(io::Error::from(io::ErrorKind::Unsupported))
            }
            """), [])

    def test_tests_simulations_and_other_crates_pass(self):
        source = "fn f() { let n = count().ok(); }\n"
        self.assertEqual(self.matches(source), [(1, "silent-fallback")])
        self.assertEqual(self.matches(source, "/nowhere/crates/fxfs/src/x.rs"), [])
        self.assertEqual(self.matches(source, "/nowhere/crates/cladefs/src/cladefsd/tests.rs"), [])
        self.assertEqual(self.matches(source, "/nowhere/crates/cladefs/tests/x.rs"), [])
        self.assertEqual(self.matches(source, "/nowhere/crates/cladefs/src/x_tests.rs"), [])
        self.assertEqual(self.matches("""\
            fn f() {}
            #[cfg(test)]
            fn quickened() -> Option<Pace> {
                PACE.try_with(|pace| *pace).ok()
            }
            #[cfg(madsim)]
            {
                let _ = (most, work);
            }
            #[cfg(any(test, feature = "testing"))]
            pub mod testing {
                fn t() { let n = count().ok(); }
            }
            fn g() { let n = count().ok(); }
            """), [(14, "silent-fallback")])

    def test_an_item_of_tests_ends_where_its_brackets_close(self):
        """Board 6908: local_cell's `compile_error!(..)` under `all(feature = "testing", ..)` hid
        every module declared after it until a closing brace at the file's top."""
        self.assertEqual(self.matches("""\
            #[cfg(all(feature = "testing", not(debug_assertions)))]
            compile_error!(
                "testing in a release build"
            );
            fn f() { let n = count().ok(); }
            #[cfg(test)]
            fn g() {
                let n = count().ok();
            }
            fn h() { let n = count().ok(); }
            """), [(5, "silent-fallback"), (10, "silent-fallback")])

    def test_a_file_declared_only_in_tests_passes(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        src = Path(scratch.name) / "crates/local_cell/src"
        (src / "sim").mkdir(parents=True)
        (src / "lib.rs").write_text("#[cfg(test)]\nmod sim;\nmod live;\n")
        (src / "sim/mod.rs").write_text("mod world;\n")
        source = "fn f() { let n = count().ok(); }\n"
        for name in ["sim/mod.rs", "sim/world.rs", "live.rs"]:
            (src / name).write_text(source) if name != "sim/mod.rs" else None
        self.assertEqual(self.matches(source, src / "sim/world.rs"), [])
        self.assertEqual(self.matches(source, src / "live.rs"), [(1, "silent-fallback")])


class ProcessWideStatic(unittest.TestCase):
    """cladefs: a daemon's static is per node, or says why it is the process's (board 7583)."""

    def matches(self, source, path="/nowhere/crates/local_cell/src/x.rs"):
        return [(match.line, match.rule)
                for match in rules.process_wide_static(Path(path), textwrap.dedent(source))]

    def test_statics_every_simulated_host_would_share_are_flagged(self):
        self.assertEqual(self.matches("""\
            /// The parts unpacked lately.
            static UNPACKED: Unpacked = Unpacked::new();
            fn f() {
                static NEXT: AtomicU64 = AtomicU64::new(0);
            }
            thread_local! {
                static SPARE: Cell<Option<Buffer>> = const { Cell::new(None) };
            }
            """), [(2, "process-wide-static"), (4, "process-wide-static"),
                   (7, "process-wide-static")])

    def test_task_local_plain_and_explained_statics_pass(self):
        self.assertEqual(self.matches("""\
            tokio::task_local! {
                static GONE: Shared<Receiver<()>>;
            }
            static NAME: &str = "cladefsd";
            static SIZES: [u64; 3] = [1, 2, 3];
            /// The process's count, every simulated host's together.
            static SERVED: AtomicU64 = AtomicU64::new(0);
            #[cfg(not(madsim))]
            static MOUNT: Mutex<Option<Mount>> = Mutex::new(None);
            thread_local! {
                // Per thread, set and cleared within one call.
                static CURRENT: Cell<Option<u64>> = const { Cell::new(None) };
            }
            """), [])

    def test_tests_and_crates_the_simulation_runs_not_pass(self):
        source = "static NEXT: AtomicU64 = AtomicU64::new(0);\n"
        self.assertEqual(self.matches(source), [(1, "process-wide-static")])
        self.assertEqual(self.matches(source, "/nowhere/crates/virtio_extent/src/x.rs"), [])
        self.assertEqual(self.matches(source, "/nowhere/crates/cladefs/src/bin/cladefsctl.rs"), [])
        self.assertEqual(self.matches(source, "/nowhere/crates/cladefs/src/cladefsd/tests.rs"), [])
        self.assertEqual(self.matches("""\
            #[cfg(feature = "shuttle")]
            static RULES: Mutex<Rules> = Mutex::new(Rules::ALL);
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
