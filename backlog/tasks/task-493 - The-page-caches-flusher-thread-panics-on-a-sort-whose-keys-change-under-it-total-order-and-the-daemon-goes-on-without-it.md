---
id: TASK-493
title: >-
  The page cache's flusher thread panics on a sort whose keys change under it
  (total order), and the daemon goes on without it
status: To Do
assignee: []
created_date: '2026-11-16 10:31'
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
- [ ] #1 The sorts take each file's key once (a snapshot before sorting) so no concurrent change can break the order, with a test that changes keys during a sort and fails before the fix
- [ ] #2 A panic on the flusher thread (or any page-cache background thread) is not silent: the daemon fails or logs and restarts it as the project's rule for background threads says, and the soak test fails if the flusher died
- [ ] #3 TASK-490's uploads-copied-each-try is shown killed by the soak's own check, not by this panic, on ubu1
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
