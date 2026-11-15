---
id: TASK-411.24
title: >-
  A host with no KMS makes a sealed tenant's new cells in the clear, reading no
  sealed marker
status: To Do
assignee: []
created_date: '2026-11-15 09:42'
updated_date: '2026-11-15 14:26'
labels: []
dependencies: []
parent_task_id: TASK-411
priority: high
type: bug
ordinal: 819000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found in TASK-411.19. A tenant whose first sealed cell was made writes `tenants/<tenant>/sealed`, and a host whose KMS says the tenant has no KEK refuses its new cells as destroyed (extent_remote `keys::new_cell_kek`). But a host with no KMS at all (`CLADEFS_KMS` unset, `keys::Host::kms` None) returns no KEK before reading anything: a store opening of a new volume or a new cell of a sealed tenant on such a host makes the cell in the clear, as before TASK-411.19. Its existing sealed cells refuse to open there ("this host has no KMS"), so only new cells are affected; an operator leaving `CLADEFS_KMS` off one host of a fleet would get them. Reading the marker on hosts with no KMS costs a HEAD per opening of a store whose record keeps no DEK unless cached per host (there is no `keys::Kms` to cache in), which plaintext-only deployments would pay.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A host with no KMS making a new cell of a tenant whose sealed marker is there refuses it, naming the tenant and saying the host has no KMS; a test shows it
- [ ] #2 Plaintext-only hosts (no KMS) pay a bounded number of bucket requests for the check, counted by a test
- [ ] #3 doc-16 says what a host with no KMS does with a sealed tenant
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
