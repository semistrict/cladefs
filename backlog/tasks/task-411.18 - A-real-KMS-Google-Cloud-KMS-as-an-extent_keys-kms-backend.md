---
id: TASK-411.18
title: 'A real KMS: Google Cloud KMS as an extent_keys::kms backend'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 03:35'
updated_date: '2026-11-16 18:28'
labels: []
dependencies: []
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
priority: medium
type: feature
ordinal: 816000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-16 left real KMS backends for later; the user chose OpenBao Transit, AWS KMS and Google Cloud KMS (2026-11-14). Why this one: tenants on GCP grant cladefs cloudkms.cryptoKeyEncrypterDecrypter on their key; the user allowed GCP for testing this backend (2026-11-14), project echophase-connectome-test. URL: gcp-kms://projects/P/locations/L/keyRings/R?key=TEMPLATE, credentials from Application Default Credentials. Each tenant's KEK is named by kms::KeyNames (a template with {tenant}, the tenant in hexadecimal, plus a per-tenant overrides file for tenants who bring their own key).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 CLADEFS_KMS=gcp-kms://projects/P/locations/L/keyRings/R?key=TEMPLATE, credentials from Application Default Credentials gives a kms::Shared whose wrap and unwrap use the KMS's own encrypt/decrypt (or wrap) of the 32-byte key under the tenant's named key, any key version the KMS keeps unwrapping
- [x] #2 The KMS's refusals map to kms::Error naming the tenant: key not found NoSuchTenant, access denied or key disabled Revoked, key destroyed or scheduled for deletion Destroyed, wrapped bytes not the key's Unauthentic, network and service errors Io
- [x] #3 A wrapped key carries which backend and key name wrapped it, so that a key wrapped by another backend or another tenant's key is refused as not authentic rather than sent to the KMS
- [x] #4 Integration tests: tests against real Cloud KMS in echophase-connectome-test, one test key ring reused (keys cannot be deleted, only versions destroyed), skipped with a message unless credentials are present; CI stays without GCP secrets unless the user says otherwise; they cover wrap, unwrap, a rotated key unwrapping old wraps, revocation and destruction mapped as above, and a store opened sealed end to end through the backend
- [x] #5 Any new dependency is OSI-licensed (stated in the task); no secret is logged; doc-16 and the README say how to configure it
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. kms/real.rs (shared, small): the header of what a real KMS wraps: magic fxkr, format 1, the backend's scheme and the key's full name (each length-prefixed), then the KMS's ciphertext; open() refuses another backend's or another key's as Unauthentic before any call.
2. kms/gcp.rs: from_url parses gcp-kms://projects/P/locations/L/keyRings/R?key=TEMPLATE[&keys=FILE] (KeyNames::of_query; a name not starting projects/ is a cryptoKey id in the URL's ring, else a full key resource name, each checked at from_url, overrides via a new KeyNames::overrides()); credentials: CLADEFS_KMS_GCP_TOKEN (development: a static bearer token) or else object_store's GCP credential provider (ADC: GOOGLE_APPLICATION_CREDENTIALS, the gcloud well-known file, the metadata server), no new crate; a REST client over reqwest: :encrypt / :decrypt with additionalAuthenticatedData cladefs-tenant:<hex>, CRC32C (crc-fast Crc32Iscsi) of plaintext, AAD, ciphertext checked both ways; errors mapped from google.rpc status (NOT_FOUND NoSuchTenant; PERMISSION_DENIED and DISABLED Revoked; DESTROYED/DESTROY_SCHEDULED Destroyed; decrypt INVALID_ARGUMENT Unauthentic; else Io), verified against the service first with curl.
3. GCP: enable cloudkms API on echophase-connectome-test; key ring cladefs-test in us-central1; fixed keys cladefs-test-<tenant hex> created if missing by the tests (destroyScheduledDuration 24h); rotations destroy the version they replace.
4. Tests: unit tests (URL parsing, header, error mapping from recorded service bodies); integration tests in crates/extent_keys/tests/gcp_kms.rs against the real service, skipped with a message unless CLADEFS_KMS_GCP_TEST_RING names a ring: wrap/unwrap, rotation, disabled version Revoked, destroyed version Destroyed, missing key NoSuchTenant, foreign project PERMISSION_DENIED Revoked, other tenant/backend/tampered Unauthentic; and an cladefs store opened sealed end to end with CLADEFS_KMS=gcp-kms://...
5. Docs: doc-16 section Real KMSes: Google Cloud KMS; README encryption paragraph. Locks: firecracker lock, guest/tests lock.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Built (2026-11-15): kms/gcp.rs (backend), kms/real.rs (wrapped header shared by the real backends: fxkr, format 1, scheme length byte + scheme, key name u16 BE length + name, then the KMS ciphertext; 113 bytes of ciphertext for a 32-byte key, software), KeyNames::overridden() (additive), testing/gcp.rs (Admin: keys made if missing, rotate, set_state, destroy; eventually()), kms/gcp/service.rs (real-service tests), fxdax extents::tests::sealed_stores_open_through_google_cloud_kms.
AAD: fxdax-tenant:<tenant hex>. CRC32C (crc-fast Crc32Iscsi) on plaintext, AAD, ciphertext both ways; encrypt answer must have verifiedPlaintextCrc32c and verifiedAdditionalAuthenticatedDataCrc32c true and name a version of the key asked.
Client choice: reqwest 0.13 (rustls, aws-lc-rs, platform verifier) + serde_json + base64 + crc-fast + tokio timer + url query parsing, and object_store's GCP credential provider (GoogleCloudStorageBuilder::from_env().with_bucket_name(placeholder).build().credentials(): ADC as fxdax's gs:// buckets have it, token cache/renewal). No new crate in any lock: all MIT OR Apache-2.0 (object_store MIT/Apache-2.0, tokio MIT). Rejected google-cloud-kms-v1/google-cloud-auth (Apache-2.0): gax stack for two calls, a second credential story. Gap: object_store has no external_account (workload identity federation outside GCP) or impersonation. Dev path: FXDAX_KMS_GCP_TOKEN (static bearer); GOOGLE_CLOUD_QUOTA_PROJECT -> x-goog-user-project. Production ADC path NOT exercised against the service (no ADC on the Mac; not set up per instructions).
Error mapping verified with curl against the service 2026-11-15 (bodies in the unit test refusals_are_mapped_as_cloud_kms_answers_them): NOT_FOUND 'CryptoKey NAME not found.' (also for a missing key ring) -> NoSuchTenant; other NOT_FOUND ('Project ... not found.', KMS_RESOURCE_NOT_FOUND_IN_LOCATION) -> Io; PERMISSION_DENIED (IAM_PERMISSION_DENIED; e.g. projects/cloud-samples-data) -> Revoked; FAILED_PRECONDITION with PreconditionFailure KEY_DISABLED -> Revoked, KEY_DESTROY_SCHEDULED -> Destroyed (KEY_DESTROYED not seen: versions reach DESTROYED a day later; mapped by the same rule, message fallback 'current state is: S.'); INVALID_ARGUMENT on decrypt ('Decryption failed.' wrong AAD, 'the ciphertext is invalid', INDECIPHERABLE_CIPHERTEXT_FOR_CRYPTOKEY) -> Unauthentic, but INVALID_CHECKSUM -> Io (retried); UNAUTHENTICATED (401) -> Io; UNAVAILABLE/INTERNAL/DEADLINE_EXCEEDED/untyped 5xx/429/network -> Io, retried twice (200 ms, 1 s). Timeouts 30 s per call, 10 s connect. State changes took seconds to ~2 min to take effect.
GCP resources (project echophase-connectome-test): enabled cloudkms.googleapis.com; key ring projects/echophase-connectome-test/locations/us-central1/keyRings/fxdax-test; keys (SOFTWARE, GOOGLE_SYMMETRIC_ENCRYPTION, destroyScheduledDuration 86400s): fxdax-probe (curl probing; versions 1, 2 destroy-scheduled, none enabled), fxdax-test-1 (1 enabled), fxdax-test-2 (rotation test: each run adds a version and destroys the old; 1 enabled), fxdax-test-3 (disable/enable test, 1 enabled), fxdax-test-4 (destroyed-primary test: each run adds one version and destroys all; 0 enabled), fxdax-test-5 (fxdax store test, 1 enabled). Billing: about 4 active software key versions ($0.06/month each, list price) plus $0.03 per 10k operations; destroyed versions are not billed. Keys and the ring can't be deleted. No IAM changes made; no other GCP resource.
Filed: TASK-411.21 (rotated tenant KEK: old version destroyed shreds older cells; no rewrap), TASK-411.22 (misnamed key ring reads as NoSuchTenant: cells in the clear).

