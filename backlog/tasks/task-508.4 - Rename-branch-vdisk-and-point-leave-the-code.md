---
id: TASK-508.4
title: The code says the glossary's words
status: To Do
assignee: []
created_date: '2026-11-18 00:11'
updated_date: '2026-11-18 00:24'
labels:
  - bucket
  - host
  - guest
dependencies: []
documentation:
  - doc-20
parent_task_id: TASK-508
ordinal: 901000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Once one type names a cell (TASK-508.3), the identifiers, fields, protos, the extent ID's first half, Fxfs's uses of it, the guest kernel's vext_id and the docs still name the concepts the glossary no longer has. They follow the glossary (CONTEXT.md, doc-20).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Every test suite passes, on macOS and on ubu1
- [ ] #2 Every identifier, proto name and doc names cells, versions and forks as CONTEXT.md does
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
