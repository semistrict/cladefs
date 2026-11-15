---
id: TASK-444.2.4
title: >-
  A store resize told as a VM's extent device starts may be lost: on_capacity
  drops what comes before it
status: To Do
assignee: []
created_date: '2026-11-15 13:57'
labels: []
dependencies: []
parent_task_id: TASK-444.2
priority: low
type: bug
ordinal: 864000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen by TASK-444.2's look around TASK-444.2.1/.2 (news arriving before a listener), not reproduced: no oracle has run it, so it is a suspicion, not a finding. virtio_extent's Device reads the store's capacity (store.info()), then sets RemoteExtentStore::on_capacity. A Told::Capacity the client receives between the two finds no listener; with ask false it is dropped (extentd client's receive loop), so the guest keeps the old capacity while the daemon has the new one, until the next resize. on_stats_asked drops likewise, but there the store answers the ask itself, which is the documented behaviour. The same structure as TASK-444.2.2: listening and reading the latest are two steps.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A test (extentd's, with a resize at the window between info and on_capacity, e.g. a failpoint) shows the resize lost, or shows it cannot be
- [ ] #2 If lost: on_capacity returns the latest capacity told as it began (as on_committed does through hearing::Latest), and the device uses it
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
