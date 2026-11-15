---
id: TASK-411.27
title: >-
  The bucket KMS: tenants' KEKs kept in the bucket, wrapped by a master key from
  FXDAX_MASTER_KEY or FXDAX_MASTER_KEY_FILE, made on a tenant's first use; the
  default KMS
status: To Do
assignee: []
created_date: '2026-11-15 11:09'
updated_date: '2026-11-15 11:09'
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
