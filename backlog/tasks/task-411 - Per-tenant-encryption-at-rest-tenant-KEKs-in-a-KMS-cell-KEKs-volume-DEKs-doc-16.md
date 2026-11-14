---
id: TASK-411
title: >-
  Per-tenant encryption at rest: tenant KEKs in a KMS, cell KEKs, volume DEKs
  (doc-16)
status: To Do
assignee: []
created_date: '2026-11-14 18:33'
updated_date: '2026-11-14 18:35'
labels: []
dependencies: []
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
priority: medium
type: feature
ordinal: 797000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Tenants need BYOK (their root key in a KMS they control and can revoke), crypto-shredding (destroying that key leaves their data unreadable in the bucket, its versions and hosts' disks), and a bucket that tells a reader nothing of their data. Today every object is plaintext. doc-16 is the design and records the user's decisions of 2026-11-14; host compromise is out of scope, as DAX needs plaintext pages in the extent cache.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A tenant whose KEK is destroyed in the KMS has no readable data in the bucket, in WAL segments, the part cache or open segments: a test reads each and fails to decrypt
- [ ] #2 A bucket read without keys shows no byte of a volume's file data: a test writes known content and finds it in no object or local file
- [ ] #3 Forks, points, shared points, followers, FUSE mounts, sqlite volumes, restores and the collector work on encrypted stores: their existing tests pass with encryption on
- [ ] #4 doc-16, doc-7 and doc-14 describe what main does
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
