---
id: TASK-519
title: Revoking a tenant can unlink its newly granted socket
status: To Do
assignee: []
created_date: '2026-11-18 12:24'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 909000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of revision 845a6c123, finding 4.

In `crates/cladefs/src/cladefsd/grants.rs:106`, revoke releases the grants lock before unlinking. A grant in between can replace the stale socket and return the same pathname. The earlier revoke then unlinks the new listener. Serialize name removal with creation and check that a fresh connection succeeds after a regrant under this interleaving.

Evidence is from source inspection. Runtime reproduction is outstanding.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A scheduled revoke and regrant of the same ID leaves the successful new grant connectable at the returned pathname.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
