---
id: TASK-537.3.5
title: Member replacement, degraded mode and metrics
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18T17:02:00Z'
updated_date: '2026-11-19T01:05:00Z'
labels:
  - durability
  - peers
dependencies:
  - TASK-537.3.4
documentation:
  - >-
    backlog/docs/doc-21 -
    Replicated-commits-a-majority-of-hosts-disks-before-the-bucket.md
parent_task_id: TASK-537.3
priority: high
type: feature
ordinal: 935000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-21 step 5. A writer must replace lost members under the joint rule, drop to f = 0 only once the bucket holds what it told, and say loudly when degraded.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Replacement installs the tail, counts both configurations while its swap is in flight, then the new one
- [ ] #2 Drop to f = 0 only once Uploaded covers every Replicated commit; no Replicated acks during it
- [ ] #3 Degraded state in stats and metrics, eager uploads while degraded
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Replicator: per-member mark queues; an active and a pending configuration, a commit replicated only on quorums of both while the pending's swap is in flight (joint rule); replacing/installed/swapped/abandon; f = 0 holds no quorum.
2. Uploader keeper (Shared::keep): replace a member refusing or down 1 s; drop to f = 0 once uploads cover what was told, when degraded with no replacement; back to the head's f as peers answer; a take asking for another f reconciled the same way; eager uploads while degraded.
3. ReplicationStatus in UploadStatus; StatCell degraded/members_up/active_replicas; /metrics cladefsd_replication_degraded and _members_up.
4. RESOURCE_EXHAUSTED refuses (replacement avoids the member).
5. Spec: Abandon action (rule abandonUnissued), model ReplicationAbandons, mutation AbandonIssued.
6. Tests: replicator unit tests; local_cell integration tests for replacement, the f = 0 drop and return, and the f = 0-wish take.
<!-- SECTION:PLAN:END -->
