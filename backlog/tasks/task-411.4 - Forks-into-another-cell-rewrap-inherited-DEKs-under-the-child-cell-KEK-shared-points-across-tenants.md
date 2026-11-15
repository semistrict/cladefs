---
id: TASK-411.4
title: >-
  Forks into another cell rewrap inherited DEKs under the child cell KEK; shared
  points across tenants
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-14 18:34'
updated_date: '2026-11-14 23:35'
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
- [ ] #1 A fork of a point the forker cannot unwrap is refused before any record is made
- [ ] #2 A fork into another cell, and a fork of a shared point into another tenant's cell, reads every inherited extent with only the child's tenant KEK
- [ ] #3 Destroying the parent's tenant KEK after the fork leaves the child fully readable
- [ ] #4 A fork into a new cell makes no KMS call: its cell KEK comes from the host's pool, refilled in batches off the fork's path; a test counts calls
- [ ] #5 Revoking or destroying a tenant's KEK discards its pool: the next fork into a new cell of that tenant is refused
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Format: Record gains inherited: Vec<keys::Inherited { branch, dek }> (layout FORMAT 10 extended, no bump: unreleased, main is at 7): each ancestor branch of another cell whose objects the branch reads, each generation, its DEK wrapped by the record's own cell KEK (generation named) with AES-KW under HKDF(cell KEK, 'fxdax inherited dek wrap' || holder place || ancestor place || generation). Given with the branch's own first DEK, in one conditional update: a record with DEKs holds all it inherits from other cells (given), complete.
2. keys::ring: one walk of the origins. A given record: own DEKs + inherited + same-cell ancestors' records; a sealed ancestor of another cell it holds not: Error::Inherited (no other cell's KEK ever needed). An ungiven record (a point, a fork's child before its keys, a clear cell): crosses into an ancestor's cell, reading its object and unwrapping its KEK (cached per taking), adding its DEKs and inherited; a writer's ring of a clear cell crossing a sealed ancestor is refused (sealed data is never inherited into the clear).
3. Ring keeps each DEK's branch (places); keys::inherit(source ring, child, child KEK) rewraps every DEK of branches outside the child's cell. with_dek gives own DEK + inherited (source: the ungiven ring) - the fallback for any child not given at its fork. rotate rewraps inherited too; add_generation keeps them.
4. Fork path (extent_store Remote): inheriting(child) before anything is made (before commit_fork / fork_point): source = the store's own ring in memory; nothing for a clear source or a same-cell child (plaintext forks unchanged: no request more); refused if the KMS refused a tenant of the source (Kms::revoked marks), or the child's tenant has no KEK (Inherited), or its KEK is refused; takes a cell KEK from the pool (given back on drop if unused). forked(epoch, child, inheriting): once the child's record exists, makes the child's cell with that KEK (create-only, token 0, so any placement takes it), the host's Kms keeping the KEK for its first taking (no unwrap at the child's opening), then gives the child's record its DEK and inherited by a conditional update. A failure fails the fork, which fxdax unforks.
5. Kms pool: Kms::pooled(kms, Pooling{low, batch, handle}) refills a tenant's pool in batches on the host runtime off the fork's path, after a take leaves it low and as a sealed store of the tenant opens; Kms::filled awaits a refill. Kms::revoked(tenant) (any Revoked/Destroyed answer, or the control API) drops the pool and the tenant's cached unwraps and marks it refused until a KMS call for it succeeds; new_kek then asks the KMS, refused. fxdax remote::kms() uses the pool.
6. fxdax host_store::fork: remote.inheriting before create_dir/commit (replaces sealed_fork_into); errno: Inherited ENOTSUP, Kms EACCES.
7. Crashes mid-fork: before the record nothing changes; record but no keys: deleting the unmade directory deletes the child, and any opener gives keys itself (with_dek, crossing once); cell made, keys not given: a keyed cell of no volume stays, as cells do. specs/bucket: records' keys and cells change nothing it models (state what and why).
8. Tests: extent_remote keys (inherited wrap bound to place; ring modes; given ring refuses uncovered; rotation rewraps inherited). extent_store encrypted: fork into another cell (same tenant, and cross-tenant shared point) reads every inherited extent; parent tenant destroyed (LocalKms::destroy), child reopened on a new host reads all; grandchild across three cells; point fork refused before any record when the host cannot unwrap (opening the point) and when the origin tenant is marked revoked; zero KMS calls on the fork with a filled pool (Counted) and refill afterwards; revoked/destroyed tenant pool dropped, next fork into a new cell refused before any record; restore/shared-point encrypted variants in fxdax where they exist; plaintext twins unchanged.
9. Fork latency: an example (fxdax-examples fork-bench) timing forks in the clear and sealed over an in-memory bucket, with and without a per-request delay; numbers in notes.
10. doc-16/doc-7 updated; nextest narrow then the full crate set + sim sample; clippy, fmt.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Since TASK-411.3, fxdax makes every fork, point and restore of a sealed store in a new cell, so all of them fail ENOTSUP until this task rewraps inherited DEKs.
<!-- SECTION:NOTES:END -->
