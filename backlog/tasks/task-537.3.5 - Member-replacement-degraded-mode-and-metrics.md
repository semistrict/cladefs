---
id: TASK-537.3.5
title: Member replacement, degraded mode and metrics
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18T17:02:00Z'
updated_date: '2026-11-19T02:44:00Z'
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
- [x] #1 Replacement installs the tail, counts both configurations while its swap is in flight, then the new one
- [x] #2 Drop to f = 0 only once Uploaded covers every Replicated commit; no Replicated acks during it
- [x] #3 Degraded state in stats and metrics, eager uploads while degraded
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Built (62a660a9c, d4109ff3a, 6800f5cd0, on main): Replicator with an active and a pending configuration (per-member marks, joint quorum, replacing/installed/swapped/abandon); the uploader's keeper (Shared::keep/replace); f = 0 drop and return; a take's wish f reconciled; ReplicationStatus, StatCell degraded/members_up/active_replicas, /metrics cladefsd_replication_degraded and _members_up; RESOURCE_EXHAUSTED refuses.
Spec: Abandon action, rule abandonUnissued, model ReplicationAbandons (6,223,898 states, 325 s), mutation AbandonIssued (caught in 3 s); every non-deep replication model and mutation re-checked after Lands took the CAS in flight (all pass, the longest 206 s). A first ReplicationAbandons with MaxReplaces = 2 ran past 15 min and was cut to 1.
Reviews: Codex (data loss: a replacement's drop with its own key removed the active configuration's logs on a shared host, fixed in Replicas::drop_logs; a take of an f = 0 cell with no peers failed, now serves degraded; reaped retired state; test races) and the verifying reviewer (trimmed by the head's segment end, so a member's partial copy of a named segment is no hole, a 537.3.4 bug; installed of f >= 1 never on the bucket alone; fresh configuration numbers per attempt, refusers avoided; a drop answered brings a member up). All fixed, each with a test.
Verified under fxjob on ubu1: local_cell 358 tests (all pass after the last fix; replicated tests run 4x stable), cladefs peers/control/replica 23 (the_control_socket_refuses_other_users needs sudo, excluded), workspace check, clippy with test.sh's flags clean.
Left for 537.3.6: an idle cell notices lost members only on its next commit (no probe); StatCell/metrics shown on real daemons; retired Member entries (a few words each) grow with replacements over a writer's life.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Replicated cells now replace lost or refusing members under the joint rule, drop to f = 0 once the bucket holds what was told (and come back as peers answer), and report degraded state in StatCell and /metrics, uploading at once while degraded. The spec gained an Abandon action checked by a model and a mutation. Two reviews found a data-loss bug in member drops and a take-blocking bug in trimming, both fixed with tests. Follow-ups noted for 537.3.6.
<!-- SECTION:FINAL_SUMMARY:END -->
