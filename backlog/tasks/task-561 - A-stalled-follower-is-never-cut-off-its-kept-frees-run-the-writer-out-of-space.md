---
id: TASK-561
title: >-
  A stalled follower is never cut off; its kept frees run the writer out of
  space
status: To Do
assignee: []
created_date: '2026-11-18 15:26'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 952000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), local-core review; verified against the code by independent reviewers. Severity: high.

**Where:** `crates/cladefs/src/host_cell.rs`: `rg -n -F 'Options { max_extent: crate::OpenCell::MAX_EXTENT, ..Options::default() }' crates/cladefs/src/host_cell.rs`

Options::default() sets view_lag and view_kept_bytes to None. cladefs opens every host store with `Options { max_extent, ..Options::default() }` (host_cell::options), and nothing in cladefs sets either limit. So the cut-off in Views::publish never runs (`while let Some(limit) = limits.kept_bytes ...`, and `limits.lag.is_some_and(..)` is always false). A follower view that stops advancing stays the oldest view. KeptExtents::trim keeps every free after its `since`. views_bytes() counts them into kept.for_views(), and State::check_room passes that to Space::check as used space. The writer's used space therefore grows by everything it frees. Once holding + kept_for_views passes the capacity, every begin_write fails with NoSpace, and with it the writes a commit carries. The state.rs Space docs record that a guest filesystem did not survive ENOSPC. doc-8 decision 7, and the TLA spec's FollowerStalled/LagCutOff property, say such a follower is cut off; the production configuration turns that off. A second effect: kept grows without bound, and State::into_bucket walks every kept entry (`self.kept.entries_mut()`) under the state lock on each acknowledged segment, so every acknowledgement gets slower as the follower stays stuck.

**Trigger.** 1. VM W mounts cell C read-write through cladefsd. VM F mounts C as a follower, so LocalCell::view() opens a view at commit e. 2. F stops sending ADVANCE: Firecracker pauses it for a VM snapshot or migration, or its guest hangs. 3. W keeps working, for example database churn: COMMIT, then FREE_MANY of the replaced extents, again and again. Each commit runs publish, then Views::publish with Limits{lag: None, kept_bytes: None}, so F is never cut. trim_kept keeps every free with freed > e, and for_views grows. 4. Once Space::check finds holding.all.bytes + kept_for_views + reserved + grows > capacity, W's WRITEs, and the WRITE_COMMIT writes, fail ENOSPC until F advances or goes away. No error ever reaches F, and nothing cuts it off.

**Why the tests stay green.** tests/views.rs builds its own Options with view_lag or view_kept_bytes set in the cut-off tests. The daemon and guest follower tests use followers that keep advancing. The TLA model checks a cut-off rule the shipped configuration never enables. TASK-92.29 fixed the per-commit cost of a lagging view and noted in its summary, 'It still keeps its frees as used space, because fxdax sets no view_lag or view_kept_bytes', but no task was filed for it.

**Backlog.** Related to TASK-42.2 (Done), which built the cut-off mechanism but set no production limits, and TASK-92.29 (Done), whose summary noted that frees stay kept as used space because no view_lag or view_kept_bytes is set, though no task was filed. doc-8 still lists the limits as an open question.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 With the options every host store opens with (host_cell::options), a follower view that stops advancing is cut off (its operations fail with ESTALE) before what it keeps can make the writer's writes or commits fail with ENOSPC.
- [ ] #2 With such a follower and sustained write-and-free churn past the store's capacity, the writer's WRITEs and WRITE_COMMITs keep succeeding. The bytes kept for views stay within the configured limit, so the work each acknowledged segment does over kept entries under the state lock stays bounded too.
- [ ] #3 A test in crates/cladefs opens a writer store with host_cell::options() and a follower view that never advances. It churns writes and frees of sealed extents past capacity and asserts that the follower is cut off and the writer keeps writing. It fails with NoSpace with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
