---
id: TASK-456.3
title: >-
  Guest memory restored as a copy-on-write window over a memory volume, its
  pages shared between VMs
status: To Do
assignee: []
created_date: '2026-11-15 13:33'
updated_date: '2026-11-17 01:09'
labels:
  - memory
  - firecracker
  - dax
dependencies:
  - TASK-456.2
parent_task_id: TASK-456
priority: high
ordinal: 864000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Firecracker's restore today fills guest memory from a file (MAP_PRIVATE) or through an outside UFFD handler (UFFDIO_COPY): each VM a copy of every page it touches. The DAX window's second stage (dax/uffd.rs, run in the VM process, UffdSecondStage::in_process) already maps sealed extents' pages from cladefsd's memory read-only and gives a VM a private copy of a page it stores to (doc-13). Guest memory restored from a memory volume should be served the same way, so VMs restored from one snapshot share every page none of them wrote. Two limits of the window don't fit guest memory: private pages capped at dax_private_max and a VM past it stopped (exit 159), where a guest may write all its memory; and one host mapping per run of pages, of which Linux allows vm.max_map_count (65,530 by default), against which the stage keeps a budget (TASK-12): a 64 GiB guest over scattered extents passes it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Two VMs restored from one snapshot on one host share the pages neither wrote: the host holds one copy of each, shown by a guest test reading the pages in both and counting cladefsd's pages and the VMs' private pages
- [ ] #2 A guest may write all its memory: private pages bounded by the guest's memory, charged to the VM process as its guest memory is (nothing reserved against the daemon or the tenant, TASK-501), not by dax_private_max, shown by a guest test writing every page
- [ ] #3 A 64 GiB guest restores and runs within vm.max_map_count, shown by a test
- [ ] #4 Pages the guest gives back through the balloon's free page reporting drop their private copies, shown by a test
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
