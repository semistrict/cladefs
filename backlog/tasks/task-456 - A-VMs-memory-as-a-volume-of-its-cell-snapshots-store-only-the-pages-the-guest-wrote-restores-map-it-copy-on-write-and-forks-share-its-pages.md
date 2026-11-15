---
id: TASK-456
title: >-
  A VM's memory as a volume of its cell: snapshots store only the pages the
  guest wrote, restores map it copy on write, and forks share its pages
status: To Do
assignee: []
created_date: '2026-11-15 13:32'
labels:
  - memory
  - snapshot
dependencies: []
priority: high
ordinal: 861000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The user (2026-11-15) wants a VM snapshot's memory, today a file on a GCS FUSE mount, kept as a volume of the same cell as the VM's disk, so a cell holds everything of one VM, and wants forked VMs to share memory pages as DAX shares sealed extents' pages: no page duplicated in host memory but those a VM writes. Decided with the user that day, after TASK-435 showed the FUSE path works and after reading E2B (e2b-dev/infra), which never lets Firecracker dump memory and stores only written non-zero pages: (1) a raw memory volume kind beside fxfs and sqlite, a sparse image of pages over extents, holes for zero pages, no filesystem; (2) the guest's memory served inside Firecracker, as the DAX window's copy-on-write second stage serves its window (dax/uffd.rs, doc-13), not by an outside handler, whose UFFDIO_COPY gives every VM its own copy; (3) a snapshot writes the private pages into a fork of the parent memory volume, zero ones as holes, in one commit, instead of Firecracker's memory file (whose full snapshot writes all of RAM, zeros too: vstate/memory.rs dump). Notes: ~/docs/inbox/2026-11-15-e2b-memory-snapshots.md.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The subtasks are done
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
