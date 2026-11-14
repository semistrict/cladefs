---
id: TASK-411.1
title: >-
  A KMS trait, a local fake, and key types: wrapped keys, generations, zeroized
  in memory
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-14 18:33'
updated_date: '2026-11-14 19:16'
labels: []
dependencies: []
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
type: task
ordinal: 798000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Every later step of doc-16 wraps or unwraps keys. Real KMS providers come later (the user's decision); a trait with a file-backed fake lets the rest be built and tested now, including revocation and destruction for shredding tests.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A trait wraps and unwraps a key under a tenant KEK, and the fake implements it, keeping KEKs in a local directory
- [ ] #2 The fake can revoke and destroy a tenant KEK, after which unwrap fails with an error naming the tenant
- [ ] #3 Unwrapped keys are zeroized on drop and have no Debug or serde output of their bytes: a test checks Debug
- [ ] #4 AES-256-GCM, AES-256-XTS and HKDF come from aws-lc-rs (the user's decision; Apache-2.0 and ISC), or storage_xts for XTS if aws-lc-rs has none
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
kms module (commit cba47aed3, branch worktree-agent-aae1193f67894a793): Kms trait (async wrap/unwrap by tenant u64), kms::Error naming the tenant (NoSuchTenant, Exists, Revoked, Destroyed, Unauthentic, Io), LocalKms file-backed fake (create/rotate/revoke/unrevoke/destroy/versions, KEK files 0600, dirs 0700), wrapped format: 'fxkw' | 1 | tenant u64 BE | KEK version u32 BE | AES-KW (57 bytes), wrapping key HKDF(KEK version, info = header). testing::Counted behind feature 'testing'. 21 extent_keys tests pass on macOS. Open: no retiring of single old KEK versions (all kept until destroy).
<!-- SECTION:NOTES:END -->
