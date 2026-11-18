---
id: TASK-633.6
title: >-
  KMS per bucket, revocation per KMS, shredding across them; an Azure Key Vault
  backend
status: To Do
assignee: []
created_date: '2026-11-18 17:03'
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
