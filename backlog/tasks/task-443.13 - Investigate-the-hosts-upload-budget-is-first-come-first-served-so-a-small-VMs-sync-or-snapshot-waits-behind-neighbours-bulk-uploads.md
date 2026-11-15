---
id: TASK-443.13
title: >-
  Investigate: the host's upload budget is first come, first served, so a small
  VM's sync or snapshot waits behind neighbours' bulk uploads
status: To Do
assignee: []
created_date: '2026-11-15 12:25'
labels:
  - perf
  - review-2026-11-15
dependencies: []
documentation:
  - docs/reviews/2026-11-15-sandbox-performance.md
parent_task_id: TASK-443
priority: medium
ordinal: 847000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/extent_remote/src/fetch.rs Budget::admit admits only the queue's head, and every upload enters at Priority::Demand (writer.rs) in one host budget of 64 requests and 512 MiB; one catching-up store keeps 16 segments in flight. A 4 KiB segment waits for every byte queued ahead: ~12 s behind 1.5 GiB at 120 MB/s. The disk analogue is TASK-322. Budget has no queue-depth or wait-time counter. Found by reading the code, not measured (docs/reviews/2026-11-15-sandbox-performance.md).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Two VMs under one daemon, one writing 4 GiB sequentially, one looping sync on a 4 KiB file, the bucket rate-limited: the second's commit.upload_wait times are recorded
- [ ] #2 The notes give the measured cost and the workload size at which a customer would feel it; a fix, if needed, is filed as its own task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
