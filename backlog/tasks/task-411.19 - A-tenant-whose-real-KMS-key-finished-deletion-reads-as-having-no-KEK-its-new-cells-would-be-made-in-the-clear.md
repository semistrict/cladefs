---
id: TASK-411.19
title: >-
  A tenant whose real-KMS key finished deletion reads as having no KEK: its new
  cells would be made in the clear
status: To Do
assignee: []
created_date: '2026-11-15 03:58'
updated_date: '2026-11-15 04:22'
labels: []
dependencies: []
parent_task_id: TASK-411
priority: medium
type: bug
ordinal: 817000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found in TASK-411.17 (AWS KMS backend). A real KMS forgets a deleted key: AWS KMS answers NotFoundException for a key whose deletion waiting period is over, exactly as for a key never made (an alias is deleted with its key), and other KMSes may do the same (OpenBao Transit's deleted key is a 404). The backends map that to kms::Error::NoSuchTenant as TASK-411.17's AC says, and doc-16 makes NoSuchTenant mean 'make the cell in the clear' (extent_remote keys::Kms::new_kek, KEKLESS_FOR). So a shredded tenant, once AWS has deleted its key, gets its next new cells written unencrypted on any host that still serves it, where LocalKms keeps a 'destroyed' mark for exactly this. Unwraps are not affected (they fail, as they should). While the key is pending deletion it reads Destroyed, which is safe; today only the operator's procedure (forget-tenant-keys, stop serving the tenant) covers the rest.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A tenant that had a KEK (it has sealed cells, or the host or bucket recorded one) is never given a cell in the clear after its KMS answers that its key is not found: a test with a KMS answering NotFound for a tenant with sealed cells shows the cell's making refused, naming the tenant
- [ ] #2 doc-16 says how a tenant's KEK forgotten by the KMS is told from a tenant that never had one
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
TASK-411.16 found the same with OpenBao: a deleted Transit key is forgotten, so wrap says NoSuchTenant and new cells of a shredded tenant would be made in the clear (OpenBao has no scheduled-deletion state). GCP Cloud KMS likely similar once a key's versions are all destroyed. The fix belongs above the backends, in how a cell chooses encryption (doc-16).
<!-- SECTION:NOTES:END -->
