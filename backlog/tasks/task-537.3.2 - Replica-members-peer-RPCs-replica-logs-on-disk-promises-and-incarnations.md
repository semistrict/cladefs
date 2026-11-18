---
id: TASK-537.3.2
title: 'Replica members: peer RPCs, replica logs on disk, promises and incarnations'
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18T17:02:00Z'
updated_date: '2026-11-18T21:55:00Z'
labels:
  - durability
  - peers
dependencies: []
documentation:
  - >-
    backlog/docs/doc-21 -
    Replicated-commits-a-majority-of-hosts-disks-before-the-bucket.md
parent_task_id: TASK-537.3
priority: high
type: feature
ordinal: 932000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-21 step 2. A daemon must hold other hosts cells logs as a member of their configurations: append, fence, read and drop, refusing older epochs and other incarnations, bounded by the lag bound per cell.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Peer service gains Open/Append (stream), Fence, ReadLog and Drop; Append has an endpoint with no per-request deadline
- [x] #2 Replica directory with an incarnation made at creation; logs written O_DIRECT through local_cell pages; a promise per cell synced before Fence answers
- [x] #3 Appends below the promise, or naming another incarnation, refused; logs of configurations the head no longer names swept
- [x] #4 Unit tests with fake links: append and read back, fence then refused append, wrong incarnation, sweep
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Reviews (2026-11-18): verifying-reviewer found 7, Codex 4 (3 overlapping, plus empty appends). All fixed in the review commit with tests: framed logs (crash padding), sweeps only of gone/deleted heads whole and of configured heads before their configuration, bound per configuration, logs held from their making, directories with only lost+found or incarnation.new opened, gaps answered with the end, 64 MiB messages, record.rs format test. Blind spots: MemDir's crash model never pads files (the padding test writes zeros itself); the sweep tests used a fixed head (now unconfigured and unreplicated heads too); the bound test used one configuration. Red: reviewers' reproducers ran red for the padding, bound, stuck log, refused directory and format test; sweep race and message limit traced. Also found: an Append deadlock (answered only once the first append came), now bounded at 30 s in tests. Structure: the member's state is one Log per (configuration, log) under the cell lock; no class-level problem seen beyond these. Full ubu1 suite: needs sudo (OPEN_QUESTIONS.md).
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Daemons with CLADEFSD_REPLICAS hold other hosts' replicated cells' logs: incarnation, promises synced before fences answer, framed logs per configuration (crash padding never counted), bounds per configuration, sweeps of what heads name no more; peer RPCs Incarnation, Append (stream), Fence, ReadLog, DropLogs; member client without request deadline. Two reviews' 11 findings fixed with tests. On main (49883947b..d71b73232).
<!-- SECTION:FINAL_SUMMARY:END -->
