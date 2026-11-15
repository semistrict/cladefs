---
id: TASK-411.1
title: >-
  A KMS trait, a local fake, and key types: wrapped keys, generations, zeroized
  in memory
status: Done
assignee:
  - '@claude'
created_date: '2026-11-14 18:33'
updated_date: '2026-11-15 03:23'
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
- [x] #1 A trait wraps and unwraps a key under a tenant KEK, and the fake implements it, keeping KEKs in a local directory
- [x] #2 The fake can revoke and destroy a tenant KEK, after which unwrap fails with an error naming the tenant
- [x] #3 Unwrapped keys are zeroized on drop and have no Debug or serde output of their bytes: a test checks Debug
- [x] #4 AES-256-GCM, AES-256-XTS and HKDF come from aws-lc-rs (the user's decision; Apache-2.0 and ISC), or storage_xts for XTS if aws-lc-rs has none
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
kms module (commit cba47aed3, branch worktree-agent-aae1193f67894a793): Kms trait (async wrap/unwrap by tenant u64), kms::Error naming the tenant (NoSuchTenant, Exists, Revoked, Destroyed, Unauthentic, Io), LocalKms file-backed fake (create/rotate/revoke/unrevoke/destroy/versions, KEK files 0600, dirs 0700), wrapped format: 'fxkw' | 1 | tenant u64 BE | KEK version u32 BE | AES-KW (57 bytes), wrapping key HKDF(KEK version, info = header). testing::Counted behind feature 'testing'. 21 extent_keys tests pass on macOS. Open: no retiring of single old KEK versions (all kept until destroy).

AC evidence: extent_keys kms tests (round trip, wrong tenant, tampered, rotate, revoke/unrevoke, destroy naming the tenant, 0600 files, Counted); tests::secrets_print_no_bytes (Debug), Secret zeroized on drop, no Serialize; AES-256-GCM, AES-KW, HKDF, HMAC from aws-lc-rs (KATs checked against LibreSSL AES and Python HKDF/GHASH); XTS from storage_xts over aes (TASK-411.5, IEEE 1619 vectors 10, 11 and aws-lc's EVP_aes_256_xts). Verified 2026-11-15 at 594ff82e9 (origin/main merged): macOS scripts/test.sh workspace 2466/2466, shuttle, scripts (mutants 0 broken), go; Linux ubu1 under fxjob (target on /mnt/fxdax-scratch), every part: clippy, workspace 2752/2752, sim 63/63, shuttle, scripts, go, guest, firecracker. Only fxdax's own crates changed (extent_keys, extent_frame, extent_remote, extent_store, sqlite_extents, fxdax, fxdax-examples): no upstream deviation; scripts/upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
New crate extent_keys: Secret (256-bit, zeroized, unprintable, not Clone), Salt, KeyId, Wrapped; HKDF subkeys, AES-256-GCM (Aead), AES-KW wrapping, HMAC over aws-lc-rs, XTS (xts::Xts, TASK-411.5) over storage_xts; kms::{Kms, LocalKms, Error} with a file-backed fake (KEK versions, rotate, revoke, destroy; wrapped blobs headed by tenant and version, AES-KW under a header-bound subkey) and testing::Counted. Known-answer tests checked against independent implementations. Verified on macOS and Linux (see notes).
<!-- SECTION:FINAL_SUMMARY:END -->
