---
id: TASK-411.19
title: >-
  A tenant whose real-KMS key finished deletion reads as having no KEK: its new
  cells would be made in the clear
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 03:58'
updated_date: '2026-11-15 09:50'
labels: []
dependencies: []
parent_task_id: TASK-411
priority: high
type: bug
ordinal: 817000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found in TASK-411.17 (AWS KMS backend). A real KMS forgets a deleted key: AWS KMS answers NotFoundException for a key whose deletion waiting period is over, exactly as for a key never made (an alias is deleted with its key), and other KMSes may do the same (OpenBao Transit's deleted key is a 404). The backends map that to kms::Error::NoSuchTenant as TASK-411.17's AC says, and doc-16 makes NoSuchTenant mean 'make the cell in the clear' (extent_remote keys::Kms::new_kek, KEKLESS_FOR). So a shredded tenant, once AWS has deleted its key, gets its next new cells written unencrypted on any host that still serves it, where LocalKms keeps a 'destroyed' mark for exactly this. Unwraps are not affected (they fail, as they should). While the key is pending deletion it reads Destroyed, which is safe; today only the operator's procedure (forget-tenant-keys, stop serving the tenant) covers the rest.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A tenant that had a KEK (it has sealed cells, or the host or bucket recorded one) is never given a cell in the clear after its KMS answers that its key is not found: a test with a KMS answering NotFound for a tenant with sealed cells shows the cell's making refused, naming the tenant
- [x] #2 doc-16 says how a tenant's KEK forgotten by the KMS is told from a tenant that never had one
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. layout: tenants/<tenant>/sealed (layout::sealed, Kind::Sealed { tenant }, no cell's, owner none); the collector's fate Stays; FORMAT 10 extended in place.
2. keys: new_cell_kek(bucket, kms, tenant) is where every cell maker gets a KEK (store opening in extent_store::upload, keys::inheriting for forks into new cells, FUSE control attach). A KEK handed out: the marker is created first (create-only, an existing one fine), cached per host for good (Kms), so it lands before the cell object holding the wrapped KEK is created: no sealed cell without its marker. None (the KMS's NoSuchTenant, heard now or believed for KEKLESS_FOR): read the marker (head), present cached for good, absent cached for KEKLESS_FOR; present: Kms::revoked(tenant) (pool, takings dropped, forks refused) and refuse as Destroyed naming the tenant and the marker. NoSuchTenant heard by new_kek or a pool refill also drops the tenant's pool (era bumped).
3. extent_keys::testing::Forgetting<K>: a KMS answering NoSuchTenant where its inner one answers Destroyed (AWS after deletion, OpenBao deleted key).
4. Tests: keys unit tests (SimBucket::served request counts, paused clock: plaintext tenant one wrap + one head per KEKLESS_FOR; marked tenant refused, pool dropped, no clear cell); extent_store encrypted: store opening, fork into a new cell refused naming the tenant after Forgetting; never-sealed tenant clear as before; collector sweep keeps the marker; fxdax FUSE attach if testable.
5. Docs: doc-16 (marker section, replace operator workarounds in AWS/OpenBao sections), doc-7 layout row.
6. fmt, clippy, nextest on touched crates + sim sample.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
TASK-411.16 found the same with OpenBao: a deleted Transit key is forgotten, so wrap says NoSuchTenant and new cells of a shredded tenant would be made in the clear (OpenBao has no scheduled-deletion state). GCP Cloud KMS likely similar once a key's versions are all destroyed. The fix belongs above the backends, in how a cell chooses encryption (doc-16).

The user's decision (2026-11-15): a tenant marker in the bucket. The first sealed cell of a tenant writes tenants/<tenant>/sealed (create-only); a tenant with the marker never gets a cell in the clear: the KMS's 'no such key' then refuses, as destroyed. Works for every backend, no host config.

Implemented (2026-11-15):
- Marker `tenants/<tenant>/sealed` (layout::sealed, Kind::Sealed, owner none; bytes: layout FORMAT; FORMAT 10 extended in place). Collector fate Stays; the leak checks of extent_remote testing, fxdax fork_faults and restore_faults keep it. No pass lists it (it is outside every cell prefix; the delimiter listings of tenants/ and tenants/<t>/cells/ do not show it).
- Written in keys::new_cell_kek (now takes the bucket), which every cell maker calls: extent_store upload (store opening), keys::inheriting (fork into a new cell; bucket threaded through Remote::inheriting), fxdax fuse/control attach. A KEK handed out => bucket::put_if_absent (one PUT, AlreadyExists = done) before returning, so before the cell object holding the wrapped KEK is created; cached per host (Kms.markers, There forever).
- Read in new_cell_kek when Kms::new_kek returns None (NoSuchTenant now or kekless-cached): bucket::exists (HEAD, retried). There => Kms::forgotten: Kms::revoked(tenant, why) and Error::Kms("tenant T: its KEK is destroyed: the KMS has none, and tenants/<T>/sealed says it had one") => EACCES in fxdax. Absent cached for KEKLESS_FOR.
- Kms::has_no_kek now drops the tenant pool (era bump), so a refill hearing NoSuchTenant leaves no pooled KEKs of a forgotten key.
- extent_keys::testing::Forgetting<K>: maps Destroyed to NoSuchTenant (wrap and unwrap).
- Tests: extent_remote keys::tests tenants_whose_kek_the_kms_forgot_are_refused_by_their_sealed_marker, pools_of_tenants_whose_kek_the_kms_forgot_go, tenants_never_sealed_have_their_marker_read_once_a_while (SimBucket::served request lists, paused clock); extent_store upload::encrypted::tenants_whose_kek_the_kms_forgot_get_no_cell_in_the_clear (opening new cell / new volume refused, fork refused, never-sealed tenant clear, sweep keeps marker); fxdax remote::tests::tenants_whose_kek_the_kms_forgot_open_no_store_in_the_clear (EACCES). Hand mutant (marker check off) failed the first, the second and the extent_store test. FUSE control attach: same new_cell_kek call, not testable on Mac.
- Filed TASK-411.24: a host with no KMS reads no marker and still makes clear cells for a sealed tenant.
- Docs: doc-16 new section "A forgotten KEK told from none", AWS/OpenBao workaround text replaced; doc-7 layout row.
- Validation (Mac): nextest -p extent_keys -p extent_frame -p extent_remote -p extent_store -p sqlite_extents -p fxdax (sim excluded): 1033 passed, 0 failed. clippy --all-targets on touched crates: no new warnings. scripts/upstream-changes.py --check: passes.

Simulation sample: sim::one, SIM_SEEDS=10 from SIM_SEED=4119000, for all 23 profiles (collectors, fresh_cells, everything, forks, bases, stale, collected, takeovers, recreated, crashes, taken, children, recovering, failed, restores, forgotten, whole_restores, taking_ids, usurped, deleted_forking, wals, forked_wals, late_checkpoints): all passed. DoD #1 (macOS and Linux ubu1) not checked: Linux not run (Mac only for this task). DoD #3: no upstream code changed beyond fxdax-marked additions; left for review.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
A tenant's sealed marker, tenants/<tenant>/sealed, tells a KEK the KMS forgot (AWS KMS after deletion, a deleted OpenBao key: both NoSuchTenant) from a tenant that never had one. keys::new_cell_kek, which every cell maker calls (store opening, fork into a new cell, FUSE control attach), creates the marker create-only before handing out a KEK, so before any sealed cell object exists; when the KMS says no such key, a present marker refuses the tenant as destroyed (Error::Kms naming tenant and marker, EACCES in fxdax), drops its pool and takings, and refuses forks. Cached per host: present for good, absent for KEKLESS_FOR, so a plaintext tenant costs at most one wrap and one HEAD per host per minute. A refill hearing NoSuchTenant drops the pool. The collector keeps the marker (Kind::Sealed). Verified on macOS: new tests in extent_remote keys (request lists counted), extent_store encrypted (opening, fork, clear twin, sweep keeps marker), fxdax remote (EACCES); a hand mutant without the marker check failed them; 1033 tests of the touched crates passed; 10 seeds of each of the 23 sim profiles passed; clippy clean; upstream-changes check passes. Docs: doc-16 section "A forgotten KEK told from none", AWS/OpenBao workaround text replaced, doc-7 layout row. Filed TASK-411.24 (hosts with no KMS read no marker).
<!-- SECTION:FINAL_SUMMARY:END -->
