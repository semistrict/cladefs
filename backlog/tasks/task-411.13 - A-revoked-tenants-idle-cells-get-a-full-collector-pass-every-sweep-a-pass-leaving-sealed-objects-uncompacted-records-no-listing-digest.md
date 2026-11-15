---
id: TASK-411.13
title: >-
  A revoked tenant's idle cells get a full collector pass every sweep: a pass
  leaving sealed objects uncompacted records no listing digest
status: Done
assignee:
  - '@claude'
created_date: '2026-11-14 23:52'
updated_date: '2026-11-15 03:24'
labels: []
dependencies:
  - TASK-411.6
parent_task_id: TASK-411
priority: low
type: bug
ordinal: 811000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-411.6: a pass over a cell whose KEK the KMS refuses (revoked, destroyed, failing, or no KMS) leaves its sealed objects uncompacted and records no listing digest, so that the next pass goes over the cell whole and compacts once the key is back. While a tenant stays revoked, each sweep lists and reads every one of its idle cells again, where a digest would have let the pass skip them (TASK-395.2).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A cell left uncompacted for want of its KEK, unchanged since, is skipped by later passes until its KEK is unwrappable again or its listing changes: a test counts the requests of a second pass
- [x] #2 A tenant unrevoked has its cells compacted at the next pass
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. passed::Passed gains uncompacted: u64, the sealed objects the pass left uncompacted for want of the cell's KEK (0: none). A locked pass records its listing's digest as any pass (TASK-411.6 dropped it) and that count.
2. collector::Heard: what a sweep's passes heard of their tenants' KEKs, a shared future per tenant: a wrap of a random key through the KMS (Ok: the KEK is to be had); no KMS: not, nothing asked. Collector::heard(Arc<Heard>) shares one; Collector::collect makes one per call (a sweep), collect_cell one per pass; sweep::Sweep::over one per run, given to each pass.
3. collect_held: a recorded Passed holding for the listing skips only if uncompacted == 0 or !heard.to_be_had(tenant). skip() reports locked 1 / uncompacted N for such a cell (metrics stay true); fxdax-gc prints no pass line for a skip that only leaves the locked cell so (the sweep's line counts it).
4. Tests: collector cells_of_revoked_tenants_are_marked_and_swept_but_not_compacted rewritten to count each pass's requests (Store stats): locked pass, then skips costing get+list+update and 0 KMS calls without KMS / 1 refused wrap with one; a write -> full; unrevoked -> full, compacted; then skips asking nothing. tenants_keks_are_asked_for_once_a_sweep for Heard. fxdax-gc passed_line test.
5. Docs: doc-7 Passes that skip (The KMS), doc-16 KEKs not to be had, module docs.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Branch t3/cache-tenant-keys-gc.
How the collector learns the KEK is back: per sweep, per tenant, one wrap of a key of no use (collector::Heard, a Shared future so concurrent spawned passes of one tenant share it). Wrap rather than an unwrap of each cell's KEK: needs no cell-object GET per locked cell and is one call for all the tenant's cells; LocalKms (and real KMSes) refuse wraps for revoked/destroyed tenants as they refuse unwraps, and a failing KMS fails both. Degenerate case: a KMS that wraps for the tenant but cannot unwrap the cell's KEK (another KMS than the one that wrapped it) -> the probe says yes, the pass goes in full and is locked again each sweep: TASK-411.6's behaviour, no worse.
Safety: only compaction depends on the KEK; a skip compacts nothing and deletes only what the recorded pass deferred, which went by the cell alone; Bucket.tla's argument (a skip is the tail of the pass it goes by) is unchanged, so no spec edit.
Format: GcLease's Passed gains a u64 (bincode, fixed width; counting_itself unchanged); no migration (unreleased FORMAT 10, memory no-existing-buckets).
Report: a skip over a locked digest reports skipped 1, locked 1, uncompacted N, so fxdax_gc_sweep_locked_cells stays the number of cells left locked; fxdax-gc prints no per-pass line for it (it did nothing new), only the sweep's.
Tests (extent_remote, all 322 pass): collector::tests::cells_of_revoked_tenants_are_marked_and_swept_but_not_compacted: no-KMS pass locked (deleted 1); next full (listing changed); next skipped: report {skipped 1, locked 1, uncompacted 3}, requests exactly [Get 1, List 1, Update 1], KMS (0,0). Revoked with KMS: two skips, same requests, 1 refused wrap each (2,0). Checkpoint written: full (locked, deleted 1), full, then skip (3,2). Unrevoked: full, compacted 3, read back under the ring (4,3). Then delete pass, full pass, skip with no KMS call (4,3). collector::tests::tenants_keks_are_asked_for_once_a_sweep (tenant 1 twice, 2 revoked twice, 3 unknown: 3 wraps; no KMS: none). fxdax-gc tests::passes_say_what_they_did_if_anything gains the locked skip (None).

Validation (macOS, branch t3/cache-tenant-keys-gc at fb253eb4a): cargo nextest -p extent_keys -p extent_frame -p extent_remote -p extent_store -p fxdax -E 'not (package(extent_store) & test(/^sim::/))': 960/960 passed (extent_keys 27, extent_frame 33, extent_remote 323, extent_store 223, fxdax 354; 1 leaky: faults::tests::faults_say_their_words, untouched). sim::one SIM_SEEDS=10 over collectors, everything, forks, takeovers, crashes, wals, forked_wals, restores, fresh_cells, failed: all pass. clippy --all-targets (extent_remote, extent_store, fxdax): no warning in touched files. fmt clean on touched crates. scripts/upstream-changes.py --check passes. No dependency change. Linux not run (no ubu1 per brief): DoD #1 open.

Verified 2026-11-15 at 594ff82e9 (origin/main merged): macOS scripts/test.sh workspace 2466/2466, shuttle, scripts (mutants 0 broken), go; Linux ubu1 under fxjob (target on /mnt/fxdax-scratch), every part: clippy, workspace 2752/2752, sim 63/63, shuttle, scripts, go, guest, firecracker. Only fxdax's own crates changed (extent_keys, extent_frame, extent_remote, extent_store, sqlite_extents, fxdax, fxdax-examples): no upstream deviation; scripts/upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
A pass leaving sealed objects uncompacted for want of its cell's KEK now records its listing's digest and how many it left (Passed::uncompacted). A pass after it whose listing matches skips the cell while the tenant's KEK is still not to be had, asked once per tenant per sweep by a wrap of a throwaway key (collector::Heard, shared by a sweep's or a collect()'s passes; none without a KMS), and goes over it in full, compacting, once the KMS answers; a changed listing passes in full as before. Skips report locked/uncompacted so fxdax-gc's sweep line and metrics still count locked cells; no per-pass line for them. doc-7 (Passes that skip) and doc-16 updated. Verified by collector::tests::cells_of_revoked_tenants_are_marked_and_swept_but_not_compacted (a skip costs exactly GET+LIST+UPDATE and 0 KMS calls without KMS / 1 refused wrap with one; a write makes it pass in full; unrevoked, the next pass compacts) and tenants_keks_are_asked_for_once_a_sweep; 960 macOS tests and a 10-seed sim sample over 10 profiles pass; Linux not run.
<!-- SECTION:FINAL_SUMMARY:END -->
