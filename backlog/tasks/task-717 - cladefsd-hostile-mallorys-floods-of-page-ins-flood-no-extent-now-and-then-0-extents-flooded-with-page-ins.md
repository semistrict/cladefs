---
id: TASK-717
title: >-
  cladefsd hostile: mallory's floods of page-ins flood no extent now and then
  ('0 extents flooded with page-ins')
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-21 00:29'
updated_date: '2026-11-21 05:33'
labels:
  - bug
  - cladefsd
  - flaky
dependencies: []
priority: low
ordinal: 1112000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen on ubu1 2026-11-20 at b1407bac5 (before TASK-690), cladefsd::tests::hostile::a_compromised_vm_process_harms_no_tenant_beside_it stressed alone, 1 of 40 runs: hostile.rs's assert 'flooded > 0' failed, '0 extents flooded with page-ins'. Mallory's random cases include floods of page-ins of the given cell's 1 MiB extents (TASK-149.34); a run may draw too few, or they may all be refused, leaving the check of the flood path unexercised. Find whether the draw or a refusal leaves it at 0, then make the test cover the flood path every run.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The cause of a run flooding no extent is named
- [ ] #2 The test floods extents in 100 runs of 100
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
