---
id: TASK-537.2
title: TLA+ spec of replicated commits and their recovery
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18T12:36:00Z'
updated_date: '2026-11-18T12:36:00Z'
labels:
  - durability
  - tla
dependencies:
  - TASK-537.1
parent_task_id: TASK-537
priority: high
type: task
ordinal: 929000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The replication protocol chosen by the research task is model-checked before implementation, as Bucket.tla was for doc-7. It must reproduce known failure modes (a stale writer's acknowledgement, recovery from a minority, truncation before upload) as mutations.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A spec under specs/ models writers, replicas, the bucket's head and replica set, crashes and message loss, and checks that a commit acknowledged Replicated survives every take
- [ ] #2 Each safety rule has a mutation config under mutations/ that TLC shows violating it
- [ ] #3 Configs run in CI next to the bucket specs
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
