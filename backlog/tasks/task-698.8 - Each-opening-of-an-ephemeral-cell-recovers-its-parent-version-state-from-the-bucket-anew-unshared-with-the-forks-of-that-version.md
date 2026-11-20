---
id: TASK-698.8
title: >-
  Ephemeral openings replay the parent version from its nearest checkpoint each
  time: the fork writes no checkpoint at the version
status: To Do
assignee: []
created_date: '2026-11-20 18:09'
updated_date: '2026-11-20 19:49'
labels: []
dependencies: []
parent_task_id: TASK-698
priority: medium
ordinal: 1098000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Corrected 2026-11-20 (the first description, that each opening decodes an unshared copy of the parent state, was filed unchecked and is wrong). upload::open_ephemeral recovers the parent state with recovery::recover_version, whose checkpoints are read through the bucket Decoded registry: a checkpoint decoded once is shared by every recovery while one holds it (TASK-407, TASK-453), and the extent map is persistent. What is not shared: a bucket fork writes the checkpoint at the version (recovery::checkpoint_at in lifecycle::finish_fork), but lifecycle::fork_ephemeral does not. So when no checkpoint sits at the version segment, every ephemeral opening starts from the nearest earlier checkpoint and replays the parent segments up to the version, reading their footers again and building a state of its own that shares most of its map nodes with the checkpoint but not with the other openings.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Ephemeral cells of one version opened at once share one decoded parent state, shown by a test counting decoded checkpoints
- [ ] #2 Memory per ephemeral cell opening measured on the test host against a bucket fork of the same version
- [ ] #3 An ephemeral fork leaves a checkpoint at its version, as a bucket fork does, so its openings start from one shared decoded state
- [ ] #4 A test opens several ephemeral cells of one version whose segment has no checkpoint before the fork and counts one decode and no footer replays per opening
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-20: lifecycle::fork_ephemeral writes the checkpoint at the version (recovery::checkpoint_at, under the version store sealing ring) after the head check; the collector ephemeral test checks it is there. AC #2 (counting decodes per opening) not written yet.
<!-- SECTION:NOTES:END -->
