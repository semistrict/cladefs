---
id: TASK-639
title: Multi-step operations undo their finished steps when a later step fails
status: To Do
assignee: []
created_date: '2026-11-18 17:08'
labels: []
dependencies: []
priority: medium
type: enhancement
ordinal: 1026000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The 2026-11-18 audit (TASK-540..631) found about ten operations that, failing partway, leave what their earlier steps did in place, often durably:

- TASK-544: a refused cross-tenant fork has already written the parent's DEKs into the child's head.
- TASK-577: a fork failing after its child's head is made returns that head's cell KEK to the pool.
- TASK-575: a VM snapshot failing after one extent device made its version leaks that version.
- TASK-597: new_volume_over_base commits the volume before checking its base layers.
- TASK-553: a failed remap gives a file's region back while SQLite holds pages from it.
- TASK-586: a failed worker start leaves the extent device activated with no workers.
- TASK-592: a seal-refused reserved writeback pays twice for its blocks.
- TASK-609: a fork that made its child reports failure when dropping its scratch version fails.
- TASK-623: Fxfs's close returns early and never closes the device.
- TASK-540: FileSet::close frees what the last root still names (a teardown ordering error of the same kind).

Failure injection rarely lands between two steps of one operation, so tests stay green. The proposal: each such operation is written as steps whose completion hands back an undo (a guard dropped on success, run on failure), with the irreversible step last, and a failpoint between every pair of steps in the tests.

AGENTS.md: a pattern of bugs pointing at the structure goes to the user before restructuring; the user agreed to file this (2026-11-18).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 For each listed operation, a failure at any step leaves the cell, bucket, key pool, region pool and device as they were before the operation began, or in a state the next opening recovers to without error, and the operation reports the failure truthfully.
- [ ] #2 Tests inject a failure between every pair of steps of each listed operation and check that state; each goes red with the undo removed.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
