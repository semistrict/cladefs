---
id: TASK-411.21
title: >-
  A tenant's KEK rotated in a real KMS leaves its cells' KEKs wrapped under the
  old version: destroying that version shreds them
status: To Do
assignee: []
created_date: '2026-11-15 04:04'
updated_date: '2026-11-15 14:28'
labels: []
dependencies: []
parent_task_id: TASK-411
priority: medium
type: bug
ordinal: 817000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found building the Google Cloud KMS backend (TASK-411.18). Real KMSes rotate a key by adding a primary version; the old versions still decrypt only while kept, and Cloud KMS, AWS KMS and OpenBao users routinely destroy old versions after rotating. cladefs wraps each cell KEK once, under the tenant KEK's primary version of the day, and never rewraps it: keys::rotate adds a cell KEK generation, but older generations stay in the cell object wrapped as they were (TASK-411.11). So a tenant that rotates and then destroys the old version shreds every cell made before, unknowingly. doc-16 'Real KMSes: Google Cloud KMS', Rotation, warns of it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A cell's wrapped KEKs can be rewrapped under its tenant KEK's newest version by one conditional update of the cell object, without its holder losing access
- [ ] #2 After the rewrap, destroying the tenant KEK's older versions leaves the cell's stores opening and reading (tested with LocalKms rotate/destroy of a version, and once against a real KMS)
- [ ] #3 doc-16 says when and how operators run it after a tenant rotates
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
