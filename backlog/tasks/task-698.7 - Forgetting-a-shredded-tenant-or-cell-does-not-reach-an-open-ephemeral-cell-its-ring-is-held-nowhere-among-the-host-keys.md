---
id: TASK-698.7
title: >-
  Forgetting a shredded tenant or cell does not reach an open ephemeral cell:
  its ring is held nowhere among the host keys
status: To Do
assignee: []
created_date: '2026-11-20 18:09'
labels: []
dependencies: []
parent_task_id: TASK-698
priority: high
ordinal: 1097000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
upload::open_ephemeral builds the ephemeral cell ring (keys::ring) and hands it to the store, but holds it in no RingHold of the host keys::Host, as an Uploader does for bucket cells (Shared.held). keys::Host::forget, which ForgetTenantKeys/ShredCell reach, therefore cannot drop the keys of an open ephemeral cell, and its store keeps reading and writing under a shredded tenant keys until closed (doc-16, TASK-637).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 An open ephemeral cell holds its ring among the host keys, and forgetting its tenant or cell halts it as it halts a bucket cell store
- [ ] #2 A test shreds the tenant of an open ephemeral cell and sees its next request refused
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
