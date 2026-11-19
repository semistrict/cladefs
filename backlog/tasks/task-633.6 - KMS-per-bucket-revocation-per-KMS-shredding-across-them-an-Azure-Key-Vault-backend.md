---
id: TASK-633.6
title: >-
  KMS per bucket, revocation per KMS, shredding across them; an Azure Key Vault
  backend
status: To Do
assignee: []
created_date: '2026-11-18 17:03'
updated_date: '2026-11-19 01:13'
labels:
  - bucket
  - multi-region
dependencies:
  - TASK-633.4
documentation:
  - >-
    backlog/docs/doc-23 -
    Cells-across-buckets-a-bucket-per-cloud-region-forks-that-span-them.md
parent_task_id: TASK-633
priority: medium
type: feature
ordinal: 937000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
One KMS serves a process (`static POOLED`), refusals revoke a tenant everywhere, the bucket KMS keeps tenant KEKs in the host's bucket (an unwrap from another reads as Destroyed and would revoke the tenant falsely), aws-kms:// is bound to one cloud region, and no Azure backend exists, though the user chose buckets on all three clouds (doc-23, KMS routing).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each bucket's registry entry names its KMS; a cell's keys are wrapped in its home bucket's; the KMS cache is keyed by tenant and KMS
- [ ] #2 A KMS refusing revokes the tenant for that KMS's buckets alone; an unwrap across buckets under the bucket KMS is refused, not Destroyed
- [ ] #3 Shredding a tenant destroys its KEK in every KMS that holds one; doc-16 says so
- [ ] #4 An azure-kv:// backend (Key Vault or Managed HSM) with doc-16's tests for backends, license checked
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Mapped 2026-11-18 (read-only survey): one KMS per process (cladefs::bucket host_keys/POOLED), the bucket KMS keeps tenants/<t>/kek in the host bucket (no cell, so the router keeps it home), CachingKms refusals/pools keyed by tenant alone, registry kms/master_key_id unread.
1. bucket_cell::bucket_kms::Kmses replaces HostKms: per bucket number (objectid::bucket_of(cell.id)), from the registry entry: details.kms -> kms::from_url (shared by URI); none + master keys -> BucketKms over that bucket's member (unrouted, so its KEK objects stay in it), checking master_key_id; NAMED and unregistered -> today's from_env; set_kms overrides all (tests).
2. CachingKms holds Kmses; refused, pools, eras keyed by (KmsKey, tenant); new_kek/give_back/refill/fill/refuses take a CellId; revoked(kms, tenant) touches that KMS alone; inner() goes (rotate, collector take the resolver).
3. kms::Error::Elsewhere (not a revocation) for an unresolvable bucket and a bucket KMS wrap of another bucket (fxkb header gains the bucket: format bump).
4. shred_tenant walks the registry: destroys each bucket KMS's KEK object; named KMSes reported for out-of-band destruction; forget(Tenant) clears every KMS.
5. extent_keys/src/kms/azure.rs (azure-kv://), as gcp.rs: KeyNames, real::wrapped, object_store's Azure credentials, tenant bound into the wrapped bytes; license check of any new crate.
Tests: two buckets with different bucket-KMS master keys; a cell of each wrapped in its own bucket; a KMS refusing revokes that bucket's tenants alone; an unwrap across buckets is Elsewhere; shredding reaches both.
<!-- SECTION:PLAN:END -->
