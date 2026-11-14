---
id: TASK-411.5
title: >-
  Open segments encrypted under the volume DEK (AES-256-XTS per 4 KiB sector);
  their logs authenticated
status: To Do
assignee: []
created_date: '2026-11-14 18:34'
updated_date: '2026-11-14 22:01'
labels: []
dependencies:
  - TASK-411.8
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
type: task
ordinal: 802000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The open segment holds raw guest writes on local disk until it is framed, read back by offset meanwhile. The user chose to encrypt it rather than rely on host disk encryption, so that shredding covers hosts' disks too (doc-16). Every write reaching it is whole 4 KiB sectors since TASK-411.8, so XTS needs no read-modify-write. TASK-411.8 found three more places plaintext reaches local disk or bypasses the store: Write::file() hands callers the open segment's raw file directly (fxdax storage/image.rs write_file, which may retry after a short write at an unaligned offset; the shuttle tests); frame_in_place writes a closing segment's tail into its raw file; and large extents' own raw files (open-extents/) hold guest data in the clear until sealed.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 An open segment file holds no plaintext of written data: a test writes known content and searches the file
- [ ] #2 Reads of sealed extents from the open segment and its framing on close give the written data
- [ ] #3 Recovery after a crash with an open segment gives the same state as before
- [ ] #4 Large extents' raw files (open-extents/) hold no plaintext of written data either
- [ ] #5 No caller writes an open segment's raw file directly: Write::file() is gone or encrypts, and image.rs's write_file retries only whole sectors
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
