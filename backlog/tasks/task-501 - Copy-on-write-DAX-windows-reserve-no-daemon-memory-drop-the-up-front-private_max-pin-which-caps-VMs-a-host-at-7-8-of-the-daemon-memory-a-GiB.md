---
id: TASK-501
title: >-
  Copy-on-write DAX windows reserve no daemon memory: drop the up-front
  private_max pin, which caps VMs a host at 7/8 of the daemon memory a GiB
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-16T21:23:00Z'
updated_date: '2026-11-17 01:10'
labels:
  - dax
  - perf
dependencies: []
documentation:
  - backlog/docs/doc-13 - Copy-on-write-DAX.md
priority: high
ordinal: 890000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Each writable DAX window pins its whole private_max (1 GiB by default, PRIVATE_MAX_DEFAULT) against its tenant arena as the device activates (CopyOnWrite::reserve, physmem::pin), and Arenas::pin refuses past 7/8 of the daemon memory: a daemon of 8 GiB fits 7 DAX writers, and the 8th VM never boots (Firecracker: "creating the DAX window: ENOMEM"). Measured on GCE at 3934f820, 8 VMs on one base, daemon fixed at 8 GiB: with DAX 7 boot at windows of 2, 4 and 8 GiB; without DAX all 8. Since TASK-164.1 private pages are the VM process own memfd, charged to its cgroup, not daemon memory, so the pin charges the cache budget for memory the daemon never holds; nothing but pin() reads the pinned count, so it neither allocates nor protects memory, it only refuses VMs. Under the governor it charges twice: the pin counts against the allowance, and the filled pages lower MemAvailable, which lowers it again. private_max already bounds each device (stores past it refused, PRIVATE ENOMEM). CopyOnWrite is the only caller of physmem::pin. Supersedes TASK-442 and the first half of TASK-443.3; the idle-private-pages half of TASK-443.3 stays.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Making a copying DAX window pins nothing against any arena and is not refused for memory: a test makes more copying windows than the arena budget would have admitted
- [ ] #2 private_max still bounds each window: stores past it are refused and PRIVATE fails ENOMEM, as the existing tests show
- [ ] #3 physmem has no pin budget left (pin, unpin, pinned_bytes, RESERVED_SHARE and the unpins bookkeeping in relief removed), or what remains has a caller
- [ ] #4 Docs say private pages are the VM process memory, bounded per device by private_max and by the VM process cgroup, not reserved against the tenant: dax.rs, extents.rs, cladefsd/memory.rs, docs/architecture.md, docs/integrator-guide.md (whose quarter-of-the-window default is stale since TASK-227), doc-13, and an OPEN_QUESTIONS.md entry replacing the TASK-164.1 and TASK-227 ones
- [ ] #5 TASK-442 closed as superseded, TASK-443.3 notes that its cap half is gone, and TASK-456.3 AC #2 no longer asks for private pages reserved against the tenant
- [ ] #6 Measured: 8 DAX VMs on one base boot against a daemon fixed at 8 GiB, and against a governed daemon
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. dax.rs: CopyOnWrite keeps only the bound (most); reserve() becomes a constructor that pins nothing (EINVAL for none), Drop goes; module and type docs say the pages are the VM process memory, bounded by private_max and its cgroup.
2. extents.rs create_copying_dax_window: no ENOMEM at creation; its doc.
3. physmem: remove pin/unpin/pinned_bytes, Arenas pin state, RESERVED_SHARE, unpins and the relieved() unpinned check, and their tests (pins_stop_at_their_budget, memory_out pin tests).
4. Tests: replace private_pages_are_reserved_as_windows_are_made with one making more copying windows than the old budget admitted; fix uffd.rs callers.
5. Docs: cladefsd/memory.rs header, docs/architecture.md, docs/integrator-guide.md, doc-13 (via backlog doc update), OPEN_QUESTIONS.md entry.
6. Backlog: TASK-442 superseded, TASK-443.3 note, TASK-456.3 AC #2.
7. cargo nextest on the touched crates locally; workspace on ubu1 under fxjob; density measurement (8 VMs, daemon 8 GiB fixed and governed).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented: CopyOnWrite holds the bound alone (new(), EINVAL for none), pins nothing; create_copying_dax_window no longer ENOMEMs; physmem pin budget removed (pin, unpin, pinned_bytes, RESERVED_SHARE, unpins and relieved()s check, their tests). Test windows_reserve_no_memory_for_their_private_pages: 3 windows each bounded at the whole daemon memory (old code refused the 2nd). Docs: dax.rs, extents.rs, extentd/{memory,client,wire}.rs, virtio_extent config.rs, architecture.md, integrator-guide.md, doc-13, OPEN_QUESTIONS.md. Backlog: TASK-442 archived (superseded), TASK-443.3 noted, TASK-456.3 AC #2 reworded.
Verified on ubu1 under fxjob: clippy gates (workspace -D warnings, shuttle, loom) clean; workspace nextest 2934/2948, the 14 failures environmental (13 FUSE/sudo refused by fxjob no-new-privileges; bucket::lifecycle::restores_connect_to_the_bucket_no_more failed once under load, 20/20 alone); shuttle part 162/162. upstream-changes.py --check not runnable on ubu1 (no Fuchsia checkout); no citation touched. Open: macOS run; density measurement (8 DAX VMs, daemon 8 GiB fixed and governed), which needs a pushed commit.
<!-- SECTION:NOTES:END -->
