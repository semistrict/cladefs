---
id: TASK-411.7
title: 'Shredding a tenant or a cell through the control API, and what it leaves'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-14 18:34'
updated_date: '2026-11-15 01:24'
labels: []
dependencies:
  - TASK-411.3
  - TASK-411.4
  - TASK-411.5
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
type: task
ordinal: 804000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Crypto-shredding is one of the three goals (doc-16). A tenant shreds by its KEK in the KMS; a cell by removing its wrapped KEK, which is only as strong as the bucket keeps no older versions of the cell object.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Shredding a tenant leaves none of its volumes readable on any host, including open segments and cached parts: an end-to-end test
- [ ] #2 Shredding a cell leaves its volumes unreadable and other cells of the tenant readable
- [ ] #3 doc-16 and the operator docs say which bucket lifecycle rules a cell shred needs
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. extent_remote::cell: Object.shredded (layout FORMAT 10 extended, unreleased); cell::shred(bucket, cell) removes every wrapped KEK and marks the object shredded by one conditional update (any host, not conditional on holding it: whoever holds it keeps changing its volumes, the mark kept); take/hold refuse a shredded cell (Error::Kms naming tenant and cell: EACCES, and the collector's 'locked' path), add_kek refuses it.
2. extent_remote::keys (additive): CellKeys knows a shredded cell (sealed, its KEKs refused naming it, so no clear writer and no Corrupt); keys::Scope {Tenant, Cell}; Kms::forget(scope) (tenant: Kms::revoked; cell: its unwrapped KEKs dropped and refused on this host even under a stale object); Ring::owner (the branch whose store opened it); HostRing::forget(scope) drops the rings of the scope's stores from the host's fetchers' union (inherited DEKs held by other tenants' stores stay: shredding a parent leaves its children what they inherited); keys::Host::forget.
3. fxdax: fences carry an errno and register by branch; remote::forget(scope, why) = KMS forget + HostRing forget + every open store of the scope fenced (EACCES) + the extent cache's extents held only by the scope's stores taken out (pages evicted, unmapped from windows, holders dropped so no page-in reads them again); extents other stores hold (shared points' clones, public bases) stay, documented. remote::shred_cell(bucket, cell) = cell::shred + forget.
4. Control API (proto + Go regenerated): ShredCell {tenant, cell} (bucket update + forget, idempotent, to be called on every host) and ForgetTenantKeys {tenant} (after the operator destroys or revokes the tenant's KEK in the KMS); control::Client methods; fxdaxctl shred-cell / forget-tenant-keys (through the daemon).
5. Tests: extent_remote cell/keys units; extent_store upload::encrypted e2e: tenant shred over two hosts (open segment, WAL, part cache on disk: no plaintext, fresh opens fail naming the tenant, fetchers refuse cached parts, other tenant reads), cell shred (its volumes refused naming the cell, the tenant's other cell reads, the collector leaves it locked not corrupt); fxdax: stores of a forgotten tenant/cell fenced EACCES, extent cache emptied of them, another tenant's untouched, reopen refused; Linux-only control API test (tests/control.rs).
6. Parent AC #3 encrypted variants: sqlite_extents tests/image.rs (macOS), fxdax tests/fuse.rs FUSE server (macOS); Linux-only: sqlite_extents tests/daemon.rs, extentd VM store through its socket, fuse_mount.rs.
7. Docs: doc-16 Shredding (what was built, what stays and why), operator docs: cell shred needs non-current versions of <C>cell expired (S3 NoncurrentVersionExpiration, GCS versioning off or lifecycle delete of non-current) or versioning off; tenant shred absolute.
8. Verify: narrow nextest runs, then the six-crate run; clippy, fmt; Linux cross-check (cargo check --target x86_64-unknown-linux-gnu).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Built (branch t3/tenant-cell-shredding from 979ecd28f):
- extent_remote::cell: Object.shredded (FORMAT 10 extended, unreleased); cell::shred (conditional update by anyone, takes every KEK generation out; idempotent: 0 the second time); take/hold/add_kek refuse a shredded cell with cell::shredded = Error::Kms('tenant T: cell T/C is shredded, its KEK gone'), so EACCES and the collector's locked path (not a corrupt pass).
- keys.rs (additive): CellKeys.shredded (sealed, KEKs refused naming the cell: no clear writer, no DEK given); Scope {Tenant, Cell}; Kms::forget (tenant -> Kms::revoked, now wired; cell -> unwrapped KEKs dropped and refused on this host even under a stale object); Ring.owner set by ring(); HostRing::forget(scope) by owner; keys::Host::forget.
- fetch::Memory::forget_kept(scope): drops kept tails (opened under keys) and recent parts (unpacked plaintext) of the scope's objects. Found necessary: without it a host told still served a cached part (the cell e2e test failed so).
- fxdax: fences carry an errno and are listed by branch (fence::fence_stores); cache::forget drops the scope's stores from cached extents' holders, evicts and removes extents nobody else open holds (no page-in reads them again); remote::forget(scope, why) -> Forgotten {stores, rings, kept, extents, bytes}; remote::shred_cell(bucket, cell). extentd refuses a fenced store's requests with its fence's errno (EIO as before for panics, EACCES for shredding).
- Control API: ShredCell {tenant, cell} -> {keks_removed, forgotten}, ForgetTenantKeys {tenant, why} -> {forgotten}; control::Client::shred_cell/forget_tenant_keys; fxdaxctl shred-cell TENANT/CELL, forget-tenant-keys TENANT (daemon only, hex). Go regenerated (go/generate.sh --check passes).
- Hosts told: open stores of the scope fenced (EACCES); their keys stay in the fenced store's memory until its holder closes it (VM process, kept point). Untold hosts keep reading open stores but open none afresh.
- Docs: doc-16 Shredding + 'What TASK-411.7 made'; doc-7 bullet; README 'Encryption and shredding' (lifecycle rules: S3 NoncurrentVersionExpiration, GCS versioning off or non-current delete; tenant shred absolute).
<!-- SECTION:NOTES:END -->
