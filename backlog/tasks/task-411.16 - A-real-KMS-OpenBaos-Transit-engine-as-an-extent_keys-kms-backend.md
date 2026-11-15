---
id: TASK-411.16
title: 'A real KMS: OpenBao''s Transit engine as an extent_keys::kms backend'
status: To Do
assignee: []
created_date: '2026-11-15 03:35'
updated_date: '2026-11-15 11:36'
labels: []
dependencies: []
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
priority: medium
type: feature
ordinal: 814000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-16 left real KMS backends for later; the user chose OpenBao Transit, AWS KMS and Google Cloud KMS (2026-11-14). Why this one: OpenBao (MPL-2.0) is self-hostable and OSI-licensed (Vault is BSL, excluded): a real KMS testable anywhere with `bao server -dev`, in CI too. URL: openbao://HOST[:PORT]/MOUNT?key=TEMPLATE (openbaos:// over TLS); token from CLADEFS_KMS_OPENBAO_TOKEN or a token file. Each tenant's KEK is named by kms::KeyNames (a template with {tenant}, the tenant in hexadecimal, plus a per-tenant overrides file for tenants who bring their own key).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 CLADEFS_KMS=openbao://HOST[:PORT]/MOUNT?key=TEMPLATE (openbaos:// over TLS); token from CLADEFS_KMS_OPENBAO_TOKEN or a token file gives a kms::Shared whose wrap and unwrap use the KMS's own encrypt/decrypt (or wrap) of the 32-byte key under the tenant's named key, any key version the KMS keeps unwrapping
- [ ] #2 The KMS's refusals map to kms::Error naming the tenant: key not found NoSuchTenant, access denied or key disabled Revoked, key destroyed or scheduled for deletion Destroyed, wrapped bytes not the key's Unauthentic, network and service errors Io
- [ ] #3 A wrapped key carries which backend and key name wrapped it, so that a key wrapped by another backend or another tenant's key is refused as not authentic rather than sent to the KMS
- [ ] #4 Integration tests: tests against a real `bao server -dev` (Mac and Linux; CI job or test script starting it); they cover wrap, unwrap, a rotated key unwrapping old wraps, revocation and destruction mapped as above, and a store opened sealed end to end through the backend
- [ ] #5 Any new dependency is OSI-licensed (stated in the task); no secret is logged; doc-16 and the README say how to configure it
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
