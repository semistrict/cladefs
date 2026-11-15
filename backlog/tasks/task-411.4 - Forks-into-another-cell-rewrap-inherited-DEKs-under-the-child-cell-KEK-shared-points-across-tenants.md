---
id: TASK-411.4
title: >-
  Forks into another cell rewrap inherited DEKs under the child cell KEK; shared
  points across tenants
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-14 18:34'
updated_date: '2026-11-15 00:33'
labels: []
dependencies:
  - TASK-411.3
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
priority: high
type: task
ordinal: 801000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A child reads its parents' objects under their DEKs. Rewrapping them under the child's own cell KEK at the fork means a child never needs another cell's KEK or another tenant's KMS, and shredding a parent leaves children what they inherited, as doc-7 already says of deletion (the user's decision; doc-16, Forks and shared points). A new cell's KEK comes from a per-host pool of KEKs the KMS wrapped ahead of time (the user's decision), so the fork's path makes no KMS call.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A fork of a point the forker cannot unwrap is refused before any record is made
- [x] #2 A fork into another cell, and a fork of a shared point into another tenant's cell, reads every inherited extent with only the child's tenant KEK
- [x] #3 Destroying the parent's tenant KEK after the fork leaves the child fully readable
- [x] #4 A fork into a new cell makes no KMS call: its cell KEK comes from the host's pool, refilled in batches off the fork's path; a test counts calls
- [x] #5 Revoking or destroying a tenant's KEK discards its pool: the next fork into a new cell of that tenant is refused
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Format: Record.inherited: Vec<keys::Inherited { branch, dek }> (layout FORMAT 10 extended, no bump: unreleased, main at 7), each ancestor of another cell's DEK generation wrapped by the record's own cell KEK, AES-KW under HKDF(cell KEK, 'fxdax inherited dek wrap' || holder place || ancestor place || generation). Given with the first DEK in one conditional update: a record with DEKs holds all it inherits from other cells.
2. keys::ring: one walk. Given record: own cell KEK alone (own + inherited + same-cell ancestors); an uncovered sealed other-cell ancestor: Error::Inherited. Ungiven record (point, child before keys, clear cell): crosses into ancestors' cells (cell object read, KEK unwrapped, cached per taking); a writer of a clear cell inheriting sealed data is refused.
3. Ring keeps each DEK's Branch; keys::inherit rewraps every DEK of branches outside the child's cell. with_dek gives own DEK + inherited (fallback for any child not given at its fork). rotate rewraps inherited; add_generation keeps them.
4. Fork path (extent_store Remote): inheriting(child) before anything is made, from the store's ring in memory and a pooled cell KEK; None for clear sources, same-cell children and points (points keep no key; their copies cross; forks of them rewrap). forked(epoch, child, inheriting): after the record, cell::make (create-only, token 0) with the KEK (Kms keeps it for the cell's first taking here) joined with the record's read, then the record's keys by conditional update.
5. Kms::pooled(Pooling{low, batch, handle}): batch refills on the host runtime after a take leaves the pool low and as a sealed store of the tenant opens; Kms::filled. Kms::revoked(tenant) on any Revoked/Destroyed answer (or the control API): pool and cached unwraps dropped, tenant refused (no refill, no fork inheriting its keys) until a KMS call succeeds. fxdax remote::kms() pooled (8/16).
6. Fork-point checkpoints (recovery::checkpoint_at) sealed under the parent's DEK: keys::Parents (writer's ring; deletion and failed-fork claims: host KMS, asked only when writing). branch::record_fork_under, delete_under, delete_fork_under; upload::delete_branch takes the KMS.
7. fxdax host_store::fork: remote.inheriting before create_dir/commit (replaces sealed_fork_into); ENOTSUP into clear cells, EACCES on KMS refusal.
8. Tests: extent_remote keys units; extent_store encrypted forks/points/pools/plaintext twins; fxdax sealed store test, sealed shared points, sealed restore fault variants.
9. Latency: fxdax-examples fork-bench.
10. doc-16/doc-7; full crate set + sim sample; clippy, fmt.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Since TASK-411.3, fxdax makes every fork, point and restore of a sealed store in a new cell, so all of them fail ENOTSUP until this task rewraps inherited DEKs.

