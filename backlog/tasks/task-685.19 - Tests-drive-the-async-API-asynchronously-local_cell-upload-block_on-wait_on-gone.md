---
id: TASK-685.19
title: >-
  Tests drive the async API asynchronously: local_cell::upload::{block_on,
  wait_on} gone
status: To Do
assignee: []
created_date: '2026-11-21 01:47'
labels:
  - async
  - testing
dependencies: []
parent_task_id: TASK-685
priority: medium
ordinal: 1123000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Several hundred test call sites (fork_faults, restore_faults, lifecycle, write_budgets, encrypted, cladefsd tests, ...) drive async APIs through upload::wait_on/block_on, a sync twin of the API for tests. Make the tests async (a runtime per test, as #[tokio::test] gives, or the crate's own), then remove the wrappers; the tools' entries (cladefsctl, on_buckets) block at their outermost entry alone.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 scripts/upstream-changes.py --check passes
- [ ] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->
