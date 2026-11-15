---
id: TASK-411.29
title: >-
  A bucket-KMS KEK object deleted outright leaves hosts making cells under the
  deleted KEK: pooled cell KEKs and the KEK held for FRESH_FOR; their writes
  acknowledged, lost at the next taking
status: To Do
assignee: []
created_date: '2026-11-15 19:30'
labels:
  - bug
  - encryption
dependencies: []
parent_task_id: TASK-411
ordinal: 872000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the review of TASK-411.28 (2026-11-15). With the sealed marker gone, BucketKms::read(make=true) finding no KEK object makes a new tenant KEK quietly. keys::Kms::new_kek hands out cell KEKs pooled earlier, wrapped under the old tenant KEK, with no KMS call; the pool is dropped only by Kms::revoked, which nothing now triggers: a refill's wrap succeeds (making the new KEK) and calls answered(), so up to a pool's worth (about 24) of stale KEKs per tenant per host stay. BucketKms::kek also wraps under the old KEK it holds for FRESH_FOR (60 s). A cell made from one works while its host holds the unwrapped secret; at its next taking (restart, another host) unwrap finds another KEK ID, reads again, and fails Unauthentic ('wrapped by another KEK of the tenant's'), which revokes() does not count: its acknowledged writes are lost. The old sealed marker turned the refill into Destroyed, dropping the pool. Only a bucket writer bypassing destroy (ShredTenant) deletes a KEK object outright; such a writer could delete data anyway, so this is a consistency defect more than an attack. kek_objects_deleted_outright_lose_what_they_wrapped uses a fresh host, so misses it. Options: (a) BucketKms::wrap remembers the KEK ID it last wrapped under per tenant and answers Destroyed once when it changes, so Kms::revoked drops the pool (closes the refill path, not pool takes before a refill); (b) pool entries remember the tenant KEK identity they were wrapped under and a take checks it against the KMS's current one when that is cheap (BucketKms holds it in memory); (c) treat a KEK object missing on a host that knew one as shredded on that host. Reproducer: Kms::pooled over BucketKms<SimBucket>, fill tenant 7's pool, delete layout::kek(7), new_cell_kek, then unwrap that KEK through a new BucketKms and Kms: Unauthentic.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A cell made after a tenant's KEK object was deleted outright is made under the tenant's new KEK on every host, or refused; never under the deleted one
- [ ] #2 The reproducer above is a test, failing before the fix
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
