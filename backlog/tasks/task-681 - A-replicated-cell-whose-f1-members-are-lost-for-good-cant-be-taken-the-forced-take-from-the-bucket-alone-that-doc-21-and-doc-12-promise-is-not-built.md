---
id: TASK-681
title: >-
  A replicated cell whose f+1 members are lost for good can't be taken: the
  forced take from the bucket alone that doc-21 and doc-12 promise is not built
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-20 00:59'
updated_date: '2026-11-20 01:04'
labels:
  - durability
  - peers
dependencies: []
references:
  - >-
    backlog/docs/doc-21 -
    Replicated-commits-a-majority-of-hosts-disks-before-the-bucket.md
priority: medium
type: bug
ordinal: 1062000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found 2026-11-19 comparing with FoundationDB's forced recovery (forceRecovery, KillRegion: recovery with known data loss when too few logs survive). doc-21's failure table (row 'Writer's host and a peer lost') says the take waits for the lost peer and 'the VM manager may then take from the bucket alone, by an explicit force, which loses that tail'. No such force exists: Shared::fence fails the take while fewer than f+1 members besides the writer answer, cladefsctl and the control API have no option to skip it, so a cell whose members' hosts are gone for good stays untakeable for ever.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The control API and cladefsctl can take a replicated cell from the bucket alone, by an explicit force, which logs and reports the commits it may lose
- [ ] #2 A forced take still fences the members it reaches and recovers what they hold
- [ ] #3 A test loses f+1 hosts of a configuration and takes the cell by force; without it, the take is refused
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