Validation (macOS, 2026-11-15, FXDAX_KMS_GCP_TOKEN set): cargo nextest run -p extent_keys -p extent_remote -p extent_store -p fxdax -E 'not (package(extent_store) & test(/^sim::/))': 962 run, 961 passed, 1 killed: fxdax storage::image::tests::flipped_bytes_fail_reads hung >3200 s under load (no KMS on its path; alone 30/30 pass in 0.35 s), filed TASK-425. The 4 kms::gcp::service tests and fxdax sealed_stores_open_through_google_cloud_kms passed in it (rotation 127 s and disable 61 s under load: state changes take effect eventually), and in 2 earlier runs of their own (3-5 s). Without the token they pass, printing why they skip. clippy --all-targets on extent_keys and fxdax: nothing in touched files. Locks: Cargo.lock, vendor/firecracker.lock (scripts/firecracker-cargo.sh --lock; --check passes), guest/tests/Cargo.lock (cargo metadata --locked passes): extent_keys' dependency lists only, no new package. Not done: Linux/ubu1 (not allowed here), scripts/upstream-changes.py --check, the ADC credential path against the service (AC #1 left unchecked for that). Billing caveat: DESTROYED versions are not billed; whether DESTROY_SCHEDULED ones are (a day each here) was not confirmed from the pricing page.

On main as dda9ba718 (TASK-411 squashed): macOS and ubu1 suites and encrypted e2e pass; upstream-changes.py --check passes. The unchecked acceptance criteria stay open: not verified in this session.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Google Cloud KMS backend (kms/gcp.rs) over the REST API: gcp-kms://projects/P/locations/L/keyRings/R?key=TEMPLATE[&keys=FILE], keys named by KeyNames (ids in the ring or whole resource names for BYOK), encrypt/decrypt with AAD fxdax-tenant:<hex> and CRC32C checks both ways, wrapped bytes behind kms/real.rs's shared header (backend and key name), refusals mapped as the service answered them (curl probes plus real-service tests), bounded retries for transient failures. Credentials: object_store's ADC provider (as gs:// buckets), FXDAX_KMS_GCP_TOKEN for development; no new crate. Verified: unit tests on recorded service bodies; 4 tests against real Cloud KMS (wrap/unwrap/tenant binding, rotation, disabled -> Revoked, destroy-scheduled -> Destroyed) and an fxdax store sealed end to end with FXDAX_KMS=gcp-kms://, all passing; broad macOS suite 961/962 (one unrelated hang, TASK-425). Open: Linux run, the ADC path against the service (AC #1), DoD items. Docs: doc-16 'Real KMSes: Google Cloud KMS', README. Filed TASK-411.21, .22, TASK-425.
<!-- SECTION:FINAL_SUMMARY:END -->
