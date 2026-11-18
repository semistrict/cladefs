---
id: TASK-636
title: Every request a store may not serve is refused in one place
status: To Do
assignee: []
created_date: '2026-11-18 17:07'
labels: []
dependencies: []
priority: high
type: enhancement
ordinal: 1023000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The 2026-11-18 audit (TASK-540..631) found the same kind of bug five times: a refusal applied on one path and missing from the paths beside it, because each request type checks for itself whether the store may still serve it.

- TASK-550: a fenced uploader is checked for Commit::Local (`uploading()`), not for Commit::Ordered, so a fenced writer acknowledges guest fsyncs that are lost.
- TASK-582: the lag bound is applied to Ordered commits, not Local ones.
- TASK-611: a halted store refuses writes but still serves ResizeCell, DropVersions and ShareVersion.
- TASK-612: making a version consults neither the uploader nor the halt.
- TASK-576: Storage::take checks the caller's fencing token only on one of its paths.

Each fix one at a time leaves the next request type to repeat the bug. The proposal is one owner of a store's standing (open, fenced, halted, closed, past its lag bound) that every request passes through before it acts, as a type a request can't act without, so that a new request type can't skip it.

AGENTS.md: a pattern of bugs pointing at the structure goes to the user before restructuring; the user agreed to file this (2026-11-18).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A store that is fenced, halted, shredded or closed refuses every request that changes the cell (writes, frees, commits at every level, resize, version make, drop and share, fork from it) with the errno doc-4 gives, and a request type added later cannot reach the store without passing the same check.
- [ ] #2 The lag bound holds for every commit level, Local included.
- [ ] #3 A table-driven test sends every request kind to a store in each refused state and checks each answer, and goes red if any one request kind skips the check (TASK-550, TASK-582, TASK-611, TASK-612 and TASK-576 are its cases).
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
