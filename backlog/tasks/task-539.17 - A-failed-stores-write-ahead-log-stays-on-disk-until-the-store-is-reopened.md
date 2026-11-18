---
id: TASK-539.17
title: A failed store's write-ahead log stays on disk until the store is reopened
status: To Do
assignee: []
created_date: '2026-11-18 17:01'
labels:
  - extentd
  - disk
dependencies: []
documentation:
  - backlog/docs/doc-22 - Disk-governor-the-hosts-disk-as-a-managed-resource.md
parent_task_id: TASK-539
priority: low
type: task
ordinal: 950000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-539.12 (doc-22, hardening): a store that failed keeps its raw files, logs and write-ahead log on the host's disk, counted in its footprint and against its tenant, until it is reopened, which recovers it and lets go of what the bucket has. Nothing reopens a failed store on its own; a host's disk can keep a failed store's backlog indefinitely. Decide whether the daemon reopens failed stores (or says so in its metrics) so that what they hold leaves.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A failed store's held bytes are visible in metrics, and either reopened by the daemon or documented as the operator's to clear
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
