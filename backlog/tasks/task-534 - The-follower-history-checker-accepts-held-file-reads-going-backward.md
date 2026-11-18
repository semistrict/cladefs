---
id: TASK-534
title: The follower history checker accepts held-file reads going backward
status: To Do
assignee: []
created_date: '2026-11-18 12:25'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 924000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of revision 845a6c123, finding 19.

At `crates/follower_history/src/lib.rs:262`, held reads are checked independently after named reads have been checked in sequence. A held object can return the new value then the old value to one reader, and the history passes if each result is individually plausible. This weakens the simulator and guest test oracle. It provides no evidence that a live follower returned stale data. Preserve operation order across both read kinds.

Isolated arm64 macOS validation. Reuse the named-read regression's two writes and commit bounds. At commit 7, submit contents 2 then 1 for the same reader and object. Change only whether the read is named or held. The named sequence returned an error on the second read. The held sequence returned success.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The checker rejects a backwards held-read sequence and a mixed named/held sequence for one reader while accepting valid independent-reader histories and legitimate reads of unlinked objects.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
