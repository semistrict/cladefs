---
id: TASK-474
title: 'One name per concept, in code too: the renames CONTEXT.md decided'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-16 02:13'
updated_date: '2026-11-16 02:53'
labels:
  - naming
dependencies: []
ordinal: 878000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
CONTEXT.md (2026-11-15) fixes one word per concept, but the code reuses words for unrelated things, so a reader cannot tell from a name which is meant. "Sealed store" reads as both an encrypted store and a store of sealed extents; "remote" is both the bucket and the daemon client; a branch lease still appears in types and the proto after TASK-407 removed branch leases. Renames only, no behaviour change; the proto is ours and its format changes outright (no migrations).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Encryption no longer says seal: `extent_frame::Seal`, `extent_store::raw::Sealer`, `keys::Ring::sealing`/`is_sealed`, `layout::Sealing` and "sealed cell/store" in docs and tests use encrypted/encryption words; extent sealing keeps "seal"
- [ ] #2 The daemon client is not called Remote: `RemoteExtentStore`, `RemoteDevice` renamed (e.g. daemon client), and `extent_store::upload::Remote` (the uploader) named for what it is
- [ ] #3 No branch lease in names: `extent_store::upload::Lease`, `UploadStatus.lease`, extentd.proto `lease_token`/`lease_epoch`/`lease_taken_unix_ms`, and mount.proto/README "leases" name the head and its taking
- [ ] #4 extentd wire flags do not share virtio flag names at different bits (`COMMIT_DURABLE`, `COMMIT_ROOT`, `MAP_ZEROS` are 1<<1, 1<<0, 1<<1 in extentd/wire.rs and 1<<0, 1<<1, 1<<3 in virtio_extent): one set of bits, or distinct names
- [ ] #5 `Commit::Durable` (uploaded) and `Commit::Local` (durable locally) carry the doc-12 stage names, so "durable" alone names neither
- [ ] #6 Workspace builds, clippy clean, the macOS suite and one ubu1 run pass
- [ ] #7 Every rename in this task's notes is done, and CONTEXT.md's Ambiguities describe the code as it is
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Grilling, 2026-11-15, adds renames to this task (CONTEXT.md is the target): cell volume → vdisk (cell::Vdisk, CreateVdisk, VdiskKind, vdisk DEK, DIR/TENANT/CELL/VDISK); remote retired on both sides (extent_remote → bucket words, daemon client); Commit/COMMIT_* stages Ordered/Local/Uploaded; Store type is the extent store alone; points never called snapshots, GetSnapshot → stats; a commit number is never an epoch (virtio_extent_epoch, follow_epoch, StatStore committed); collector in types, gc only in binary/keys/metrics, cleaner retired; clone/restore → fork (Storage::clone_to, spec rules clones*). Daemon names are TASK-475's (cladefsd): do this after it lands, to not collide with its rename.

Round 3: placement only for a host holding a cell (state::Placement → Layout or Packing, see notes; cache Placement → the Arena it names); MAP_AHEAD → MAP_PREFAULT, map_ahead → prefault; tenant: uint64 ID only in every RPC, string names gone, /metrics shows the ID; segment life one enum Phase (seals.rs Stage merged into segments.rs Phase if one concept); stage only for the second stage; doc-12 stages → levels.

Round 4: state::Placement → Packing {Segment, Own}; one ExtentId type, storage_device's (cladefs::extents::ExtentId and extent_frame::table::Id go; ExtentIdLe stays the wire form; table::Id orders branch-first today, ExtentId sequence-first: the table format may change, no migration); `sequence` for an extent's, `segment` for a segment's number (Footer.segment, keys <epoch>.<segment>); CLADEFS_HOST* → CLADEFS_TEST_HOST*, drivers keep *-host.sh; upload::Lease → Taking, proto taken_token/taken_epoch/taken_unix_ms.

Round 5: cladefs::fence (panic/shred) → cladefs::halt (Halt, halt_stores, halted store); fence only for cut off by a later taking; host_pin → pinned_version, LangSmith pin never names the candidate (README, qualify.sh, qualify-budgets.py, test-e2e.sh); CLADEFS_EXTENTD_CACHE[_BYTES] → CLADEFSD_PART_CACHE[_BYTES] (with or after TASK-475); nightly: prose only.

Round 6: round = A/B only; a case's repeats → repeats (CLADEFS_E2E_REPEATS, repeat-K.json, CLADEFS_SOAK_ROUNDS → _REPEATS); same-named types must be one concept, else renamed (storage::Kept → KeptImage, fetch::Grant → Slot, keys::View → RingView, table::Footer → TableFooter, BLOCK → TABLE_BLOCK/ID_BLOCK/DIRECT_BLOCK…; generic Kind may stay); Follower.tla actions Advance → LetGo, Invalidate → Drop, rule drop, trace WaitBarrier → Wait (keep every mutation caught); SetTenantRoom → SetTenantInFlight, peer stream room → credit, room = segment free space only.

Decided without the user (OPEN_QUESTIONS.md): wal::BASE → START; MakePublic generation → attach_only; NO_TENANT → DEFAULT_TENANT; window changes confirmed, not acknowledged; extentd wire uses virtio's flag bits; Bucket.tla ck → ckpt; super-block (Fxfs on-disk) vs superblock (VFS), upstream untouched.
<!-- SECTION:NOTES:END -->
