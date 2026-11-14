---
id: TASK-411.2
title: >-
  extent_frame FORMAT 2: data parts sealed with AES-256-GCM after zstd, under a
  subkey per object from a random salt; footer and table authenticated
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-14 18:33'
updated_date: '2026-11-14 19:07'
labels: []
dependencies:
  - TASK-411.1
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
type: task
ordinal: 799000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The bucket, the WAL and the part cache all hold framed objects, so encrypting parts at framing covers all three (doc-16, What is encrypted). The salt per object is needed because a fenced writer can hold different bytes for a segment key another epoch landed (doc-14), and nonces from the key alone would repeat.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A framed object with a key has no plaintext part bytes; reading it back with the key gives the data, with the wrong key or a flipped byte an error, never wrong data
- [ ] #2 A part moved to another object or index fails authentication (associated data covers object key, index, encoding, length)
- [ ] #3 Framing throughput with encryption is measured against without, on ubu1 via fxjob --bench, and the numbers are in the task
- [ ] #4 A FORMAT 2 reader refuses a FORMAT 1 object by naming its format, not as corrupt (doc-11 rule 1); no migration (the user's decision)
- [ ] #5 An object of a cell with no key is written and read in the clear, its trailer saying so; each encryption test has a plaintext twin
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
