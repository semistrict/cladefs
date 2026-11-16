---
id: TASK-474
title: 'One name per concept where the code collides: seal, remote, lease, durable'
status: To Do
assignee: []
created_date: '2026-11-16 02:13'
updated_date: '2026-11-16 02:13'
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
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
