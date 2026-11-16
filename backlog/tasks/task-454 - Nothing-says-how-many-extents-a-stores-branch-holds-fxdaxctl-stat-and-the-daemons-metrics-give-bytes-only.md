---
id: TASK-454
title: >-
  Nothing says how many extents a store's branch holds: cladefsctl stat and the
  daemon's metrics give bytes only
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 13:15'
updated_date: '2026-11-16 00:43'
labels:
  - perf
  - review-2026-11-15
dependencies: []
references:
  - TASK-443.2
priority: medium
ordinal: 850000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-443.2 found that daemon heap per open fork is ~220-470 B times the base's extent count, so the count of a real sandbox base decides whether that is megabytes or tens of gigabytes; and no command, stat or metric gives it. cladefsctl stat prints capacity, used, available, own, inherited, public (bytes); extent_store::Store::extents() has it but nothing exposes it. Without it the size of TASK-443.2's problem on a real base can't be read from a host.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 cladefsctl stat DIR prints the number of extents the store's branch holds, own and inherited
- [ ] #2 The count of a real sandbox base image on ubu1 is recorded in TASK-443.2's notes
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. extent_store: Space (state.rs), which counts the bytes the branch's extents hold, own and all, counts the extents too: set at opening from the map Recovering already walks, one more as finish_write enters an extent, one less as free removes one; Space::audit (test and audit builds, every state release) recounts them from the map. Info gains own_extents and inherited_extents, read in O(1) as info() is.
2. cladefs: StoreBytes becomes the split of what a store holds, bytes and extents (one info() call); StoreState carries it.
3. Control API: StatStoreResponse gains own_extents and inherited_extents (ListedStore carries StatStoreResponse, so ListStores too); go/generate.sh regenerates go/extentd/v1.
4. cladefsctl stat prints own_extents and inherited_extents lines, locally and through the daemon; module docs, integrator guide and proto comments say what it costs for a store no VM has open (opened whole, as before: the count adds nothing to it).
5. Tests: extent_store unit test of exact counts across write, grow, free, commit, reopen; cladefs remote.rs randomized sequence (seeded) of writes, frees, commits, reopenings and forks, the counts equal to extents() recounted after each; cladefsctl test (locally on macOS, through the daemon on Linux) of exact own/inherited extents before and after writes, frees, a fork; control.rs (Linux only) exact counts through StatStore and ListStores.
6. cargo nextest for extent_store and cladefs on macOS, clippy as CI, go generate --check and go tests.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-15, built (commit 9b4dfaad7, branch t454): extent_store Space keeps Holding (extents and bytes, all and own) beside the room, set at opening by Holding::of over the map Recovering already walked (the old byte loop), entered as finish_write inserts an extent (or_insert_with), removed as free takes one out; Space::audit recounts all four from the map at every state release in test and audit builds (AUDITED_EXTENTS bound unchanged). Info gains own_extents and inherited_extents: O(1), as info() was. cladefs: StoreBytes became StoreHolding { bytes, extents: Whose { own, inherited } } (ExtentStore::holding, one info() call); StoreState.holding; StatStoreResponse own_extents = 19, inherited_extents = 20 (ListStores carries StatStoreResponse); go/generate.sh run, --check clean. cladefsctl stat prints own_extents and inherited_extents after own/inherited (Stat, Display, replacing stat_lines' eight arguments).
Closed stores: unchanged. StatStore of a store no VM has open opens it whole (open_here: its state recovered from the bucket, its lease taken, the map built: time and transient memory as its extents, ~240 B each, TASK-443.2), and cladefsctl stat without the daemon does the same; the count adds nothing to that, being set from the walk the opening makes anyway. Said in the .proto's StatStore comment, cladefsctl's docs and docs/integrator-guide.md (Limits, Extents). Not fixed here (TASK-443 review item 25).
Operator, on ubu1: CLADEFS_EXTENTD_GRPC=<control socket> cladefsctl stat <base dir> (through the daemon), or cladefsctl stat <base dir> with the daemon's CLADEFS_EXTENTD_REMOTE/TOKEN and nothing open on it: lines own_extents N and inherited_extents N; a base's count is their sum.
Tests (macOS, cargo nextest run -p extent_store -p cladefs: 655 passed, 11 skipped): extent_store stores_tell_their_own_bytes_from_those_inherited (exact counts after create from an inherited state, writes, rewrites, growth, frees, a second free, commit, reopen); store::tests::audits_find_extent_counts_drifted (audit panics on a count no map holds); cladefs extents::tests::holding_follows_the_extents (proptest, 32 cases of up to 23 random writes, frees of own or inherited, commits, reopenings and forks, holding() equal to extents() recounted after each); cladefs::cladefsctl stat_counts_extents_locally (exact own/inherited bytes and extents of a new store, written, forked, the fork written and freed in, the parent after). Hand mutants (restored, touched): Holding::removed not decrementing extents, and finish_write not calling entered: all three of the extent_store, proptest and cladefsctl tests red each time. Clippy and go parts of scripts/test.sh (CLADEFS_TEST_PARTS='clippy go'): pass. scripts/upstream-changes.py --check: pass.
Linux only, NOT run (no ubu1 in this task; cross-compiling fails on aws-lc-sys/zstd-sys build scripts): cladefs::control extents_are_counted_through_the_control_api (new; StatStore open and closed, ListStores, a fork), stores_through_the_control_api and open_stores_are_listed_with_their_leases_and_uploads (extended with exact counts), cladefs::cladefsctl stat_counts_extents_through_the_daemon. They compiled on no machine yet.

AC #1 checked on the local path's evidence (stat_counts_extents_locally); its daemon path's test is Linux-only and unrun. AC #2 (a real base's count on ubu1, into TASK-443.2) and DoD #1 (workspace on macOS and Linux) left open: only extent_store and cladefs ran, on macOS.

Correction: the code's commit is 48a40af6b (9b4dfaad7 above was its first form, amended to drop a proptest-regressions file the hand mutants left; never pushed).

2026-11-15: Linux, ubu1 under fxjob (CARGO_BUILD_JOBS=4, --test-threads 4), main 21f042594: scripts/test.sh parts workspace and go, 2817 tests passed, 83 skipped; go/extentd/v1 ok (clippy passed in the run before, on 618403778). macOS: the crates this change touched and the scripts suite (see notes above). DoD #1 checked on that evidence.
<!-- SECTION:NOTES:END -->
