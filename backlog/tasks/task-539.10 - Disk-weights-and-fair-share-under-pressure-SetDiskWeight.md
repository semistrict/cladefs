---
id: TASK-539.10
title: Disk weights and fair share under pressure (SetDiskWeight)
status: Done
assignee: []
created_date: '2026-11-18 14:17'
updated_date: '2026-11-18 16:49'
labels:
  - extentd
  - disk
dependencies:
  - TASK-539.9
documentation:
  - backlog/docs/doc-22 - Disk-governor-the-hosts-disk-as-a-managed-resource.md
parent_task_id: TASK-539
priority: medium
type: feature
ordinal: 942000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A tenant filling the host disk must not take every other tenant's share first. This mirrors SetMemoryWeight. (doc-22 task T9.)
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 rpc SetDiskWeight is added to cladefsd.proto v1, control.rs and the regenerated Go client. disk::set_weight refuses the default tenant with EINVAL/INVALID_ARGUMENT, and weights persist for later grants.
- [x] #2 Under pressure, govern_once publishes governor::shares over the allotments on the disk. With weights 3:1, the allotment over its share is refused while the other is admitted, and both commit (test).
- [x] #3 Out of pressure, shares are unlimited (test).
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
SetDiskWeight in cladefsd.proto, control.rs, Go client regenerated (go/generate.sh --check clean); allotment weights and shares; DiskFull::Share. Test: cladefsd::disk::tests::weights_share_a_disk_under_pressure. Linux (ubu1, fxjob, 9d5012c..2d67074): clippy --workspace --tests clean; local_cell+bucket_cell 590/593, cladefs 666/669, rest of workspace 1680/1686, every failure one of a1167a56a's regression tests for open bugs (TASK-516..536), failing on the base too. macOS: the same crates' suites, same three pre-existing audit failures.
<!-- SECTION:FINAL_SUMMARY:END -->
