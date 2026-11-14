---
id: TASK-411.13
title: >-
  A revoked tenant's idle cells get a full collector pass every sweep: a pass
  leaving sealed objects uncompacted records no listing digest
status: To Do
assignee: []
created_date: '2026-11-14 23:52'
labels: []
dependencies:
  - TASK-411.6
parent_task_id: TASK-411
priority: low
type: bug
ordinal: 811000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-411.6: a pass over a cell whose KEK the KMS refuses (revoked, destroyed, failing, or no KMS) leaves its sealed objects uncompacted and records no listing digest, so that the next pass goes over the cell whole and compacts once the key is back. While a tenant stays revoked, each sweep lists and reads every one of its idle cells again, where a digest would have let the pass skip them (TASK-395.2).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A cell left uncompacted for want of its KEK, unchanged since, is skipped by later passes until its KEK is unwrappable again or its listing changes: a test counts the requests of a second pass
- [ ] #2 A tenant unrevoked has its cells compacted at the next pass
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
