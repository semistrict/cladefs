---
id: TASK-411.14
title: >-
  fxdax-gc --delete-branch of a sealed branch with an unrecorded fork into
  another cell needs a KMS
status: To Do
assignee: []
created_date: '2026-11-15 00:17'
labels: []
dependencies: []
parent_task_id: TASK-411
priority: low
ordinal: 810000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-411.4 seals the parent's checkpoint at a fork point under the parent's DEK, as a child holding the parent's keys refuses one in the clear. A deletion that records the forks of its branch writes that checkpoint for a fork into another cell whose child has no record yet (its writer died between the fork's acknowledgement and recording it): branch::delete_under takes the host's KMS for it, and without one fails (Error::Kms) rather than write a checkpoint the child refuses. fxdax-gc reads no FXDAX_KMS (doc-16: 'fxdax-gc needs none'), so its --delete-branch of such a branch fails until it has one. fxdax's own deletions (host_store::delete) pass the host's KMS.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 fxdax-gc --delete-branch deletes a sealed branch whose fork into another cell has no record yet, the checkpoint at the fork point sealed under the branch's DEK, given FXDAX_KMS
- [ ] #2 Without a KMS it fails naming the tenant and what it needs, leaving the branch as a deletion stopped partway
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
