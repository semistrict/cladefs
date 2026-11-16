---
id: TASK-411.28
title: >-
  No plaintext: every cell sealed, always; the bucket KMS the default; a host
  with no KMS and no master key refuses to start
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 11:09'
updated_date: '2026-11-15 22:17'
labels: []
dependencies:
  - TASK-411.27
  - TASK-411.23
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
priority: high
type: task
ordinal: 824000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The user's decision (2026-11-15): remove plaintext entirely. Every cell is sealed; there is no clear mode, no opt-out. With the bucket KMS (TASK-411.27) as the default whenever CLADEFS_KMS is unset, and every process (extentd, cladefsctl, FUSE mounts, cladefs-gc, tools, tests) refusing to start without a KMS or a master key, the clear paths go: Seal::Clear objects, clear raw files and in-place framing in the clear (TASK-411.23 makes sealed in-place framing the only kind), the clear/sealed test twins, TASK-411.10's kekless cache, TASK-411.19's 'no such key means clear' choice and its marker if the bucket KMS no longer needs it, TASK-411.22 and TASK-411.24 (moot once no cell is ever clear). A deep refactor: prefer deleting code to keeping branches; one code path.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 No code path writes or reads an object, raw file, log or record in the clear: Seal::Clear and the clear branches are gone from extent_frame, extent_store, extent_remote, sqlite_extents and cladefs; a frame in the clear is refused as corrupt
- [ ] #2 Every process that opens stores refuses to start, naming CLADEFS_KMS and CLADEFS_MASTER_KEY, when neither a KMS nor a master key is configured; with only a master key it uses the bucket KMS
- [ ] #3 A KMS answering 'no such key' refuses the cell as destroyed, for every backend; the kekless cache and the clear-cell choice are gone; TASK-411.22 and TASK-411.24 closed as moot
- [ ] #4 Every test runs sealed (a test master key or LocalKms by default in test helpers); the clear twins are removed, the sealed variants kept; e2e unchanged (already sealed)
- [ ] #5 doc-16, doc-7, doc-14, README and the operator docs describe one sealed mode; the macOS and Linux suites and the encrypted e2e suite pass
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
One sealed path, layer by layer, each compiled and its tests run before the next; clear code deleted, not branched.
1. extent_keys: kms::Error::NoSuchTenant stays a backend's precise answer but every caller refuses it as destroyed; testing::MemoryKms (KEKs made on first wrap, in memory) as the tests' default KMS.
2. extent_frame: Seal kind 0 is corrupt ('an object in the clear'); Seal::Clear, Opening::Clear, Error::Clear go; Packer::new/keyed/in_place, framed, RawPart::of take a key, not an Option; NoKeys stays for keyless readers (collector marking, WAL recovery's whole-footer check: CRCs only).
3. extent_remote: bucket_kms::from_env returns the host's KMS or an error naming CLADEFS_KMS and CLADEFS_MASTER_KEY; keys::Host.kms, CellKeys.kms, Collector/Sweep kms mandatory; Kms::new_kek/new_cell_kek always a KEK or a refusal (NoSuchTenant refused as destroyed, pools dropped); kekless cache (KEKLESS_FOR) and the sealed marker (layout::sealed, Kind::Sealed, Kms markers, bucket KMS's marker read) removed: with no plaintext 'no such key' always refuses, and the bucket KMS's tombstone is how a tenant is shredded; a KEK object deleted outright loses the tenant's data as shredding does, new cells sealed under a new KEK, never in the clear. Ring::clear -> Ring::keyless (metadata readers only); writers' rings always seal (Ring::sealing not optional; keys::ring refuses a writer of a record with no DEK); layout::Sealing not optional; checkpoints always authenticated; cells always with a KEK (one without and not shredded is corrupt). Tests: Writer::open (test helper) opens sealed through the shared test KMS (cell made with a KEK, record given its DEK); frames under the writer's ring or a test DEK.
4. extent_store: RawFile always sealed (RawFile::clear, clear logs FXLOG v2 go), in-place framing and looks ahead sealed only, WAL recovery closes sealed only, upload's clear branches (CellKeys::none for no KMS, ring_of clear) go; test helpers default to the test KMS; local store tests seal under a test ring; clear twins removed.
5. sqlite_extents, cladefs (remote::kms mandatory: startup check in extentd, cladefsctl, CLI/FUSE; image, import, control attach), cladefs-gc, cladefs-gc-fill, cladefs-examples (test master key when none set), guest tests runner/harness, scripts (old pins documented).
6. Mutant patches re-pointed (crates/cladefs/mutants.sh --check, crates/extent_remote/mutants.sh --check, scripts/guest-mutants-host.py --check).
7. Docs: doc-16 (one sealed mode; what went), doc-7, doc-14, README, OPEN_QUESTIONS; close TASK-411.22/.24 as moot, notes on .10/.19/.26.
8. Verify on Mac: test.sh parts clippy workspace shuttle scripts go; sim sample (10 seeds a profile); firecracker-cargo --check; guest/tests cargo metadata --locked; upstream-changes --check; test-openbao.sh, test-local-kms.sh; Linux cross-check (zig).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Includes finding TASK-411.26's process (sandboxes: three tenant-0 cells made in the clear by a process that isn't the case's daemons): with no plaintext, it refuses to start; run scripts/sandboxes.sh short sealed on ubu1 after this task and settle TASK-411.26 and TASK-411.23 AC #1.

