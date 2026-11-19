---
id: TASK-633.6
title: >-
  KMS per bucket, revocation per KMS, shredding across them; an Azure Key Vault
  backend
status: In Progress
assignee: []
created_date: '2026-11-18 17:03'
updated_date: '2026-11-19 02:49'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-18 night (unattended):
- 82bb2aa37 CachingKms over Kmses (per bucket number), refusals/pools keyed by (KMS, tenant); OneKms = single KMS. On main.
- 9bbf7f7cd ByBucket: home bucket and NAMED keep the env choice; another bucket: registry kms, else CLADEFSD_BUCKET_<N>_CLADEFS_KMS/CLADEFS_KMS, else bucket KMS over that bucket with its own master keys, master_key_id checked. KMSes keyed by URL (shared refusals). DECISION: the home bucket's registry entry is not consulted (its own env is its config; avoids behaviour change for one-bucket hosts).
- b8364158c ByBucket::shred: tombstones in every bucket-KMS bucket of the registry (copy + root); named-KMS and unreachable buckets named in an error after shredding where possible.
- 6bd3a5581 azure-kv:// backend (no new crates; reqwest/serde_json/base64/url already deps, all MIT/Apache). Tested against an in-process fake vault + authority only: refusal codes (403 revoked, 404 KeyNotFound none, 400 unwrap unauthentic) follow Key Vault's error codes, NOT checked against the live service (no Azure credentials; GCP-only exception for live KMS tests).
- e82da0965 fxkb format 2 binds the bucket identity; cross-bucket unwrap -> kms::Error::Elsewhere (not a revocation). doc-16 updated (format, shredding across buckets).
Test: bucket_kms::tests::buckets_cells_are_under_their_own_buckets_kms (3 buckets: own bucket KMS, bucket KMS w/ own master key, local KMS by registry), azure tests (7), hand mutants for each step.

Review (verifying-reviewer, 2026-11-18 night) found 4 defects in the unlanded commits; fixed on branch t3/mr-kms with tests and mutants:
1. SECURITY azure.rs: a tenant override (keys=FILE) or registry-recorded URL could name any https host, which would get the vault bearer token. Now every host must be a Key Vault/Managed HSM domain (public, China, US Gov); endpoint= only https://<vault host> or http loopback (emulators). Tests overrides_naming_no_vault_are_refused, urls_naming_no_vault_or_no_keys_are_refused.
2. kms_vars: a bucket's own CLADEFS_KMS/MASTER_KEY/MASTER_KEY_FILE override the host's as one group (was: host _FILE + bucket key = "both set" refusal; host CLADEFS_KMS beat a bucket's own master key). Test a_buckets_own_kms_variables_override_the_hosts_as_one.
3. CachingKms: a control-API revocation now holds for every KMS until each answers (Refused{by, told}); it used to be dropped for KMSes not yet asked, letting inheriting() pass for a parent in an unasked bucket. Test revocations_told_hold_for_every_kms_until_each_answers.
4. ByBucket::of keeps the bucket KMS that won the chosen race in buckets (shred forgot a duplicate instance).
<!-- SECTION:NOTES:END -->
