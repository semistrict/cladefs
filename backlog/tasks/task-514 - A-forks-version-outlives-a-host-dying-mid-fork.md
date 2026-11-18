---
id: TASK-514
title: A fork's version outlives a host dying mid-fork
status: To Do
assignee: []
created_date: '2026-11-18 06:55'
labels: []
dependencies: []
ordinal: 904000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Forks are made from versions only (PR #1): host_cell::fork_through commits a version, forks it, then drops it. A host that dies between the version's commit and its drop leaves the version in its cell's head for good, keeping every extent it names from the collector, until an operator drops it (fork_faults::forks_orphaned_by_a_lost_host_keep_what_they_inherited_until_deleted drops it by hand). Fix: mark fork versions so the cell's next writer, or the collector, drops those no fork is using; or name the version in the child's Forking record and drop it when the child's fork resumes or is deleted.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A version made for a fork goes once the fork is made, failed or abandoned by a dead host, with no operator step
- [ ] #2 The orphan test drops no version by hand
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
