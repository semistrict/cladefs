---
id: TASK-562
title: 'Forget never halts the FUSE mount''s stores: a shredded cell stays readable'
status: To Do
assignee: []
created_date: '2026-11-18 15:26'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 953000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), shred-everywhere-kms review; verified against the code by independent reviewers. Severity: high.

**Where:** `crates/cladefs/src/bucket.rs`: `rg -n -F 'let stores = crate::halt::halt_stores(|cell| scope.covers(cell), Errno::EACCES, &halted);' crates/cladefs/src/bucket.rs`

`bucket::forget` halts stores only through `halt::HALTS`, and only `OpenCell` registers there (`open_cell.rs` `Halt::new`, in `OpenCell::new` and `follow`). Since TASK-475.2 the extent daemon serves its FUSE mount of cells in-process (`cladefsd/mount.rs`, `fuse/control.rs` attach -> `Storage::take` -> `ImageDevice` -> `storage/kept.rs` `KeptImage`, a bare `LocalCell<HostDir>` with its `Uploader`). These stores have no `Halt`, so `halt_stores` never finds them (rg finds no halt/forget in storage/, filesystem.rs, fuse/control.rs or cladefsd/mount.rs). The other steps of `forget` miss them too. `cache::forget` doesn't apply because `ImageDevice` reads the store directly, not through the extent cache. `forget_kept` and `HostRing::forget` stop only reads that go to the bucket. The store keeps its own `Ring` and `RawEncryption` keys. It keeps serving everything local (the open segment, write-ahead log segments not yet acknowledged, own extents) and Fxfs's in-memory metadata. It keeps taking writes and commits. Its uploader keeps swapping the shredded head, because `head::swap` doesn't refuse a shredded head (only `head::take` checks `head.shredded`). The FUSE server also answers opens with `FOPEN_KEEP_CACHE` and 24 h entry and attr TTLs (`TIMEOUT_SECS`), and nothing invalidates them, so the kernel keeps serving the file pages it cached. The reply's `Forgotten.stores` doesn't count these stores, so the operator is told the host forgot the cell.

**Trigger.** 1. Run cladefsd with `CLADEFSD_MOUNT`/`CLADEFSD_MOUNT_STORES`. 2. `cladefsctl attach` (Mount.Attach) cell T/C; write a file under MOUNT/T/C. 3. The operator calls `ShredCell{T,C}` on that daemon (or `ForgetTenantKeys{T}` after destroying T's KEK, or `ShredTenant{T}` under the bucket KMS). 4. The reply says `forgotten.stores = 0` for the mounted cell. 5. Reading the file through the mount still returns plaintext, from the kernel page cache or from the store's local data. Writes and fsync still succeed, and the uploader keeps putting new segments, encrypted under the shredded cell's DEK, into the bucket and swapping its head. This lasts until someone detaches the cell or the daemon drains.

**Why the tests stay green.** TASK-411.7's Linux tests (tests/control.rs `encrypted_stores_served_and_shredded_through_the_control_api`, `tenants_encrypted_under_the_bucket_kms_and_shredded_through_the_control_api`) shred only stores that VM connections opened (`daemon.open`, which is an OpenCell). tests/fuse_mount.rs never shreds. When TASK-411.7 landed, the mount ran as a process of its own (`cladefs-mount --control`), out of the daemon's reach. TASK-475.2 moved it into the daemon without giving its stores a Halt.

**Backlog.** No task covers it. TASK-411.7 (Done) made forget halt every store open in the process but tested only stores opened by VM connections. TASK-475.2 (Done) later moved the FUSE mount into the daemon without giving its stores a Halt.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 On a daemon serving a cell through CLADEFSD_MOUNT, ShredCell, ForgetTenantKeys and ShredTenant all halt the mounted cell's store. Afterwards, opening, reading (pages the kernel had cached included) and writing a file through the mount fail with EACCES, and no further segment of the cell is uploaded.
- [ ] #2 The forget reply counts the mounted store: forgotten.stores is 1 for one attached cell.
- [ ] #3 A Linux test in tests/control.rs starts a daemon with CLADEFSD_MOUNT, attaches a cell of tenant 1, writes a file and shreds it, then asserts both criteria. It repeats this for ForgetTenantKeys and ShredTenant. A unit check asserts that every store kind the daemon opens (OpenCell and KeptImage) is listed in HALTS. Both go red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
