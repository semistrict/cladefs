---
id: TASK-650
title: >-
  Daemon tests abort with SIGABRT at process exit after passing (a different
  test each run)
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18 21:17'
updated_date: '2026-11-20 21:08'
labels:
  - cladefsd
  - flaky
dependencies: []
priority: medium
type: bug
ordinal: 1032000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
On ubu1 (2026-11-18), three full workspace runs each had one test process abort with SIGABRT after its test reported ok: cladefs::tenants tenants_hold_their_own_pages (twice, on main 6b7b54e24 too), cladefsd::tests::snapshots_after_private_pages_are_rebound_restore_their_bytes once. stderr shows nothing but the DAX window INFO lines, then "(test aborted with signal 6: SIGABRT)", so the abort is in teardown: a destructor, a thread still running as the process exits (the daemon's serve thread, uffd fault threads), or a runtime dropped from within itself. Unrelated to TASK-633.9 (seen on main). LIBC_FATAL_STDERR_=1 is set by test.sh; no glibc message was printed.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The cause of the exit-time abort is found (core dump or a backtrace on abort under the test harness)
- [x] #2 The affected tests exit cleanly in 50 repeated runs under full-suite load on ubu1
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Collect core dumps on ubu1 (ulimit -c unlimited) from a stress run of the aborting tests; backtrace them.
2. Gate our random draws at process exit so none runs during aws-lc's teardown.
3. Prove it: a child process exiting as its threads draw, which aborts without the gate; the stress run clean.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Seen again on ubu1 2026-11-19 (branch t3/mirrors at 439a7068c, TASK-633.8): cladefs::tenants tenants_hold_their_own_pages SIGABRT after 'test result: ok', stderr only the DAX window INFO lines; 3072/3073 otherwise. The run on 50933bf33 before it was clean (3069/3069).

Seen 2026-11-20 on ubu1 (t3/sim-spike, scripts/test.sh workspace under fxjob, --test-threads 4): cladefsd::tests::snapshots_after_private_pages_are_rebound_restore_their_bytes passed, then SIGABRT at exit; passed alone right after.

2026-11-20 on ubu1: cladefs's snapshots_after_private_pages_are_rebound_restore_their_bytes and tenants::tenants_hold_their_own_pages pass, then SIGABRT at exit; stressed together 60 times: main e052b73f 5 failed, t3/async-1 2 failed. Both seen in full workspace runs.

2026-11-20 (TASK-691 Linux run, ubu1, f4f4f4c78): cladefs::disk tenants_past_their_disk_limits_are_refused_while_others_write passed, then SIGABRT at exit.

2026-11-20: seen again on ubu1 (async rewrite's guest transport batch): cladefsd::tests::shrinks_return_once_the_pages_they_evict_are_free reported ok, then SIGABRT; its stderr's last line 'letting a VM process go, as it stopped answering stage operations' (the test's own unresponsive VM process). Passed 3/3 alone.

Cause (2026-11-20, 8 cores of 60 stress rounds on ubu1): the abort is aws-lc's. As the process exits (_dl_fini), aws-lc's ELF destructors clear every thread's random generator (rand_thread_local_state_clear_all, which write-locks each for good) and free its jitter seed DRBG (tree_jitter_free_global_drbg); a thread drawing then aborts: tree_jitter_drbg_derive_seed with a NULL DRBG (a tokio blocking-pool thread's first draw), or CRYPTO_MUTEX_lock_read in rand_bytes_core. The drawer each time: an uploader still writing a checkpoint (Salt::random in extent_frame::Packer::keyed) after the test returned. aws-lc's own docs say the program must stop its threads first; upstream main is unchanged. Tried AWS_LC_SYS_NO_JITTER_ENTROPY=1 (as Firecracker builds it): only moved the abort (2 of 60), reverted. Fix: extent_keys::random::fill, through which every draw of ours goes (Secret::random, Salt::random, TenantKek IDs); an atexit handler, registered with the first draw and run before the ELF destructors, stops new draws and waits for those under way (SeqCst counter); a thread drawing after parks until the process ends. Nothing waits on uploaders, so exit cannot hang. Test: random::tests::processes_exit_cleanly_as_their_threads_draw (20 children, 8 threads drawing and 8 starting fresh drawing threads as each exits): on ubu1 the gate's hand mutant aborts (round 2, SIGABRT), the fix passes; the Mac's aws-lc does not abort either way. ubu1 stress (the four tests, 60 rounds): 60/60, no cores (8 of 60 aborted before).

Verified 2026-11-20 at 6cdd8528a: ubu1 workspace 3159/3159; VMM and guest builds clean; macOS workspace 2794/2794; upstream-changes --check passes; clippy gate clean.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
SIGABRT after passing tests was aws-lc aborting draws made after its exit-time teardown of random generators, by uploaders still writing checkpoints. Every draw now goes through extent_keys::random::fill, gated by an atexit handler that runs before aws-lc's destructors. Verified: the affected tests 60/60 stress rounds on ubu1 (8 of 60 aborted before), a child-exit test that aborts without the gate, workspaces on macOS and Linux.
<!-- SECTION:FINAL_SUMMARY:END -->