Branch t3/rewrap-inherited-deks (from 7bd73c7d9).
Where rewrapped DEKs live: Record.inherited (format: layout FORMAT 10 extended, no bump; unreleased, main at 7). Inherited { branch, dek: Dek { generation, kek, wrapped } }, AES-KW under HKDF(child cell KEK, 'fxdax inherited dek wrap' || holder (tenant,cell,id) || ancestor (tenant,cell,id) || generation). Given with the first DEK in one conditional update, so a record with DEKs is complete for other-cell ancestors.
Fork path: Remote::inheriting(child) before commit/record (no bucket request; source = the store's own ring; KEK from pool); Remote::forked(.., inheriting) after the record: cell::make (create-only, token 0, so any placement takes it) joined with the record GET, then record PUT (conditional). Kms keeps a made cell's KEK for its first taking here (Takings entry with no taking adopted by the first). Points: no keys (giving them a DEK broke point stores: wal reseal wants a sealing key for any sealed branch); copies cross into the origin's cell; forks of points rewrap from the copy's ring.
Found and fixed: recovery::checkpoint_at (record_fork's checkpoint at the fork point for a child of another cell) was written in the clear: a sealed parent's child refused it (Downgrade). Now sealed under the parent's DEK via keys::Parents: writer's ring; deletion (branch::delete_under) and failed-fork claims (delete_fork_under) take the host KMS, asked only when a checkpoint is to be written; deletion without a KMS of a sealed parent needing one fails (filed TASK-411.14 for fxdax-gc).
KMS calls on the fork path: none (pool); child's first opening on the forking host: none; elsewhere one unwrap of the child's cell KEK. A point fork on a host: its copy unwrapped the origin's cell KEK once a taking as it opened. Empty pool: one inline wrap + batch refill.
Crashes mid-fork: before the record nothing given (Inheriting returns the KEK to the pool on drop); record made, keys not given: unmade directory deleted with the child, or any opener gives keys itself (with_dek crossing once); cell made, keys not given: keyed cell of no volume stays, the opener uses its KEK; give racing deletion: Deleted fails the fork. specs/bucket unchanged (records' keys don't change kind/origin/deleted; cell object not a kept/deleted object; checkpoint written where/when it was; a KMS-less deletion refusal = deletion stopped partway). No TLC run (no spec edit).
Constraint for TASK-411.6 (doc-16): compaction must keep an inherited object's DEK generation, or children lose it.
Fork latency (Mac, release, memory bucket, fork-bench, 2 runs): sealed fork median 34.0/32.3 ms, p90 40.9/35.9; clear with KMS 33.2/33.2; clear no KMS 33.8/35.0. Requests a fork 47.5 sealed vs 44.5 clear (+3: cell create, record GET in parallel, record PUT). With FXDAX_REMOTE_LATENCY_MS=10: sealed 225/242 ms, clear+KMS 200/214, clear no KMS 200/218 (~+25 ms: two round trips). First opening of a fork: 4-9 ms all modes (memory), ~170-224 ms at 10 ms latency.

Validation (macOS): cargo nextest -p extent_keys -p extent_frame -p extent_remote -p extent_store -p sqlite_extents -p fxdax -E 'not (package(extent_store) & test(/^sim::/))': 962/963 passed (extent_keys 22, extent_frame 33, extent_remote 294, extent_store 81, sqlite_extents 9, fxdax 236 passes); the 1 failure is fxdax storage::kept::tests::images_dropped_as_their_pass_runs_leave_their_bucket_whole, the known load flake TASK-328 (same 'Report { deleted: 0 ... }'), passing 3/3 alone. sim::one SIM_SEEDS=10 over everything, forks, takeovers, crashes, wals, forked_wals, restores, fresh_cells, collectors, failed: all pass. clippy --all-targets on extent_keys/extent_remote/extent_store/fxdax/fxdax-examples: no warnings in touched files. fmt clean. scripts/upstream-changes.py --check passes. No dependency change (vendor/firecracker.lock untouched). Linux not run (user's).
AC evidence: #1 extent_store encrypted::points_fork_into_other_tenants_cells_rewrapped_unless_their_keys_are_refused (host whose KMS refuses the point's tenant opens no copy; a host holding a copy, told the tenant is revoked, refuses the clone: Kms error, no record, no cell, pool untouched); keys::tests::pools_refill_in_batches_and_go_with_their_tenants_keks. #2 forks_into_other_cells_keep_what_they_inherit_rewrapped_and_outlive_their_parents_tenant (cross-tenant fork and a grandchild in a third cell; record inherited = parent (and child), unwraps under the child's cell KEK only), the points test's shared-point clone into tenant 2, fxdax remote::tests::sealed_points_fork_into_other_tenants_cells_while_shared, extents::tests::sealed_stores_serve_follow_reopen_and_fork_into_sealed_cells_alone, restore_faults::sealed_* (3 variants), keys::tests::rings_cross_cells_until_records_hold_what_they_inherit. #3 same tests after LocalKms::destroy of the parent's tenant: child and grandchild reopened on a fresh host read every extent, KMS calls (0, 3) (0 for the parent's tenant past its refused opening). #4 Counted: fork (0,0), child opening on the forking host (0,0), take leaving the pool low refills a batch (4,0). #5 revoked_tenants_pools_go_and_forks_into_their_new_cells_are_refused (control-API revocation drops the pool, next fork refused (1 wrap, refused), no record/cell; destroyed unheard: pooled KEKs still make cells until a refill's wraps fail, pool dropped, next fork refused).
Plaintext twin: forks_in_the_clear_ask_for_nothing_more_and_sealed_ones_go_into_no_clear_cell (inheriting None, no KMS call, takes no bucket; record keyless; sealed into a clear tenant's cell refused Inherited, nothing made).
Filed: TASK-411.14 (fxdax-gc --delete-branch of a sealed branch with an unrecorded fork into another cell needs a KMS).
<!-- SECTION:NOTES:END -->
