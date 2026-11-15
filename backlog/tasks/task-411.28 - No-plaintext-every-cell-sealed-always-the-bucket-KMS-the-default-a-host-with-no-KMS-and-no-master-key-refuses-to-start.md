---
id: TASK-411.28
title: >-
  No plaintext: every cell sealed, always; the bucket KMS the default; a host
  with no KMS and no master key refuses to start
status: To Do
assignee: []
created_date: '2026-11-15 11:09'
labels: []
dependencies:
  - TASK-411.27
  - TASK-411.23
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
priority: high
type: task
ordinal: 824000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The user's decision (2026-11-15): remove plaintext entirely. Every cell is sealed; there is no clear mode, no opt-out. With the bucket KMS (TASK-411.26) as the default whenever FXDAX_KMS is unset, and every process (extentd, fxdaxctl, FUSE mounts, fxdax-gc, tools, tests) refusing to start without a KMS or a master key, the clear paths go: Seal::Clear objects, clear raw files and in-place framing in the clear (TASK-411.23 makes sealed in-place framing the only kind), the clear/sealed test twins, TASK-411.10's kekless cache, TASK-411.19's 'no such key means clear' choice and its marker if the bucket KMS no longer needs it, TASK-411.22 and TASK-411.24 (moot once no cell is ever clear). A deep refactor: prefer deleting code to keeping branches; one code path.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 No code path writes or reads an object, raw file, log or record in the clear: Seal::Clear and the clear branches are gone from extent_frame, extent_store, extent_remote, sqlite_extents and fxdax; a frame in the clear is refused as corrupt
- [ ] #2 Every process that opens stores refuses to start, naming FXDAX_KMS and FXDAX_MASTER_KEY, when neither a KMS nor a master key is configured; with only a master key it uses the bucket KMS
- [ ] #3 A KMS answering 'no such key' refuses the cell as destroyed, for every backend; the kekless cache and the clear-cell choice are gone; TASK-411.22 and TASK-411.24 closed as moot
- [ ] #4 Every test runs sealed (a test master key or LocalKms by default in test helpers); the clear twins are removed, the sealed variants kept; e2e unchanged (already sealed)
- [ ] #5 doc-16, doc-7, doc-14, README and the operator docs describe one sealed mode; the macOS and Linux suites and the encrypted e2e suite pass
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
