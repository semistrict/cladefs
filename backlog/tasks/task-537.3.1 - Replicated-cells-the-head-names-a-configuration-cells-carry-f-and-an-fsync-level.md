---
id: TASK-537.3.1
title: >-
  Replicated cells: the head names a configuration, cells carry f and an fsync
  level
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18T17:02:00Z'
updated_date: '2026-11-18T21:55:00Z'
labels:
  - durability
  - peers
dependencies: []
documentation:
  - >-
    backlog/docs/doc-21 -
    Replicated-commits-a-majority-of-hosts-disks-before-the-bucket.md
parent_task_id: TASK-537.3
priority: high
type: feature
ordinal: 931000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-21 step 1. The bucket head must name the active configuration (config number, the epoch that made it, f, members as address and incarnation) so that takes and replacements are fenced by its CAS; the VM manager must be able to make a cell replicated. Nothing replicates yet: this is the formats and the API.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Head gains replicas (bucket format bumped, the old format refused); a cell with none takes and opens as before
- [x] #2 CreateCell takes f and the fsync level (ordered, local, replicated), kept in the head by the taking (every host reads it there; a taking asking nothing keeps it); StatCell reports them
- [x] #3 Tests: head round trip, format refusal, options through the control API
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
The wish lives in the head, not the store file (decision in OPEN_QUESTIONS.md): a take on another host must know f and the fsync level. PlaceCell needs nothing: CreateCell is the take. Plumbing: CreateCell replicas/fsync -> control::wish (EINVAL past MAX_F, fsync without replicas, Replicated with f 0) -> cells::create -> upload::Options::replication -> head::Taking::replication -> Head::replication. StatCell reports f, fsync and the configuration number from the uploader's taking.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
The head says whether a cell is replicated (bucket format 21 after main's 20; the old refused): its wish (f, fsync level) and active configuration (number, epoch, f, members as address and incarnation). CreateCell's replicas/fsync give it through the taking (upload::Options -> head::Taking), kept by takings asking nothing; StatCell reports it. On main (0b96ed5f1).
<!-- SECTION:FINAL_SUMMARY:END -->
