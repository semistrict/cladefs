---
id: TASK-493
title: >-
  The page cache's flusher thread panics on a sort whose keys change under it
  (total order), and the daemon goes on without it
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-16 10:31'
updated_date: '2026-11-16 11:47'
labels:
  - bug
dependencies: []
priority: high
ordinal: 884000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found 2026-11-16 on ubu1 running extentd::tests::soak::a_degraded_bucket_costs_the_daemon_nothing_it_does_not_bound on unchanged code (linux-batch at c5998bfb9, main's pages.rs from TASK-433.5, 38c8470dd): `thread 'store flusher' panicked at .../core/src/slice/sort/shared/smallsort.rs:854:5: user-provided comparison function does not correctly implement a total order` during the slow-bucket phase, and the test still PASSED. crates/extent_store/src/pages.rs sorts the cache's files with `files.sort_by_key(|file| file.used())` (evict_to) and `files.sort_by_key(|file| file.dirtied())` (twice), and each key is read live during the sort: `used()` is `self.used.load(Ordering::Relaxed)` and `dirtied()` locks the file's pages for `dirtied`, both changed by other threads meanwhile, so the comparison is not a total order and Rust's sort panics. The flusher thread then dies: the process's page cache has no flusher from then on (dirty pages written back only by writers held at the bound, nothing expiring), and nothing notices. It also made the hand mutant uploads-copied-each-try (TASK-490) look killed when the panic, not the soak's budget check, failed the run.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The sorts take each file's key once (a snapshot before sorting) so no concurrent change can break the order, with a test that changes keys during a sort and fails before the fix
- [x] #2 A panic on the flusher thread (or any page-cache background thread) is not silent: the daemon fails or logs and restarts it as the project's rule for background threads says, and the soak test fails if the flusher died
- [x] #3 TASK-490's uploads-copied-each-try is shown killed by the soak's own check, not by this panic, on ubu1
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Red test first: Shifting fake files whose keys change on every read (scrambled), through evict_to and write_back_to (then the flusher's pass): Rust's sort panics as on ubu1.
2. PageCache::files_by(key): each file's key taken once, then sorted; evict_to, write_back_to and the flusher's pass (PageCache::flush_pass, extracted) order by it.
3. Shared::pages(): pages lock tolerating poison without counting a use; dirtied()/evict_clean() use it (a writer's panic otherwise killed the flusher next).
4. Dead flusher: shared::Holding::work(FLUSHER) held for the flusher's life, so the daemon's abort_on_panics hook aborts on its panic (TASK-149.24's rule); elsewhere Dying records flusher_died, logs error, wakes writers; throttle panics if the flusher died instead of waiting for good. Tests: child-process SIGABRT naming the flusher; no-hook death known and throttle panics.
5. Soak: abort_on_panics as the daemon, flusher_lives() each tick and at the end.
6. Search every sort/max_by/min_by/BinaryHeap/select over shared keys in extent_store, extent_remote, cladefs.
7. clippy as CI, nextest workspace + shuttle part on macOS; soak on ubu1 by the parent.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Blind spot: no test ordered files whose keys change during the sort (pages.rs tests had one file, or files idle while ordering); the flusher's death had no observer (thread result never read, no hook marker), so the soak passed.
Class test: pages::tests::files_ordered_by_keys_changing_as_they_are_read: Shifting fake files whose keys (used, dirtied) change on every read of any file (scrambled counter), 0..=256 files, through evict_to, write_back_to and flush_pass. Red before the fix with the ubu1 panic (smallsort.rs:854 "does not correctly implement a total order"); a monotone key did not trip detection, a scrambled one does.
Structure: the cache orders files only through PageCache::files_by (keys taken once, then sorted); the flusher's pass extracted to PageCache::flush_pass so tests run it without the thread. Also Shared::pages(): dirtied()/evict_clean() unwrapped the pages lock, so a writer's panic poisoning it killed the flusher at its next pass; now poison-tolerant as lock() already was.
Dead flusher: shared::Holding (the HELD marker, now also Shared<G>'s) with Holding::work(FLUSHER) held for the flusher thread's life: cladefsd's abort_on_panics aborts at its panic naming "page cache flusher" (TASK-149.24's rule; decision in OPEN_QUESTIONS.md). Without the hook: Dying guard sets flusher_died, logs an error, wakes writers; throttle panics instead of waiting for good. Tests: a_panic_on_the_flusher_aborts_the_daemon (child process SIGABRT naming it; red with the Holding line removed: the child lived on), a_dead_flusher_is_known_and_holds_no_writer_for_good (red with Dying removed).
Soak: abort_on_panics as cladefsd does, flusher_lives() each second and at the end. Linux-only: cross-checked (cargo check --target aarch64-unknown-linux-musl -p cladefs --lib --profile test); not run here (ubu1).
Look-around (sort, sort_by*, sort_unstable*, sort_by_cached_key, max_by*, min_by*, BinaryHeap, select_nth, binary_search_by over extent_store, extent_remote, cladefs src): only pages.rs read live keys. Others sort owned values or snapshot tuples: cladefs cache.rs evict_in_order and let_go_least_used (keys collected into tuples first), dax/uffd.rs runs (tuples), extent_store views.rs min_by_key (under the views lock; min/max_by_key compute each key once anyway), extent_remote recovery.rs max_by_key (under the kept lock), extent_remote cache.rs found.modified (metadata snapshot), store/collector/passed/sweep/keys/memory/disk/control (fields of owned items). No BinaryHeap or select_nth. A syntactic rust-rules.py rule was tried and dropped (dead-ends/sort-keys-syntactic-rule.md: it flagged a pure accessor in fxfs-posix).

macOS (6c1914594): clippy part as CI (fmt, workspace -D disallowed_methods -D warnings, shuttle and loom builds) clean; workspace nextest 2538/2538; shuttle part 37+10+6+33+10+18+loom 2+1+1 all passed. sim part not run. Open: AC #2's soak run and AC #3 need ubu1; DoD Linux run too.

2026-11-16 ubu1 (t493-int, main 4a3283a59 + TASK-493): clippy and workspace 2856 of 2857 passed, the one failure fuse_differential's directory ctime (TASK-495, unrelated: TASK-493 touches the page cache and the soak only); the soak on this tree passed in 311 s with no flusher panic; mutants.sh uploads-copied-each-try killed in 249 s by the soak's own check: 'assertion left == right failed: 334 MiB of anonymous memory, past its budget of 3201 MiB' (soak.rs:262).
<!-- SECTION:NOTES:END -->
