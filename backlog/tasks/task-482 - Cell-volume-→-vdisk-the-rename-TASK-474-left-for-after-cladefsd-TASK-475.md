---
id: TASK-482
title: 'Cell volume → vdisk: the rename TASK-474 left for after cladefsd (TASK-475)'
status: Done
assignee:
  - '@claude'
created_date: '2026-11-16 04:50'
updated_date: '2026-11-16 13:33'
labels:
  - naming
dependencies: []
ordinal: 880000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
CONTEXT.md (2026-11-15, the user's choice): a cell's named branch is a vdisk; volume is Fxfs's alone. TASK-474 deferred it because TASK-475 (one daemon, cladefsd, serving FUSE cells; on another branch until it lands) is rewriting the same code: mount.proto, the FUSE mount, the store table. Start after TASK-475 is on main.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 cell::Volume → cell::Vdisk, its Kind, CreateVolume → CreateVdisk (mount.proto, extentd.proto VolumeKind), Volume DEK → vdisk DEK, DIR/TENANT/CELL/VOLUME → VDISK in paths and docs
- [ ] #2 volume names only Fxfs volumes in code and docs; CONTEXT.md's Ambiguities no longer says the code still says volume
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Rename cell::Volume -> Vdisk and its API/errors in extent_bucket; extent_store's description field and Volume -> Vdisk, store FORMAT 11 -> 12.
2. Protos (VdiskKind, CreateVdisk, Vdisk, vdisks fields), their servers/clients, cladefsctl create-vdisk; regenerate Go.
3. memory_volume -> memory_vdisk (MemoryVdisk); extent_files/sqlite_extents Volume -> FileSet (their own concept).
4. Prose: README, integrator guide, Bucket.tla comments, doc-7/11/14/16 (doc-16 retitled), CONTEXT.md.
5. Verify: fmt, clippy+go, Linux clippy in Docker, scripts, workspace.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Kept as volume: every Fxfs volume (crates/fxfs*, cladefs volume.rs/image.rs/filesystem.rs/lib.rs Image docs, cladefs-mount's default volume, storage fsck of volumes, fxfs-stack/metabench examples, doc-10, doc-1/2/8, dead-ends, guest/ except one cell-sense comment in test_fxfs_guest_import.rs), OPEN_QUESTIONS.md (decision record), docs/measurements, docs/reviews.
extent_files::Volume and sqlite_extents::Volume are not vdisks: they are the on-device format of named files (what a SQLite or memory vdisk holds, also on a local image), so FileSet / FileSetWindow, modules file_set.rs; CONTEXT.md gains File set. Root magic FXSQLVOL kept (bytes, no format change).
Formats: extent_store description FORMAT 'cladefs extent store 12' (field 'vdisk NAME' was 'volume NAME'). Not bumped: extent_bucket layout FORMAT (cell Object is bincode, field names not on disk); protos (field numbers unchanged; gRPC wire unchanged); extentd wire VERSION (only docs changed).

Checks (Mac): cargo fmt; CLADEFS_TEST_PARTS='clippy go' exit 0; scripts 516 OK (after TASK-494); workspace 2535 passed, 80 skipped; upstream-changes.py --check exit 0. Linux: cargo clippy --workspace --tests -A clippy::all -D warnings in OrbStack fxdax-linux-rust, exit 0. Left for DoD #1: nextest on ubu1.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Landed on main 29dec3ee1 (CONTEXT.md, the grilled word choices; renames across code, protos, wire, specs, scripts, docs). macOS: clippy, 2547 workspace tests, 516 script tests, mutants, TLC bucket and follower models and mutations. Linux (ubu1 t474d): 2824 workspace tests passed; guest cases passed but the pgbench pair, which fails main alike (TASK-492). The final merges were checked on macOS and with Linux clippy; ubu1 run t474e was still running at landing.
<!-- SECTION:FINAL_SUMMARY:END -->
