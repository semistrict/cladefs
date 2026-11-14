---
id: TASK-411.10
title: >-
  Plaintext tenants on a host with a KMS cost one KMS wrap call per store
  opening: the KMS's no-such-tenant answer is not cached
status: To Do
assignee: []
created_date: '2026-11-14 23:22'
labels: []
dependencies:
  - TASK-411.3
parent_task_id: TASK-411
priority: low
type: bug
ordinal: 808000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-411.3: a cell chooses encryption iff a KMS is configured and the tenant has a KEK. For a tenant with none, each opening asks the KMS again (a wrap call that fails NoSuchTenant), as that answer is not cached. A real KMS makes that a round trip on every opening of every plaintext tenant's store.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A host opening several stores of a tenant with no KEK makes at most one KMS call for it within a bounded time: a test counts calls (testing::Counted)
- [ ] #2 A tenant given a KEK later gets encrypted new cells within that bound, documented in doc-16
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
