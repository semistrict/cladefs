---
id: TASK-658.2
title: >-
  Explainer film: replicated commits, durable on a majority of hosts before the
  bucket
status: Done
assignee: []
created_date: '2026-11-19 15:28'
updated_date: '2026-11-19 16:13'
labels:
  - explainer
dependencies: []
documentation:
  - >-
    backlog/docs/doc-21 -
    Replicated-commits-a-majority-of-hosts-disks-before-the-bucket.md
  - backlog/docs/doc-12 - The-fsync-durability-contract.md
parent_task_id: TASK-658
type: docs
ordinal: 1040000
---

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A 45-75 s film shows an fsync reaching Replicated on a majority of hosts without the bucket, and a lost host losing none of it
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
explainer/replication/, 70 s: one commit of cell 9 on 2 of 3 hosts' disks, sync vs fsync levels as the code has them, the take by host D with fencing and the tail installed before the head swaps, degraded mode. Verified: check 0 errors, independent fact check (fixed the fsync-waits-for-the-bucket premise and 4 more), render 70 s at -16.1 LUFS.
<!-- SECTION:FINAL_SUMMARY:END -->
