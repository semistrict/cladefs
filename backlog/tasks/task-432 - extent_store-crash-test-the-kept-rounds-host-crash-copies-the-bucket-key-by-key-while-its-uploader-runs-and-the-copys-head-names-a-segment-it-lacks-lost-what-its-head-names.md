---
id: TASK-432
title: >-
  extent_store crash test: the kept rounds' host crash copies the bucket key by
  key while its uploader runs, and the copy's head names a segment it lacks
  (lost: what its head names)
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 09:51'
updated_date: '2026-11-15 10:50'
labels:
  - bug
  - testing
  - flaky
dependencies: []
priority: high
ordinal: 823000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/extent_store/tests/crash/kept.rs Host::crash copies the in-memory bucket by listing it, then reading each object, while the crashed host's uploader still uploads and swaps the head. Keys sort branches/ < checkpoints/ < extents/ < heads/ < segments/: a segment (or checkpoint) PUT after the listing and a head swap before the head is read leave a copy whose head names an object it lacks; the post-crash collector pass then fails 'lost: branch 1/1/3: what its head names'. Benign before TASK-407 (segments dense, no head); since, about 1 in 15 runs of recovers_from_its_bucket_with_and_without_its_disk on the Mac (round 85, round 9). Confirmed by a diagnostic: the copy's head {end: 9, checkpoint: (7, 1)} names segments/.../0000000000000001.0000000000000008, not copied, present in the original. A dead host's writes are those landed at one instant, which the copy must be.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Host::crash's copy is one instant's bucket (object_store's InMemory::fork, under one lock)
- [x] #2 recovers_from_its_bucket_with_and_without_its_disk passes in 30 consecutive runs on the Mac with no 'lost'
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Host::crash: let copy = Arc::new(before.fork()) instead of listing and copying key by key.
2. Run the kept test 30 times on the Mac (with the TASK-431 workload change), none failing.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Fixed: Host::crash forks the InMemory store (object_store 0.14.2's InMemory::fork clones its storage under one read lock), the bucket as it was at one instant, instead of listing and copying key by key. Evidence: before, 1 of 16 runs and 1 of 8 more failed 'lost' (round 85; round 9, the diagnostic showing segment 8 named by the copy's head, absent from the copy, present in the original); after, 33 consecutive runs of recovers_from_its_bucket_with_and_without_its_disk on the Mac with none (26 alone, 4 with every core busy, 3 in full crash-binary runs).
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
kept.rs's Host::crash copies the bucket at one instant (InMemory::fork) rather than key by key while the dead host's uploader still swapped the head: no copy whose head names a segment it lacks. 33 consecutive runs, no 'lost'.
<!-- SECTION:FINAL_SUMMARY:END -->
