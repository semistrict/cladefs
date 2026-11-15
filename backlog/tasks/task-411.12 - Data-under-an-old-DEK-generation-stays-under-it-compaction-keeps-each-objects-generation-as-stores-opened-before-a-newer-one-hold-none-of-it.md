---
id: TASK-411.12
title: >-
  Data under an old DEK generation stays under it: compaction keeps each
  object's generation, as stores opened before a newer one hold none of it
status: To Do
assignee: []
created_date: '2026-11-14 23:43'
labels: []
dependencies: []
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
priority: low
type: task
ordinal: 810000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-16 says rotating a DEK starts a generation and old data moves to the new one as compaction and checkpoints rewrite it. It does not (TASK-411.6): checkpoints rewrite metadata only, and the collector seals each moves under the generation its object was sealed under, because a store's ring is built once, as it opens (`keys::ring`), and a store opened before `keys::add_generation` holds none of the newer generation: moves sealed under it would be unreadable to that store (NoKey on an extent it reads every day). So a DEK generation, once used, stays needed for as long as any extent sealed under it lives, and dropping an old generation (after a suspected DEK exposure, say) is impossible short of rewriting the volume. Moving data on needs readers to learn generations added after they opened (rings refreshed from the record, or a generation only used once every holder of the branch reopened) before the collector may seal under the newest.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A pass compacting an object sealed under an older generation seals its moves under the newest the branch's record holds, and every open store of the branch, and its forks' and points', reads them
- [ ] #2 A test adds a generation while a store is open, compacts, and the open store reads the moved extents
- [ ] #3 doc-16 says how old generations empty and when one may be dropped
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
