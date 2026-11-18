---
id: TASK-637
title: Forget reaches every holder of keys and plaintext through one registry
status: To Do
assignee: []
created_date: '2026-11-18 17:07'
labels: []
dependencies: []
priority: high
type: enhancement
ordinal: 1024000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The 2026-11-18 audit (TASK-540..631) found eleven places where a shred or Forget misses something that holds a tenant's keys or decrypted data. Every cache holds them on its own terms, and Forget has to know about each one by hand.

- Keys: TASK-571 and TASK-579 (the KEK caches keep every cell's unwrapped KEK, deleted cells included), TASK-578 (a KMS read in flight at a shred brings the KEK back), TASK-577 (a failed fork returns a cell KEK already in a head to the pool), TASK-544 (a refused cross-tenant fork leaves the parent's DEKs in the bucket), TASK-545 (a shredded cell's versions still fork), TASK-606 (a passing KMS refusal keeps refusing forks for good).
- Plaintext: TASK-601 (extent cache pages of closed stores), TASK-587 (follower-mapped extents taken out of the extent cache), TASK-630 (page-ins in flight at a shred), TASK-562 (the daemon's FUSE mount's stores are never halted).

The proposal: anything that holds a tenant's or cell's keys or decrypted pages can hold them only by registering under that tenant and cell, and Forget walks the registry, halting or dropping each holder and refusing new registrations for what was forgotten. A holder that does not register can't get the keys.

AGENTS.md: a pattern of bugs pointing at the structure goes to the user before restructuring; the user agreed to file this (2026-11-18).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 After ShredCell or ForgetTenantKeys returns, no memory of the daemon holds that cell's or tenant's unwrapped KEKs or DEKs, nor any decrypted page of its extents, whatever was open, closed, in flight or mapped by a follower when it ran, and no later request brings them back.
- [ ] #2 A holder of keys or decrypted pages cannot obtain them without being reachable by Forget, enforced by the types rather than by convention.
- [ ] #3 A test shreds while each kind of holder is live (open store, closed store with cached pages, follower mapping, page-in and KMS read in flight, FUSE mount, kept version store) and checks every holder is empty and every later read or fork of the cell fails; it goes red if any one holder is left out of Forget.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
