---
id: TASK-411.16
title: 'A real KMS: OpenBao''s Transit engine as an extent_keys::kms backend'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 03:35'
updated_date: '2026-11-15 04:06'
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
doc-16 left real KMS backends for later; the user chose OpenBao Transit, AWS KMS and Google Cloud KMS (2026-11-14). Why this one: OpenBao (MPL-2.0) is self-hostable and OSI-licensed (Vault is BSL, excluded): a real KMS testable anywhere with `bao server -dev`, in CI too. URL: openbao://HOST[:PORT]/MOUNT?key=TEMPLATE (openbaos:// over TLS); token from FXDAX_KMS_OPENBAO_TOKEN or a token file. Each tenant's KEK is named by kms::KeyNames (a template with {tenant}, the tenant in hexadecimal, plus a per-tenant overrides file for tenants who bring their own key).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 FXDAX_KMS=openbao://HOST[:PORT]/MOUNT?key=TEMPLATE (openbaos:// over TLS); token from FXDAX_KMS_OPENBAO_TOKEN or a token file gives a kms::Shared whose wrap and unwrap use the KMS's own encrypt/decrypt (or wrap) of the 32-byte key under the tenant's named key, any key version the KMS keeps unwrapping
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

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. kms/openbao.rs: OpenBao, a Kms over Transit's encrypt/decrypt (reqwest+rustls, on a tokio runtime of its own so its futures run on any executor). URL openbao://HOST[:PORT]/MOUNT?key=T[&keys=F][&namespace=NS][&ca=PEM] (openbaos:// = HTTPS, ca= trusts only that CA); token from FXDAX_KMS_OPENBAO_TOKEN or the file FXDAX_KMS_OPENBAO_TOKEN_FILE names (re-read on a 403, the request retried once with a changed token: OpenBao Agent's file sink).
2. Wrapped = header (magic fxkm, format, backend 'openbao', tenant, key name) + Transit's ciphertext; the header sent as associated_data (and context, for derived keys), so the server authenticates it; unwrap refuses locally a header of another magic/backend/tenant/key name.
3. Error mapping verified against bao 2.7.2: 400 'encryption key not found' on decrypt -> Destroyed (the header says the key existed); soft-deleted -> Revoked; 'too old' -> Destroyed if trimmed below min_available_version, else Revoked; AEAD failure / invalid ciphertext -> Unauthentic; 403 -> GET keys/NAME probe: 404 NoSuchTenant (wrap) / Destroyed (unwrap), 200 Revoked, 403 -> lookup-self: valid token Revoked, else Io (fxdax's token refused); 404 route (no mount), 5xx, 429, network -> Io after retries (3 tries, 100 ms then 400 ms; 5 s connect, 15 s request timeouts).
4. Test helper extent_keys::testing::openbao::DevServer: starts bao server -dev on a free port (TLS optionally), CLI wrappers for admin; skips with a reason when bao is absent unless FXDAX_REQUIRE_OPENBAO=1.
5. Tests in openbao.rs: URL/token parsing; wrap/unwrap; rotation; revocation (policy, soft delete, min_decryption_version); destruction (trim, delete); unknown tenant; bad/rotated token file; local refusals made without a server; shared key across tenants refused by AD; tampering; down/sealed server -> Io; namespace; openbaos:// with ca=; kms::from_url via env.
6. End to end: extent_store encrypted.rs World made generic over its KMS; a sealed cell made and taken by a second host through OpenBao, then refused when revoked.
7. scripts/lib/openbao.sh + scripts/test-openbao.sh (release binary by digest, FXDAX_REQUIRE_OPENBAO=1) for CI; CI step proposed in notes, not added.
8. Docs: doc-16 'Real KMSes: OpenBao', README encryption paragraph.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented kms/openbao.rs (OpenBao, Token), testing/openbao.rs (DevServer), extent_store encrypted.rs World generic over its KMS plus sealed_stores_open_through_openbao, scripts/lib/openbao.sh + scripts/test-openbao.sh, doc-16 'Real KMSes: OpenBao', README.

Behaviour verified against bao 2.7.2 (brew, MPL-2.0) by hand with curl before coding, then by the tests: encrypt upserts a missing key for a token with create (root); a least-privilege token (update only) gets 403 for a missing key's encrypt, 400 'encryption key not found' for its decrypt; an unknown token and a token without rights both 403; soft-delete (DELETE keys/NAME/soft-delete) refuses both calls with 400 'refusing to use soft-deleted key', restorable; min_decryption_version raised: 400 'disallowed by policy (too old)', reversible; trim (min_available_version) the same message, versions gone; AEAD failure 400 'cipher: message authentication failed'; 'invalid ciphertext: version is too new'; sealed 503 'Vault is sealed'; missing mount 404 'no handler for route' for root, 403 for a restricted token; a missing key's GET keys/NAME is 404 with errors []; associated_data refused for non-AEAD types (rsa), context ignored by non-derived keys.

Decisions: header (fxkm, 1, 'openbao', tenant, key name) sent as associated_data AND context: binds the tenant, which matters when overrides give two tenants one key (a mutant sending no header made shared_keys_unwrap_only_their_tenants_wraps fail, then restored); context makes derived keys work. 403 probed (GET keys/NAME, then auth/token/lookup-self) so that NoSuchTenant (cells in the clear) is said only for a key OpenBao says it lacks, and an expired host token is Io, not every tenant Revoked. Soft-delete is Revoked (restorable; OpenBao has no scheduled deletion), trim Destroyed, delete Destroyed on unwrap but NoSuchTenant on wrap (OpenBao forgets deleted keys) - documented in doc-16 with how to keep a shredded tenant refused. Token file re-read on a 403, call retried once if changed. Own 1-thread Tokio runtime: futures run under any executor (tests use futures::executor::block_on); shutdown_background on drop so it drops inside a runtime too (tested). Retries: 3 tries, 100 ms then 400 ms, transport/412/429/5xx; 5 s connect, 15 s request timeouts; redirects off.

Dependencies (all already in Cargo.lock via object_store/extent_remote, no new crate): reqwest 0.13.5 MIT OR Apache-2.0 (features rustls: rustls Apache-2.0 OR ISC OR MIT, aws-lc-rs, rustls-platform-verifier MIT OR Apache-2.0), tokio 1.53.1 MIT, serde_json 1.0.154 MIT OR Apache-2.0, base64 0.22.1 MIT OR Apache-2.0, bytes 1.12.1 MIT, url 2.5.8 MIT OR Apache-2.0, zeroize feature serde; tempfile (MIT OR Apache-2.0) optional for the testing feature. OpenBao itself MPL-2.0 (test tool only). Cargo.lock, guest/tests/Cargo.lock (cargo metadata --offline; --locked passes after) and vendor/firecracker.lock (firecracker-cargo.sh --lock; --check passes) updated: extent_keys' dependency lists and zeroize's serde only.

CI proposal (not added): in ci.yml's object-stores job, a step 'The OpenBao KMS tests' running scripts/test-openbao.sh --no-fail-fast (fetches OpenBao 2.7.2's release by digest, FXDAX_REQUIRE_OPENBAO=1 so nothing skips). No secrets needed. Verified on the Mac with the release binary and no brew bao on PATH: 18/18.
<!-- SECTION:NOTES:END -->
