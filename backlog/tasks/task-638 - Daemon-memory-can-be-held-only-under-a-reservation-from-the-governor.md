---
id: TASK-638
title: Daemon memory can be held only under a reservation from the governor
status: To Do
assignee: []
created_date: '2026-11-18 17:08'
labels: []
dependencies: []
priority: medium
type: enhancement
ordinal: 1025000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The 2026-11-18 audit (TASK-540..631) found about fifteen places where the extent daemon holds memory outside its one budget (AGENTS.md: one memory budget, the governor's) or lets a table grow without bound. Each holder must remember to count itself, and many don't.

- Driven by a VM process past admission: TASK-546 (MAPPINGS replies), TASK-547 (transfer memfds), TASK-548 (in-flight budget leaked when a connection ends mid-payload), TASK-581 (ACK and STAGE_COUNTS payloads), TASK-588 (PIN of any extent ID), TASK-614 (decoded FREE_MANY and MAP_MANY).
- Outside the governor: TASK-603 (each store's unpacked parts), TASK-619 (OPENDIR copies of whole directories), TASK-552 (a mapped file set's heap copies), TASK-596 (persistent-layer chunks in the guest).
- Growing for the daemon's life: TASK-571 and TASK-579 (KEK caches), TASK-610 (VERSIONS_MADE), TASK-602 (an extent's window list), TASK-570 (MAX_VERSIONS never enforced).

The proposal: substantial daemon memory comes only through a reservation from the governor (or a per-tenant budget under it), released when its holder drops, so that holding without counting does not compile and a dropped connection or holder releases by construction. Tables keyed by cells, versions or extents get a bound or an owner whose end removes their entries.

AGENTS.md: a pattern of bugs pointing at the structure goes to the user before restructuring; the user agreed to file this (2026-11-18).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Every allocation of the daemon that a VM process, a tenant or a cell's size can make grow is counted by the governor or a tenant's budget for as long as it is held, and is released when its holder (connection, store, request) goes, including on errors and disconnects.
- [ ] #2 No table of the daemon grows with the number of cells, versions or extents ever seen: each is bounded or emptied by its owner's end.
- [ ] #3 A hostile-client test drives each listed path (TASK-546, -547, -548, -581, -588, -614) and checks that the tenant's held memory stays within its budget and returns to zero when its connections end; a soak checks the listed tables stay flat across cell create and delete cycles.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
