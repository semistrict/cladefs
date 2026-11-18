---
id: TASK-508.1
title: 'A cell has one filesystem: the cell object''s vdisk map and cell cuts go'
status: In Progress
assignee: []
created_date: '2026-11-18 00:10'
updated_date: '2026-11-18 00:44'
labels:
  - bucket
  - host
dependencies: []
documentation:
  - doc-20
parent_task_id: TASK-508
ordinal: 898000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Main's cells (TASK-364) hold a map of named vdisks, each a branch of its own, and versions carry a cell_cut for whole-cell cuts (doc-18). With a cell being one filesystem (doc-20) the map, add/remove vdisk, the vdisk name rules and cell_cut have nothing to do; a cell has a kind instead. A VM's disk and memory become cells of their own, and the daemon's API, the FUSE mount and the Go bindings stop naming vdisks.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The cell object names no vdisks; a cell's kind is the cell's
- [x] #2 NewVersion and Version carry no cell_cut; VersionCut is NewVersion
- [x] #3 The daemon, the FUSE mount and the Go bindings make and open cells, not vdisks of cells
- [x] #4 Every test suite that passed before passes
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Bucket: cell::Object names one Filesystem { branch, kind }; hold refuses another branch (CellHasBranch); add/remove vdisk, names, VdiskExists/VdiskName/NoVdisk go; FORMAT 16.
2. Versions: VersionCut -> NewVersion, cell_cut gone (as t3/one-log-per-cell's 6e983a805).
3. Store: StoreFile and create_host take the cell's kind, no vdisk name; store format 15.
4. Host and daemon: vdisk_at goes; CreateStore into a cell refused once it has a filesystem (EEXIST).
5. FUSE mount: a cell shown at TENANT/CELL; Attach of a new cell makes its Fxfs filesystem (capacity); CreateVdisk and Vdisk go; cladefsctl mount create; Go regenerated.
6. Tests on macOS; Linux-only code checked on ubu1 from a bundle (no push).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done as planned. A cell's object names one cell::Filesystem { branch, kind }; holding it with another branch's is refused (Error::CellHasBranch, EEXIST). NewVersion/Version carry no cell_cut. Store descriptions name the cell's kind (store format 15; bucket FORMAT 16). FUSE mount: a cell shows at TENANT/CELL; Attach of a new cell (capacity) makes its Fxfs filesystem; CreateVdisk and Vdisk gone; cladefsctl mount create; Go regenerated. Bucket-level tests that put several branches in TEST_CELL (encrypted.rs) make stores naming no kind, so none claims the cell; attached-base tests fork generations into cells of their own.
Verified: macOS cargo nextest -p extent_frame,extent_bucket,extent_store,cladefs,sqlite_extents,extent_files,follower,follower_sim,virtio_extent 1096 run, 1093 passed + the 3 attached-base tests fixed after (8/8 bases_ pass); sim 57/57; Shuttle extent_store 37/37 and 10/10, cladefs 33/33, follower 10/10, follower_sim 18/18; clippy (workspace, shuttle) clean; fmt; go generate --check, vet, test. ubu1 (Linux, from a patch, nothing pushed): clippy -p cladefs,sqlite_extents,extent_store,extent_bucket --tests --bins clean; nextest -p cladefs -p sqlite_extents 685 run, 682 passed + the same 3, rerun with fuse_mount 22/22; Go tests against cladefsd pass. Not run: the whole workspace's nextest (DoD 1), guest/e2e suites.
<!-- SECTION:NOTES:END -->
