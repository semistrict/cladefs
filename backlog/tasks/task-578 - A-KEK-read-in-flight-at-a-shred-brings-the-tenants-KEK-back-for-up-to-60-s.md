---
id: TASK-578
title: A KEK read in flight at a shred brings the tenant's KEK back for up to 60 s
status: To Do
assignee: []
created_date: '2026-11-18 15:28'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 969000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), crypto review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/bucket_cell/src/bucket_kms.rs`: `rg -n -F 'Ok(kek) => held.insert(tenant, Held { kek: kek.clone(), read }),' crates/bucket_cell/src/bucket_kms.rs`

BucketKms::kek, finding the held KEK stale, drops the lock, awaits self.read() (a GET of tenants/<t>/kek), then inserts whatever it read with its start Instant. It has no generation or epoch check against `forget`. ShredTenant -> BucketKms::destroy writes the tombstone and calls forget, and CachingKms::revoked calls forget_dyn again. A read whose GET returned the KEK object before the tombstone landed completes afterwards and re-inserts the destroyed tenant's KEK, fresh for up to 60 s. Meanwhile, on the very host that shredded: (1) a halted store's VM reopening it unwraps its cell KEK (CachingKms::kek has no `refused` check; revoked only cleared takings), so the store serves the shredded tenant's data again. (2) Every successful call runs CachingKms::answered(tenant), which clears `refused`. That re-enables pool refills (new cells wrapped under the destroyed KEK) and lets keys::inheriting allow forks whose source rings hold the tenant's DEKs, such as a shared version into another tenant's cell, whose child then keeps the shredded tenant's DEKs for good. doc-16 says the bucket KMS's forget 'drops it at once' and that the shredder refuses at once.

**Trigger.** Host H on the bucket KMS. Tenant T's held KEK is older than FRESH_FOR, and a pool refill (fill's 16 concurrent wraps, after a fork took from the pool) or a store opening calls wrap/unwrap for T, each issuing GET tenants/T/kek. ShredTenant(T) runs on H while a GET is in flight: destroy's update lands, forget runs, bucket::forget halts T's stores and CachingKms::revoked sets refused. The GET returns the pre-tombstone object -> kek() inserts Held{kek, read=t0}. For the rest of the 60 s, H's unwraps and wraps for T succeed, answered(T) clears refused, and a reopened store of T serves data.

**Why the tests stay green.** bucket_kms::tests::shredded_tenants_are_destroyed_and_never_made_a_kek_again runs every call sequentially on block_on with no read straddling forget. SimBucket's served_in_rounds is used only for the make race, never for destroy-vs-read.

**Backlog.** Related: TASK-411.29 (To Do) is a KEK object deleted outright, not a read racing forget. TASK-411.27 and TASK-411.7 (Done) built the bucket KMS and the forget wiring without naming this race.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 On a host using the bucket KMS, a GET of a tenant's KEK that is in flight when ShredTenant runs does not bring the KEK back. Every later wrap and unwrap for that tenant on that host fails with Destroyed: a reopened store, a pool refill and a new cell KEK are all refused.
- [ ] #2 After a tenant is refused, a late successful answer from before the refusal does not clear it. Pool refills and forks that inherit the tenant's DEKs stay refused.
- [ ] #3 A test in the bucket_kms tests uses SimBucket::served_in_rounds to start unwrap(7, w) on a host whose KEK is stale and to run destroy(7) and forget(7) in the same round. It asserts a later unwrap on that host is Destroyed, and it goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
