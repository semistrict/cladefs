---
id: TASK-635
title: >-
  lifecycle.rs module doc says a fork writes nothing of the version's cell, but
  it writes the reference and the checkpoint there
status: To Do
assignee: []
created_date: '2026-11-18 17:04'
labels:
  - bucket
dependencies: []
priority: low
type: docs
ordinal: 941000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/bucket_cell/src/lifecycle.rs opens with "A fork is of a version: its child's head alone, which writes nothing of the version's cell's", but `fork_version_keyed` writes the reference (`refer(bucket, origin, child, owner)`, under the parent's prefix) and `finish_fork` the checkpoint at the version, in the parent's cell. Found by the multi-region research (doc-23), where whose bucket each write lands in matters.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The module doc says what a fork writes into the version's cell and what into the child's
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
