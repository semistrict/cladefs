---
id: TASK-411.17
title: 'A real KMS: AWS KMS as an extent_keys::kms backend'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 03:35'
updated_date: '2026-11-15 04:10'
labels: []
dependencies: []
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
priority: medium
type: feature
ordinal: 815000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-16 left real KMS backends for later; the user chose OpenBao Transit, AWS KMS and Google Cloud KMS (2026-11-14). Why this one: the usual BYOK target: tenants grant fxdax kms:Encrypt/Decrypt on their CMK (key ARN, alias, or a key in another account by ARN). URL: aws-kms://REGION?key=TEMPLATE, credentials from the standard AWS chain (env, profile, instance role). Each tenant's KEK is named by kms::KeyNames (a template with {tenant}, the tenant in hexadecimal, plus a per-tenant overrides file for tenants who bring their own key).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 FXDAX_KMS=aws-kms://REGION?key=TEMPLATE, credentials from the standard AWS chain (env, profile, instance role) gives a kms::Shared whose wrap and unwrap use the KMS's own encrypt/decrypt (or wrap) of the 32-byte key under the tenant's named key, any key version the KMS keeps unwrapping
- [x] #2 The KMS's refusals map to kms::Error naming the tenant: key not found NoSuchTenant, access denied or key disabled Revoked, key destroyed or scheduled for deletion Destroyed, wrapped bytes not the key's Unauthentic, network and service errors Io
- [x] #3 A wrapped key carries which backend and key name wrapped it, so that a key wrapped by another backend or another tenant's key is refused as not authentic rather than sent to the KMS
- [ ] #4 Integration tests: tests against an OSI-licensed AWS KMS emulator (e.g. nsmithuk/local-kms, MIT) on the Mac and in CI, as CI holds no cloud credentials; they cover wrap, unwrap, a rotated key unwrapping old wraps, revocation and destruction mapped as above, and a store opened sealed end to end through the backend
- [x] #5 Any new dependency is OSI-licensed (stated in the task); no secret is logged; doc-16 and the README say how to configure it
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Client: aws-sdk-kms + aws-config (Apache-2.0, AWS's own): the full credential chain (env, shared profile/SSO/credential_process, web identity, ECS, IMDSv2), retries with backoff and error codes maintained upstream; a hand-made SigV4 REST client would still need aws-config for credentials, so it saves little and adds a protocol to maintain. Calls run on a runtime of the backend's own (one worker thread), so the futures poll on any executor and the HTTP client's connections never outlive a caller's runtime; the client is made lazily on it.
2. kms/aws.rs: AwsKms; from_url parses aws-kms://REGION?key=TEMPLATE[&keys=FILE][&endpoint=URL] (KeyNames::of_query). Wrap = Encrypt(KeyId = the tenant's key name, plaintext 32 bytes, EncryptionContext {fxdax-tenant: tenant hex}); Wrapped = header (magic 'fxka', format 1, key name length u16 BE, key name) + CiphertextBlob. Unwrap: header checked (magic, format, key name == KeyNames::of(tenant)) before any call, else Unauthentic; Decrypt(KeyId = key name, same context); plaintext must be 32 bytes.
3. Error mapping by the service's error code: NotFoundException -> NoSuchTenant; AccessDeniedException, DisabledException -> Revoked; KMSInvalidStateException 'pending deletion' -> Destroyed, other states -> Revoked; InvalidCiphertextException, IncorrectKeyException -> Unauthentic; everything else (throttling, KeyUnavailable, KMSInternal, dispatch/timeout, credentials) -> Io naming operation and key, no secrets. Unit tests of the mapping on synthetic SDK errors (AccessDenied, throttling: the emulator has no IAM nor throttling).
4. Emulator: nsmithuk/local-kms (MIT), pinned pseudo-version, installed by scripts/test-local-kms.sh into target/local-kms with go install; extent_keys::testing::LocalAwsKms (testing feature) starts it on PORT=0 in a temp dir, reads the bound port from its log, seeds keys, admin calls (create key/alias, disable, schedule deletion). Tests #[ignore = needs local-kms: scripts/test-local-kms.sh] as the RustFS/Azurite ones (the repo's pattern), named local_kms_*: wrap/unwrap, rotation (two instances seeded with one then two backing keys of the same key ID: old wraps unwrap), disabled -> Revoked, pending deletion -> Destroyed, not found -> NoSuchTenant, wrong tenant/context/backend/tamper -> Unauthentic, alias name/alias ARN/key ID/key ARN naming, overrides; extent_store: a sealed store opened end to end through kms::from_url(aws-kms://...&endpoint=...).
5. CI: object-stores job gains setup-go and scripts/test-local-kms.sh (dummy env credentials, no AWS account).
6. Lock files: Cargo.lock, vendor/firecracker.lock (scripts/firecracker-cargo.sh --lock), guest/tests check. Licenses of every new crate checked from cargo metadata.
7. Docs: doc-16 'Real KMSes: AWS KMS', README encryption paragraph (configuration, IAM policy with kms:EncryptionContext:fxdax-tenant condition, cross-account BYOK through overrides).
8. Verify: narrow nextest, then the four-package run; clippy, fmt.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Client: aws-sdk-kms 1.131.0 + aws-config 1.13.2 (both Apache-2.0, AWS's own). Why: the full standard credential chain (env, shared profile, SSO, credential_process, web identity, ECS, IMDSv2) with refresh, retries/backoff and error codes are maintained upstream; a hand SigV4 client over reqwest would still need aws-config (or object_store's providers, which lack profiles/SSO) for credentials and would add a JSON protocol to keep. aws-sdk-kms with default-features = false, features [default-https-client, rt-tokio]: its default 'rustls' feature pulls the legacy client (hyper 0.14, rustls 0.21, h2 0.3); this way it is hyper 1 / rustls 0.23 / aws-lc-rs, already in the build.
New crates and licenses (cargo metadata): arc-swap MIT OR Apache-2.0; aws-config, aws-credential-types, aws-runtime, aws-sdk-kms, aws-sdk-sso, aws-sdk-ssooidc, aws-sdk-sts, aws-sigv4, aws-smithy-async, aws-smithy-http, aws-smithy-http-client, aws-smithy-json, aws-smithy-observability, aws-smithy-query, aws-smithy-runtime, aws-smithy-runtime-api, aws-smithy-runtime-api-macros, aws-smithy-schema, aws-smithy-types, aws-smithy-xml, aws-types: Apache-2.0; base64-simd MIT; block-buffer MIT OR Apache-2.0; bytes-utils Apache-2.0/MIT; cpufeatures MIT OR Apache-2.0; deranged MIT OR Apache-2.0; num-conv MIT OR Apache-2.0; outref MIT; powerfmt MIT OR Apache-2.0; regex-lite MIT OR Apache-2.0; sha1 MIT OR Apache-2.0; signal-hook-registry MIT OR Apache-2.0; time, time-core, time-macros MIT OR Apache-2.0; urlencoding MIT; vsimd MIT; xmlparser MIT/Apache-2.0. extent_keys also uses url, tokio (workspace, already in the lock) and tempfile (optional, testing feature). Emulator: nsmithuk/local-kms, MIT (LICENSE checked), pinned pseudo-version v0.0.0-20260228092603-2a5e92d09ab6 (no module-path release tag), Go-installed into target/local-kms by scripts/test-local-kms.sh, not a dependency of the build.
Lock files: Cargo.lock, vendor/firecracker.lock (scripts/firecracker-cargo.sh --lock; --check passes), guest/tests/Cargo.lock (cargo metadata --locked failed before, passes after update). scripts/upstream-changes.py --check passes.
Wrapped format: 'fxka', format 1, key name length u16 BE, key name, CiphertextBlob. Encryption context {fxdax-tenant: tenant hex}. Decrypt names KeyId. Calls on a 1-thread runtime of the backend's own (client made lazily there), dropped with shutdown_background; attempt timeout 10 s, call 30 s.
Deviation from the brief: the emulator tests are #[ignore = needs local-kms: scripts/test-local-kms.sh] and named local_kms_*, as the RustFS/Azurite ones are (the repo's pattern), and fail loudly when run without FXDAX_LOCAL_KMS or credentials, rather than passing silently with a printed skip. Each still starts its own emulator on a free port (PORT=0, port read from its log).
Mutation checks (hand mutants, restored): Decrypt without context or KeyId -> 7/8 emulator tests fail; Decrypt without KeyId -> the another-key test failed only after it was rewritten to use a ciphertext under another key with the tenant's own context (the first version's relabelled blob was refused by context alone); constant context -> the shared-key cross-tenant test fails.
Filed: TASK-411.19 (a tenant whose key finished deletion reads NoSuchTenant: new cells in the clear), TASK-411.20 (changing a tenant's key name leaves earlier wraps unauthentic; no rewrap path).

Validation (8f59f2536, macOS): scripts/test-local-kms.sh 9/9 (8 extent_keys kms::aws local_kms_*, 1 extent_store upload::encrypted::local_kms_sealed_stores_open_through_aws_kms), also from a fresh install of local-kms; 6 non-emulator kms::aws unit tests pass; cargo nextest run -p extent_keys -p extent_remote -p extent_store -p fxdax -E 'not (package(extent_store) & test(/^sim::/))' 953/953 passed, 90 skipped; clippy --all-targets on extent_keys (testing) and extent_store: no new warnings; cargo fmt --check clean; shellcheck clean.
Left unchecked: AC #1, as only the environment leg of the credential chain was exercised (profile, web identity, ECS, instance role are aws-config's, untested here, no AWS account); AC #4, as CI's object-stores step is added but has not run (not pushed); DoD #1, Linux (ubu1) not run per the brief.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
AWS KMS backend in extent_keys/src/kms/aws.rs (AwsKms): FXDAX_KMS=aws-kms://REGION?key=TEMPLATE[&keys=FILE][&endpoint=URL]; Encrypt/Decrypt via aws-sdk-kms + aws-config (Apache-2.0; hyper 1/rustls 0.23 client), encryption context fxdax-tenant=tenant hex, Decrypt naming KeyId; wrapped = 'fxka' v1 + key name + ciphertext blob, refused unasked if another backend's or key's; errors mapped NotFound->NoSuchTenant, AccessDenied/Disabled->Revoked, pending deletion->Destroyed, InvalidCiphertext/IncorrectKey->Unauthentic, rest->Io. Verified against local-kms (MIT emulator, per-test on a free port, scripts/test-local-kms.sh, CI object-stores step added): 9/9 incl. rotation (seeded emulators), revoke/destroy, context/key binding, a sealed store end to end; four-package suite 953/953 on macOS. Docs: doc-16 'Real KMSes: AWS KMS', README. Open: AC #1 (non-env credential legs untested), AC #4 (CI not yet run), Linux DoD. Filed TASK-411.19, TASK-411.20.
<!-- SECTION:FINAL_SUMMARY:END -->
