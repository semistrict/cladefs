---
id: TASK-606
title: A passing KMS refusal keeps refusing forks of a tenant's kept version
status: To Do
assignee: []
created_date: '2026-11-18 15:32'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 997000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), shred-everywhere-kms review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/bucket_cell/src/keys.rs`: `rg -n -F 'if let Some(refused) = kms.refuses(tenant) {' crates/bucket_cell/src/keys.rs`

`CachingKms.refused[tenant]` is set by any Revoked, Destroyed or NoSuchTenant answer (`heard`, `fill`, `kek`) or by ForgetTenantKeys. Only `answered()` clears it, which needs a later successful KMS call for that tenant. `keys::inheriting` checks `refuses()` for every tenant whose DEKs the source ring holds and returns the stored refusal without any KMS call. `new_cell_kek`, which would ask the KMS and clear the refusal, comes after that check and is never reached. A store at a version that was opened before the refusal (a kept store) already holds its ring and unwraps nothing, so nothing on the fork path ever calls the KMS for that tenant. The daemon doesn't recover either. `fork_kept`/`fork_held` treat EACCES as not the store's fault (`store_at_fault` = `!matches!(errno, Errno::EEXIST | Errno::EACCES)`, cells.rs:893), so the kept store is not let go and reopened (reopening would unwrap through the KMS and clear the refusal). And `kept()` sets `forked = now` on every restore, so the 60 s LINGER never expires while restores keep coming. So after a temporary refusal, every restore or fork of that tenant's snapshot on the host fails with EACCES with no end. The same holds for forks of its shared version into other tenants' cells. Triggers include a key disabled then enabled again, an IAM or policy edit, AWS AccessDenied, GCP PERMISSION_DENIED, its changes that take up to ~2 min to show, or NotFound for an alias just made. It ends only after 60 s with no fork, or when an unrelated fresh open of one of the tenant's cells on that host makes a KMS call.

**Trigger.** 1. Tenant A's zygote snapshot version V is kept open on host H (`fork_version` -> `opened_version` -> `kept_or_open` -> `keep`). 2. A disables its KMS key for a minute (AWS DisabledException, GCP DISABLED, or OpenBao soft-delete or a policy edit). 3. A restore on H forks V. `new_cell_kek(A)` pops a pooled KEK and its refill's wraps hear Revoked, so `CachingKms::revoked(A)` puts A in `refused`. 4. A enables the key again. 5. Every later restore or ForkCell of V on H goes `OpenCell::fork` -> `host_cell::fork` -> `Uploader::inheriting` -> `keys::inheriting` -> `refuses(A)` = Some -> Error::Kms -> EACCES, with no KMS call. `store_at_fault(EACCES)` is false, so the kept store stays and `kept()` refreshes its timestamp. Restores fail for as long as they arrive within 60 s of each other.

**Why the tests stay green.** local_cell upload/encrypted.rs `versions_fork_into_other_tenants_cells_rewrapped_unless_their_keys_are_refused` revokes the version's tenant on host 7. It then forks from a fresh copy on host 8 and never lifts the revocation and forks again from the same open store. `revoked_tenants_pools_go_and_forks_into_their_new_cells_are_refused` revokes only the child's tenant, whose `new_cell_kek` asks the KMS and so clears the refusal. No daemon test covers a kept store whose fork the KMS refused.

**Backlog.** Related: TASK-411.4 (Done) designed the refusal to last until a KMS call succeeds again, yet left no KMS call on the fork path. TASK-395.7 (Done) made EACCES not the kept store's fault. TASK-411.13 (Done) solved the same case for the collector with a throwaway wrap.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A tenant's KMS key can be refused (Revoked, Destroyed or NoSuchTenant) and then served again. Forks of that tenant's version from a store the host keeps open then succeed once the KMS answers again: the kept store is not reopened and forks need no 60 s pause. Forks of its shared version into other tenants' cells succeed too.
- [ ] #2 Regression tests go red with the fix reverted. One in local_cell upload/encrypted.rs opens a copy of tenant 1's version on a pooled host and calls kms.revoked(TENANT) while the KMS still serves it, then asserts a fork from the same open copy succeeds. A daemon test asserts that VM snapshot restores of a kept version recover once the KMS answers again.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