Progress 2026-11-15 (Mac):
- Marker decision: removed (layout::sealed, Kind::Sealed, bucket::put_if_absent/exists, Kms markers, the bucket KMS's marker read). With no plaintext, NoSuchTenant refuses as destroyed for every backend (keys::revokes), so a forgotten KEK and a tenant never given one are refused alike; the bucket KMS shreds by tombstone; a KEK object deleted outright gets a new KEK at the next wrap and what the old one wrapped is lost, as shredding loses it (bucket_kms test kek_objects_deleted_outright_lose_what_they_wrapped). Nothing it guarded can happen any more.
- Kekless cache (KEKLESS_FOR) removed; a refusal is noted as any revocation (Kms::revoked).
- Startup refusal: bucket_kms::no_kms(), returned by from_env; fxdax::remote::require_kms in fxdax-extentd, fxdaxctl locally(), fxdax-mount; fxdax-gc for collecting and --delete-branch; fxdax-gc-fill; the guest runner (sealed::require_kms).
- Found and fixed while sealing everything: image collectors (fxdax::storage::collect) had no KMS, so with every cell sealed they compacted nothing (storage::kept tests caught it); they hold the host's KMS now. fuse/control.rs attach (Linux-only) moved to the new cell-KEK API, giving the KEK back on a failed hold.
- Tests' KMS: extent_keys::testing::MemoryKms (unit tests, extent_remote::keys::testing) and KnownKms (every tenant's KEK one known key) behind fxdax's tests-kms feature (fxdax's and sqlite_extents' dev-dependencies; compile_error in release): a test's process and the fxdaxctl/extentd/fxdax-mount it starts agree with nothing shared, whatever bucket each uses. Benches call fxdax::remote::testing::sealed().
- Mutant patches: crates/fxdax/mutants/framed-in-copies.patch and large-extents-framed-in-copies.patch re-anchored (their hunks' context had the removed sectors.is_some()==packer.key().is_some() check and the sealing.is_some() arm); same mutation, same meaning (framing forced into copies). extent_remote's and the guest's applied unchanged.
- Filed TASK-460: a sealed take reads its cell's object twice (6 GETs where a clear take asked 5); remote::tests::takes_recover... expectation raised to 6 with a pointer to it.
- sim: SimBucket's journal described puts by a hash of their bytes; sealed bytes differ run to run (random keys and salts), so seeds_replay compared nothing deterministic. Puts are journaled by length now.

2026-11-15, after the delegate (rate-limited before committing; its work committed as b1ce5f5c6, renamed by scripts/rename-to-cladefs.sh and merged three-way onto the branch as c43a45345):
- Review (verifying-reviewer) found and a6e1d62e3 fixed: a ring holding keys read an object authenticated under a key it lacked as stored when its branch's record kept no DEKs, and records are unauthenticated: a bucket writer could strip an ancestor's DEKs and plant a checkpoint under a made-up key. keys::View::sealed is now true for every ring but the keyless one (test objects_authenticated_under_keys_unheld_are_refused_by_every_ring_holding_keys, failing before). Also sqlite_extents/tests/daemon.rs (Linux-only) took its writer under the tests' KMS, not the daemon's.
- Review found, filed as TASK-411.29: a bucket-KMS KEK object deleted outright leaves pooled cell KEKs under the deleted KEK.
- Linux-only, found on ubu1: cladefs-mount chose its KMS before blocking SIGTERM, so a KMS thread took SIGTERM and died (fuse_mount x3); fixed in 9fc2eaa46. A sealed raw file encrypted a write through a copy of its whole size, holding a 64 MiB write's size again outside extentd's room (hostile what_requests_hold_is_within_the_room); now a MiB at a time (9fc2eaa46).
- After merging main's TASK-433.5 page cache: Shuttle crash rounds ceased to be functions of their seed (a crash's hole in an open segment's uncommitted tail decrypts to noise of the raw file's random salt); under shuttle the salt is Shuttle's seeded generator's (82ba34832).
- Seen once each under load on ubu1, unexplained yet: disk.rs tenants_past_their_disk_limits_are_refused_while_others_write refused at 14 MiB of 16 (1 of 17 runs); extentd::tests::stores_fork_live_once_nothing_is_open passed then aborted (SIGABRT) at exit.
- macOS: test.sh (clippy, workspace 2575-2584, Shuttle 37/37, scripts 353, go) green on a6e1d62e3 and on the merge 94b98e83f but for the Shuttle round fixed since; real Cloud KMS 12/12; OpenBao 19/19; local-kms 9/9.
<!-- SECTION:NOTES:END -->
