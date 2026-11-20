---
id: TASK-698.8
title: >-
  Each opening of an ephemeral cell recovers its parent version state from the
  bucket anew, unshared with the forks of that version
status: To Do
assignee: []
created_date: '2026-11-20 18:09'
labels: []
dependencies: []
parent_task_id: TASK-698
priority: medium
ordinal: 1098000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
upload::open_ephemeral calls recovery::recover_version for every opening, so hundreds of ephemeral cells forked from one base each decode the base checkpoint and hold their own copy of its extent map, where bucket forks share one decoded layer (TASK-453, bucket_cell::Recovered::layer). The at-version store the daemon keeps open for forks holds that layer already (AtVersion.state).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Ephemeral cells of one version opened at once share one decoded parent state, shown by a test counting decoded checkpoints
- [ ] #2 Memory per ephemeral cell opening measured on the test host against a bucket fork of the same version
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
