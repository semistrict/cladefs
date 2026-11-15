---
id: TASK-411.27
title: >-
  The bucket KMS: tenants' KEKs kept in the bucket, wrapped by a master key from
  FXDAX_MASTER_KEY or FXDAX_MASTER_KEY_FILE, made on a tenant's first use; the
  default KMS
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 11:09'
updated_date: '2026-11-15 11:51'
labels: []
dependencies: []
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
priority: high
type: feature
ordinal: 823000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The user's decisions (2026-11-15): when no FXDAX_KMS is configured, the default is not plaintext but a simple KMS keeping every tenant's KEK in object storage itself, encrypted with one master key given in an environment variable or a file. A tenant's KEK is made on its first use (no setup per tenant). Several master keys may be given for rotation: the newest wraps, any unwraps. A host with neither FXDAX_KMS nor a master key refuses to start (TASK-411.28 switches the defaults and removes plaintext). Shredding a tenant is deleting its KEK object (the bucket-versioning caveat of a cell shred, doc-16).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A KMS over a bucket implements extent_keys::kms::Kms: a tenant's KEK is a random key at tenants/<tenant>/kek, wrapped (AES-KW or AES-GCM, bound to the tenant and the master key's ID) by the master key, created create-only on the tenant's first wrap, every host racing to create it agreeing on one
- [ ] #2 FXDAX_MASTER_KEY (64 hex digits) or FXDAX_MASTER_KEY_FILE (32 raw bytes or 64 hex digits) gives the master key; several keys (comma-separated, or one per line) rotate it: wraps under the first, unwraps under any, and a rotation tool rewraps tenants' KEK objects under the newest
- [ ] #3 A tenant whose KEK object was deleted is refused as destroyed (not made anew): a tombstone or the TASK-411.19 marker tells a deleted KEK from one never made; doc-16 says how shredding a tenant works and what bucket versioning it needs
- [ ] #4 The master key is never logged nor written to the bucket; a wrong master key is refused naming its ID, not taken as a missing tenant
- [ ] #5 Tests: wrap/unwrap, first-use creation racing on two hosts, rotation, deletion refused as destroyed, wrong master key, and a sealed store end to end through it
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. extent_keys::kms::master: MasterKeys from FXDAX_MASTER_KEY (hex keys, commas/whitespace, first = newest) or FXDAX_MASTER_KEY_FILE (32 raw bytes, or hex text as the variable), both set refused, IDs = 8 bytes of SHA-256 over a domain label and the key (hex); buffers zeroized, no error echoes key text.
2. extent_keys::kms::bucket: formats only. KEK object tenants/<t>/kek: 'fxkt', format 1, kind (1 KEK, 2 tombstone), tenant, KEK id, master ID, AES-KW of the KEK under HKDF(master, header); tombstone: header alone. Keys the bucket KMS wraps: 'fxkb', format 1, tenant, KEK id, AES-KW under HKDF(KEK, header). New kms::Error::UnknownMasterKey naming the object's master ID and the host's. Kms trait gains forget(tenant) (default nothing), called by keys::Kms::revoked.
3. extent_remote::layout::kek(tenant), Kind::Kek kept by every pass and fault table (collector, testing, fork/restore faults).
4. extent_remote::bucket_kms::BucketKms<B: Bucket>: wrap loads or makes the tenant's KEK (GET; absent: sealed marker there -> Destroyed, else create-only, a create losing the race takes the object that landed), then wraps; unwrap loads (absent or tombstone -> Destroyed). Per-host cache of unwrapped KEKs, re-read after FRESH_FOR (60 s), dropped by forget. destroy(tenant): KEK object replaced by a tombstone (conditional), never made again. rotate(bucket, keys): every tenant's KEK object rewrapped under the newest master key by conditional update. from_env(bucket): FXDAX_KMS if set, else the bucket KMS if a master key is given, else none (TASK-411.28 makes that an error).
5. Wiring: fxdax::remote::kms() through from_env over the host's bucket (FXDAX_EXTENTD_REMOTE; none with a master key -> fail closed, as a bad FXDAX_KMS); fxdax-gc's kms() over its URL's bucket; fxdax-gc URL --rotate-master-key; fxdax::remote::shred_tenant (tombstone, then forget), control API ShredTenant, fxdaxctl shred-tenant; Go regenerated.
6. Tests: formats and parsing (extent_keys); bucket_kms: wrap/unwrap, two hosts racing creation, rotation, destroy -> Destroyed with no new KEK, manual deletion with marker -> Destroyed, wrong master key named, no master key bytes in any object/log/error, collector sweep leaves KEK objects; extent_store sealed store end to end on two hosts through BucketKms; fxdax: env wiring (FXDAX_MASTER_KEY alone seals) and shred_tenant fencing.
7. Docs: doc-16 'The bucket KMS', doc-7 layout rows, README encryption paragraph.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Decisions (2026-11-15):
- Crypto and formats in extent_keys (kms::master: MasterKeys, IDs; kms::bucket: KekObject, TenantKek, wrap/unwrap), storage in extent_remote::bucket_kms (BucketKms<B: Bucket>, destroy, rotate, from_env): one crate holds what is secret.
- Shredding leaves a tombstone AT tenants/<t>/kek (conditional overwrite), not a second object: no delete-then-create race, and rotation's conditional update fails against it. A KEK object deleted outright reads destroyed if the tenant's sealed marker (TASK-411.19) is there; otherwise a new KEK is made (no sealed cell could exist without the marker).
- Kms trait gains forget(tenant) (default no-op; Counted/Forgetting forward it); keys::Kms::revoked calls it, so ForgetTenantKeys drops the bucket KMS's cached KEK. Cache entries are re-read after FRESH_FOR (60 s), so untold hosts refuse a shredded tenant within a minute.
- New kms::Error::UnknownMasterKey names the object's master ID and the host's IDs (EACCES via Error::Kms), never NoSuchTenant.
- The bucket KMS's bucket is the host's: FXDAX_EXTENTD_REMOTE for fxdax processes, the URL for fxdax-gc. A master key with no host bucket fails closed (Unreached KMS), as a bad FXDAX_KMS does. FXDAX_KMS wins over a master key (warned).
- Rotation tool: fxdax-gc URL --rotate-master-key (bucket-wide maintenance, beside the collector; no host involved). Shred: control API ShredTenant (FAILED_PRECONDITION/ENOTSUP under another KMS), fxdaxctl shred-tenant, fxdax::remote::shred_tenant; Go regenerated, go/generate.sh --check passes.
- No new dependency (aws-lc-rs digest for IDs, already used).
- Linux-only code (control.rs, fxdaxctl through(), tests/control.rs) checked and clippy-clean with cargo clippy --target x86_64-unknown-linux-gnu (zig cc as the C cross compiler); the new tests/control.rs test is NOT run (needs Linux FUSE/extentd: ubu1).
<!-- SECTION:NOTES:END -->
