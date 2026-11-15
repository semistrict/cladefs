---
id: TASK-411.26
title: >-
  Sandboxes run sealed leaves three tenant-0 cells in the clear in the run's
  bucket (runner sealed fails), not the case's daemons'
status: To Do
assignee: []
created_date: '2026-11-15 11:06'
updated_date: '2026-11-15 14:26'
labels:
  - bug
  - encryption
dependencies: []
parent_task_id: TASK-411
priority: high
ordinal: 821000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-411.23 on ubu1 (2026-11-15, branch t3/sealed-in-place 02302b95, scripts/sandboxes.sh short, test-e2e.sh's run-local LocalKms with KEKs for tenants 0-15, encryption.txt 'sealed'): fxfs-guest-sandboxes passed (writes 1.21/1.22x, framings within budget), but the run-end check over the S3 bucket (runner sealed) failed: 19 cells, 504 objects, 22 in the clear: cells 0/1fd382a3cdc91f6e, 0/6599f1fb1647e3cb, 0/b388ad3de7945ea9 with no KEK, their checkpoints and segments framed in the clear, the records of branches 62f7d328cd000000, -001, -002 without DEKs. The case's own daemons' branches are bff0032b03..., another ID generator: the three clear branches are the run's first, one process's, so likely the runner's in-process stores (the image or rootfs the case builds, cladefs::import::testing) or the run's daemon, made before or without the KMS. TASK-411.15's sandboxes run on 04d5b312 (prebuilt) found 0 in the clear at the run's end, so something between that commit and 83219b55 (main's TASK-400/401/407 merges, the AWS/OpenBao/GCP KMS backends) may have changed it; not bisected. Results kept nowhere (ubu1 scratch removed); reproduce with scripts/sandboxes.sh short on ubu1. Keeps a qualification of an encrypted build failing its sandboxes part.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 scripts/sandboxes.sh short on ubu1 passes sealed with the run-end check finding 0 in the clear
- [ ] #2 The process that made the clear cells is named, and why it made them without a KEK
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Likely a process of the sandboxes run started without FXDAX_KMS (cells made by a process not the case's daemons). TASK-411.28 makes every store-opening process refuse to start without a KMS or master key, which will name it; check this task's ACs there.
<!-- SECTION:NOTES:END -->
